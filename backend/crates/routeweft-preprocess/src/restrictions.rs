//! Static driving restrictions resolved against directed graph edges.
//! Independently implemented; malformed relations are never silently permitted.
use crate::{
    diagnostics::RestrictionDiagnostic,
    pbf::{Osm, Relation},
    value,
};
use routeweft_dataset::Edge;
use std::collections::BTreeMap;

type Resolved = Option<(String, Vec<Vec<usize>>)>;
pub(crate) fn applies(rel: &Relation) -> bool {
    value(&rel.tags, "type") == "restriction"
        && !value(&rel.tags, "except")
            .split(';')
            .any(|v| matches!(v.trim(), "motorcar" | "motor_vehicle" | "vehicle"))
        && [
            "restriction",
            "restriction:motorcar",
            "restriction:motor_vehicle",
            "restriction:vehicle",
        ]
        .iter()
        .any(|k| rel.tags.contains_key(*k) || rel.tags.contains_key(&format!("{k}:conditional")))
}
pub(crate) fn conditional(rel: &Relation) -> bool {
    [
        "restriction:conditional",
        "restriction:motorcar:conditional",
        "restriction:motor_vehicle:conditional",
        "restriction:vehicle:conditional",
    ]
    .iter()
    .any(|k| rel.tags.contains_key(*k))
}
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve(
    rel: &Relation,
    osm: &Osm,
    mapping: &BTreeMap<i64, usize>,
    by_way: &BTreeMap<i64, Vec<usize>>,
    edges: &[Edge],
    outgoing: &[Vec<usize>],
) -> std::result::Result<Resolved, Box<RestrictionDiagnostic>> {
    let fail = |code, message: &str| Box::new(RestrictionDiagnostic::new(rel, code, message));
    let from: Vec<_> = rel.members.iter().filter(|m| m.role == "from").collect();
    let to: Vec<_> = rel.members.iter().filter(|m| m.role == "to").collect();
    let via: Vec<_> = rel.members.iter().filter(|m| m.role == "via").collect();
    // Relations with no retained roads cannot affect this driving graph. Do not
    // confuse an empty from/to list with proof that the relation is irrelevant.
    if !rel
        .members
        .iter()
        .any(|m| m.kind == 1 && by_way.contains_key(&m.id))
    {
        return Ok(None);
    }
    let rule = [
        "restriction:motorcar",
        "restriction:motor_vehicle",
        "restriction:vehicle",
        "restriction",
    ]
    .iter()
    .find_map(|k| rel.tags.get(*k))
    .ok_or_else(|| {
        fail(
            "MissingRestrictionTag",
            "missing static driving restriction tag",
        )
    })?;
    if !matches!(
        rule.as_str(),
        "no_left_turn"
            | "no_right_turn"
            | "no_straight_on"
            | "no_u_turn"
            | "no_entry"
            | "no_exit"
            | "only_left_turn"
            | "only_right_turn"
            | "only_straight_on"
            | "only_u_turn"
    ) {
        return Err(fail(
            "UnsupportedRestriction",
            "unsupported static restriction value",
        ));
    }
    if from.is_empty() || to.is_empty() {
        return Err(fail(
            "MissingFromTo",
            "restriction needs from and to members",
        ));
    }
    if from.iter().chain(&to).any(|m| m.kind != 1) {
        return Err(fail("InvalidMemberType", "from/to members must be ways"));
    }
    if (from.len() != 1 && rule != "no_entry") || (to.len() != 1 && rule != "no_exit") {
        return Err(fail(
            "UnsupportedMultiplicity",
            "multiple from only for no_entry; multiple to only for no_exit",
        ));
    }
    if via.is_empty() {
        return Err(fail(
            "MissingVia",
            "restriction has no via member; turn location is not inferred",
        ));
    }
    let node_via = via.len() == 1 && via[0].kind == 0;
    if !node_via && !via.iter().all(|m| m.kind == 1) {
        return Err(fail(
            "UnsupportedViaShape",
            "expected one via-node or ordered via-ways",
        ));
    }
    if via.len() > 64 || from.len().saturating_mul(to.len()) > 256 {
        return Err(fail(
            "RestrictionLimitExceeded",
            "restriction member expansion limit exceeded",
        ));
    }
    for m in from.iter().chain(&to).chain(&via) {
        if !(if m.kind == 0 {
            osm.nodes.contains_key(&m.id)
        } else {
            osm.ways.contains_key(&m.id)
        }) {
            return Err(fail(
                "MissingReference",
                "restriction references an absent node or way; check extract closure",
            ));
        }
        if m.kind == 1
            && osm.ways[&m.id]
                .refs
                .iter()
                .any(|id| !osm.nodes.contains_key(id))
        {
            return Err(fail(
                "MissingReference",
                "restriction way references absent nodes; check extract closure",
            ));
        }
    }
    // A way may legitimately be excluded by the conservative profile. Resolve
    // every remaining from/to combination, but never discard an unresolved one.
    let mut sequences = Vec::new();
    let mut work = 0;
    for f in &from {
        for t in &to {
            if !by_way.contains_key(&f.id) {
                continue;
            }
            if !by_way.contains_key(&t.id) {
                if rule.starts_with("only_") {
                    return Err(fail(
                        "UnresolvedRestriction",
                        "only restriction's allowed exit was excluded by the profile",
                    ));
                }
                continue;
            }
            let before = sequences.len();
            if node_via {
                let Some(&node) = mapping.get(&via[0].id) else {
                    return Err(fail(
                        "UnresolvedRestriction",
                        "via-node has no retained road connection",
                    ));
                };
                for &a in &by_way[&f.id] {
                    work += 1;
                    if work > 100_000 {
                        return Err(fail(
                            "RestrictionLimitExceeded",
                            "restriction search budget exceeded",
                        ));
                    }
                    if edges[a].to != node {
                        continue;
                    }
                    for &b in &outgoing[node] {
                        work += 1;
                        if work > 100_000 {
                            return Err(fail(
                                "RestrictionLimitExceeded",
                                "restriction search budget exceeded",
                            ));
                        }
                        if edges[b].way != t.id {
                            continue;
                        }
                        if f.id == t.id
                            && rule.ends_with("u_turn") != (edges[b].to == edges[a].from)
                        {
                            continue;
                        }
                        sequences.push(vec![a, b]);
                        if sequences.len() > 10_000 {
                            return Err(fail(
                                "RestrictionLimitExceeded",
                                "too many restriction paths",
                            ));
                        }
                    }
                }
            } else {
                let ways: Vec<_> = via
                    .iter()
                    .map(|m| m.id)
                    .chain(std::iter::once(t.id))
                    .collect();
                // Repeated via-way IDs are ambiguous in this edge-simple resolver.
                if ways.windows(2).any(|w| w[0] == w[1]) {
                    return Err(fail(
                        "UnsupportedViaShape",
                        "adjacent repeated via/to ways are not supported",
                    ));
                }
                for &start in &by_way[&f.id] {
                    paths(
                        edges,
                        outgoing,
                        &ways,
                        0,
                        vec![start],
                        false,
                        &mut sequences,
                        &mut work,
                    )
                    .map_err(|()| {
                        fail(
                            "RestrictionLimitExceeded",
                            "restriction path search budget exceeded",
                        )
                    })?;
                }
            }
            if sequences.len() == before {
                return Err(fail(
                    "UnresolvedRestriction",
                    "no directed path for retained from/to combination",
                ));
            }
        }
    }
    if sequences.is_empty() {
        Ok(None)
    } else {
        Ok(Some((rule.clone(), sequences)))
    }
}
#[allow(clippy::too_many_arguments)]
fn paths(
    edges: &[Edge],
    outgoing: &[Vec<usize>],
    ways: &[i64],
    index: usize,
    path: Vec<usize>,
    entered: bool,
    out: &mut Vec<Vec<usize>>,
    work: &mut usize,
) -> Result<(), ()> {
    *work += 1;
    if *work > 100_000 || path.len() >= 256 || out.len() > 10_000 {
        return Err(());
    }
    let last = *path.last().ok_or(())?;
    for &next in &outgoing[edges[last].to] {
        *work += 1;
        if *work > 100_000 || out.len() >= 10_000 {
            return Err(());
        }
        if path.contains(&next) || edges[next].to == edges[last].from {
            continue;
        }
        let next_index = if edges[next].way == ways[index] {
            Some(index)
        } else if entered && index + 1 < ways.len() && edges[next].way == ways[index + 1] {
            Some(index + 1)
        } else {
            None
        };
        if let Some(i) = next_index {
            let mut p = path.clone();
            p.push(next);
            if i == ways.len() - 1 {
                out.push(p);
            } else {
                paths(edges, outgoing, ways, i, p, true, out, work)?;
            }
        }
    }
    Ok(())
}
