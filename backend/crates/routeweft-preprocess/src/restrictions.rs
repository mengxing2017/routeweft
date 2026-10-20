//! Static driving restrictions resolved against directed graph edges.
//! Independently implemented; malformed relations are never silently permitted.
use crate::{
    diagnostics::RestrictionDiagnostic,
    pbf::{Osm, Relation},
    value,
};
use routeweft_dataset::Edge;
use std::collections::BTreeMap;

pub(crate) type Resolved = Option<(String, Vec<Vec<usize>>)>;
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
/// Minimal fallible graph access shared by memory and disk restriction resolution.
pub(crate) trait Access {
    fn has_way(&mut self, way: i64) -> crate::Result<bool>;
    fn way_edge(&mut self, way: i64, at: usize) -> crate::Result<Option<usize>>;
    fn edge(&mut self, id: usize) -> crate::Result<TurnEdge>;
    fn outgoing(&mut self, node: usize, at: usize) -> crate::Result<Option<usize>>;
    fn node(&mut self, id: i64) -> crate::Result<Option<usize>>;
    fn reference_state(&mut self, kind: u64, id: i64) -> crate::Result<u8>;
}
#[derive(Clone, Copy)]
pub(crate) struct TurnEdge {
    pub from: usize,
    pub to: usize,
    pub way: i64,
}
impl From<&Edge> for TurnEdge {
    fn from(edge: &Edge) -> Self {
        Self {
            from: edge.from,
            to: edge.to,
            way: edge.way,
        }
    }
}
pub(crate) enum ResolveError {
    Diagnostic(Box<RestrictionDiagnostic>),
    Access(crate::Error),
}
impl From<crate::Error> for ResolveError {
    fn from(error: crate::Error) -> Self {
        Self::Access(error)
    }
}
struct MemoryAccess<'a> {
    osm: &'a Osm,
    mapping: &'a [i64],
    by_way: &'a BTreeMap<i64, Vec<usize>>,
    edges: &'a [Edge],
    outgoing: &'a routeweft_dataset::Adjacency,
}
impl Access for MemoryAccess<'_> {
    fn has_way(&mut self, id: i64) -> crate::Result<bool> {
        Ok(self.by_way.contains_key(&id))
    }
    fn way_edge(&mut self, id: i64, at: usize) -> crate::Result<Option<usize>> {
        Ok(self.by_way.get(&id).and_then(|ids| ids.get(at)).copied())
    }
    fn edge(&mut self, id: usize) -> crate::Result<TurnEdge> {
        Ok((&self.edges[id]).into())
    }
    fn outgoing(&mut self, node: usize, at: usize) -> crate::Result<Option<usize>> {
        Ok(self.outgoing[node].get(at).copied())
    }
    fn node(&mut self, id: i64) -> crate::Result<Option<usize>> {
        Ok(self.mapping.binary_search(&id).ok())
    }
    fn reference_state(&mut self, kind: u64, id: i64) -> crate::Result<u8> {
        Ok(if kind == 0 {
            if self.osm.nodes.contains_key(&id) {
                2
            } else {
                0
            }
        } else {
            self.osm.ways.get(&id).map_or(0, |way| {
                if way.refs.iter().all(|id| self.osm.nodes.contains_key(id)) {
                    2
                } else {
                    1
                }
            })
        })
    }
}
pub(crate) fn resolve(
    rel: &Relation,
    osm: &Osm,
    mapping: &[i64],
    by_way: &BTreeMap<i64, Vec<usize>>,
    edges: &[Edge],
    outgoing: &routeweft_dataset::Adjacency,
) -> std::result::Result<Resolved, Box<RestrictionDiagnostic>> {
    let mut access = MemoryAccess {
        osm,
        mapping,
        by_way,
        edges,
        outgoing,
    };
    match resolve_with(rel, &mut access) {
        Ok(value) => Ok(value),
        Err(ResolveError::Diagnostic(diagnostic)) => Err(diagnostic),
        Err(ResolveError::Access(_)) => unreachable!("memory restriction access is infallible"),
    }
}
pub(crate) fn resolve_with(
    rel: &Relation,
    access: &mut impl Access,
) -> std::result::Result<Resolved, ResolveError> {
    let fail = |code, message: &str| {
        ResolveError::Diagnostic(Box::new(RestrictionDiagnostic::new(rel, code, message)))
    };
    let from: Vec<_> = rel.members.iter().filter(|m| m.role == "from").collect();
    let to: Vec<_> = rel.members.iter().filter(|m| m.role == "to").collect();
    let via: Vec<_> = rel.members.iter().filter(|m| m.role == "via").collect();
    // Relations with no retained roads cannot affect this driving graph. Do not
    // confuse an empty from/to list with proof that the relation is irrelevant.
    let mut relevant = false;
    for member in &rel.members {
        if member.kind == 1 && access.has_way(member.id)? {
            relevant = true;
            break;
        }
    }
    if !relevant {
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
        match access.reference_state(m.kind, m.id)? {
            0 => {
                return Err(fail(
                    "MissingReference",
                    "restriction references an absent node or way; check extract closure",
                ))
            }
            1 => {
                return Err(fail(
                    "MissingReference",
                    "restriction way references absent nodes; check extract closure",
                ))
            }
            _ => (),
        }
    }

    // A way may legitimately be excluded by the conservative profile. Resolve
    // every remaining from/to combination, but never discard an unresolved one.
    let mut sequences = Vec::new();
    let mut work = 0;
    for f in &from {
        for t in &to {
            if !access.has_way(f.id)? {
                continue;
            }
            if !access.has_way(t.id)? {
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
                let Some(node) = access.node(via[0].id)? else {
                    return Err(fail(
                        "UnresolvedRestriction",
                        "via-node has no retained road connection",
                    ));
                };
                let mut from_at = 0;
                while let Some(a) = access.way_edge(f.id, from_at)? {
                    from_at += 1;
                    work += 1;
                    if work > 100_000 {
                        return Err(fail(
                            "RestrictionLimitExceeded",
                            "restriction search budget exceeded",
                        ));
                    }
                    let before_edge = access.edge(a)?;
                    if before_edge.to != node {
                        continue;
                    }
                    let mut next_at = 0;
                    while let Some(b) = access.outgoing(node, next_at)? {
                        next_at += 1;
                        work += 1;
                        if work > 100_000 {
                            return Err(fail(
                                "RestrictionLimitExceeded",
                                "restriction search budget exceeded",
                            ));
                        }
                        let after_edge = access.edge(b)?;
                        if after_edge.way != t.id {
                            continue;
                        }
                        if f.id == t.id
                            && rule.ends_with("u_turn") != (after_edge.to == before_edge.from)
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
                let mut start_at = 0;
                while let Some(start) = access.way_edge(f.id, start_at)? {
                    start_at += 1;
                    paths(
                        access,
                        &ways,
                        0,
                        vec![start],
                        false,
                        &mut sequences,
                        &mut work,
                    )
                    .map_err(|error| match error {
                        WalkError::Access(error) => ResolveError::Access(error),
                        WalkError::Limit => fail(
                            "RestrictionLimitExceeded",
                            "restriction path search budget exceeded",
                        ),
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
    access: &mut impl Access,
    ways: &[i64],
    index: usize,
    path: Vec<usize>,
    entered: bool,
    out: &mut Vec<Vec<usize>>,
    work: &mut usize,
) -> std::result::Result<(), WalkError> {
    *work += 1;
    if *work > 100_000 || path.len() >= 256 || out.len() > 10_000 {
        return Err(WalkError::Limit);
    }
    let last = *path.last().ok_or(WalkError::Limit)?;
    let last_edge = access.edge(last)?;
    let mut at = 0;
    while let Some(next) = access.outgoing(last_edge.to, at)? {
        at += 1;
        *work += 1;
        if *work > 100_000 || out.len() >= 10_000 {
            return Err(WalkError::Limit);
        }
        let next_edge = access.edge(next)?;
        if path.contains(&next) || next_edge.to == last_edge.from {
            continue;
        }
        let next_index = if next_edge.way == ways[index] {
            Some(index)
        } else if entered && index + 1 < ways.len() && next_edge.way == ways[index + 1] {
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
                paths(access, ways, i, p, true, out, work)?;
            }
        }
    }
    Ok(())
}

enum WalkError {
    Limit,
    Access(crate::Error),
}
impl From<crate::Error> for WalkError {
    fn from(error: crate::Error) -> Self {
        Self::Access(error)
    }
}
