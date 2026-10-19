//! Offline PBF preprocessing with a deliberately conservative driving profile.
pub mod pbf;
use pbf::{Osm, Tags};
use routeweft_dataset::{invalid, Edge, Error, Graph, Manifest, Point, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub fn distance(a: Point, b: Point) -> f64 {
    let dlat = (b.lat - a.lat).to_radians();
    let dlon = (b.lon - a.lon).to_radians();
    let h = (dlat / 2.0).sin().powi(2)
        + a.lat.to_radians().cos() * b.lat.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    6_371_008.8 * 2.0 * h.clamp(0.0, 1.0).sqrt().asin()
}
fn value<'a>(tags: &'a Tags, key: &str) -> &'a str {
    tags.get(key).map(String::as_str).unwrap_or("")
}
fn access(tags: &Tags) -> bool {
    for key in ["motorcar", "motor_vehicle", "vehicle", "access"] {
        match value(tags, key) {
            "" => {}
            "yes" | "permissive" | "designated" => return true,
            _ => return false,
        }
    }
    true
}
fn directional_access(tags: &Tags, direction: &str) -> bool {
    for key in ["motorcar", "motor_vehicle", "vehicle", "access"] {
        let directional = format!("{key}:{direction}");
        let access = tags.get(&directional).or_else(|| tags.get(key));
        if let Some(access) = access {
            return matches!(access.as_str(), "yes" | "permissive" | "designated");
        }
    }
    true
}
fn speed(text: &str, default: f64) -> Result<f64> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(default);
    }
    let (text, factor) = if let Some(v) = trimmed.strip_suffix("mph") {
        (v.trim(), 1.609344)
    } else if let Some(v) = trimmed.strip_suffix("km/h") {
        (v.trim(), 1.0)
    } else {
        (trimmed, 1.0)
    };
    let n = text
        .parse::<f64>()
        .map_err(|_| invalid(format!("unsupported maxspeed: {trimmed}")))?
        * factor;
    if !n.is_finite() || n <= 0.0 || n > 300.0 {
        return Err(invalid("maxspeed range"));
    }
    Ok(n.min(default))
}
fn default_speed(class: &str) -> Option<f64> {
    Some(match class {
        "motorway" => 110.,
        "motorway_link" => 50.,
        "trunk" => 90.,
        "trunk_link" => 45.,
        "primary" => 70.,
        "primary_link" => 40.,
        "secondary" => 60.,
        "secondary_link" => 35.,
        "tertiary" => 50.,
        "tertiary_link" => 30.,
        "unclassified" => 40.,
        "residential" => 30.,
        "living_street" => 10.,
        "service" => 20.,
        _ => return None,
    })
}
fn count(stats: &mut BTreeMap<String, u64>, key: &str) {
    *stats.entry(key.into()).or_default() += 1;
}
/// Build with deterministic OSM-ID and way/member order. Every retained adjacent
/// node pair becomes an edge; no geometric intersections are invented.
pub fn graph(osm: &Osm) -> Result<(Graph, BTreeMap<String, u64>)> {
    let mut stats = BTreeMap::new();
    let mut selected = Vec::new();
    let mut used = BTreeSet::new();
    let mut blocked = BTreeSet::new();
    for (&id, node) in &osm.nodes {
        if !access(&node.tags)
            || node.tags.keys().any(|k| k.contains(":conditional"))
            || (!value(&node.tags, "barrier").is_empty()
                && !matches!(
                    value(&node.tags, "barrier"),
                    "no" | "entrance" | "cattle_grid" | "toll_booth" | "border_control"
                ))
        {
            blocked.insert(id);
            count(&mut stats, "blocked_nodes");
        }
    }
    // Conditional restriction semantics are not interpreted: conservatively omit
    // all involved ways instead of silently permitting an illegal movement.
    let mut conditional_ways = BTreeSet::new();
    for rel in &osm.relations {
        if value(&rel.tags, "type") == "restriction"
            && rel.tags.keys().any(|k| k.contains("conditional"))
        {
            for m in &rel.members {
                if m.kind == 1 {
                    conditional_ways.insert(m.id);
                }
            }
            count(&mut stats, "conditional_restrictions_blocked");
        }
    }
    for way in osm.ways.values() {
        let class = value(&way.tags, "highway");
        let Some(default) = default_speed(class) else {
            count(&mut stats, "unsupported_way_class");
            continue;
        };
        if (!directional_access(&way.tags, "forward") && !directional_access(&way.tags, "backward"))
            || value(&way.tags, "area") == "yes"
            || value(&way.tags, "service") == "emergency_access"
            || way.tags.keys().any(|k| {
                [
                    "maxheight",
                    "maxwidth",
                    "maxweight",
                    "maxlength",
                    "maxaxleload",
                ]
                .iter()
                .any(|base| k == base || k.starts_with(&format!("{base}:")))
            })
            || conditional_ways.contains(&way.id)
            || way.tags.keys().any(|k| k.contains(":conditional"))
        {
            count(&mut stats, "access_or_conditional_ways_blocked");
            continue;
        }
        if !value(&way.tags, "reversible").is_empty()
            || matches!(value(&way.tags, "oneway"), "reversible" | "alternating")
        {
            count(&mut stats, "reversible_ways_blocked");
            continue;
        }
        let direction = match value(&way.tags, "oneway") {
            "yes" | "1" | "true" => 1,
            "-1" => -1,
            "no" | "0" | "false" => 0,
            "" => {
                if value(&way.tags, "junction") == "roundabout" || class == "motorway" {
                    1
                } else {
                    0
                }
            }
            _ => return Err(invalid("unsupported oneway")),
        };
        let base = speed(value(&way.tags, "maxspeed"), default)?;
        let forward = speed(value(&way.tags, "maxspeed:forward"), base)?;
        let backward = speed(value(&way.tags, "maxspeed:backward"), base)?;
        for &node in &way.refs {
            if !osm.nodes.contains_key(&node) {
                return Err(invalid("way references missing node"));
            }
            used.insert(node);
        }
        selected.push((way, direction, forward, backward));
    }
    let mut nodes = Vec::new();
    let mut mapping = BTreeMap::new();
    for id in used {
        mapping.insert(id, nodes.len());
        nodes.push(osm.nodes[&id].point);
    }
    let mut edges = Vec::new();
    let mut by_way: BTreeMap<i64, Vec<usize>> = BTreeMap::new();
    for (way, direction, forward, backward) in selected {
        for pair in way.refs.windows(2) {
            if blocked.contains(&pair[0]) || blocked.contains(&pair[1]) {
                continue;
            }
            let from = mapping[&pair[0]];
            let to = mapping[&pair[1]];
            let a = nodes[from];
            let b = nodes[to];
            if (a.lon - b.lon).abs() > 180. || a.lat.abs() > 85. || b.lat.abs() > 85. {
                return Err(invalid("unsupported geographic extent"));
            }
            let length = (distance(a, b) * 1000.).round() as u64;
            if length == 0 {
                count(&mut stats, "zero_length_segments_omitted");
                continue;
            }
            for (from, to, allowed, speed) in [
                (
                    from,
                    to,
                    direction >= 0 && directional_access(&way.tags, "forward"),
                    forward,
                ),
                (
                    to,
                    from,
                    direction <= 0 && directional_access(&way.tags, "backward"),
                    backward,
                ),
            ] {
                if !allowed {
                    continue;
                }
                if edges.len() >= 2_000_000 {
                    return Err(Error::LimitExceeded);
                }
                by_way.entry(way.id).or_default().push(edges.len());
                edges.push(Edge {
                    from,
                    to,
                    way: way.id,
                    distance_mm: length,
                    duration_ms: ((length as f64 * 3.6 / speed).round() as u64).max(1),
                    name: value(&way.tags, "name").into(),
                    class: value(&way.tags, "highway").into(),
                    roundabout: value(&way.tags, "junction") == "roundabout",
                });
            }
        }
    }
    if nodes.is_empty() || edges.is_empty() {
        return Err(invalid("no driving roads"));
    }
    let mut outgoing = vec![Vec::new(); nodes.len()];
    for (id, e) in edges.iter().enumerate() {
        outgoing[e.from].push(id);
    }
    let mut forbidden = BTreeSet::new();
    let mut only: BTreeMap<Vec<usize>, BTreeSet<usize>> = BTreeMap::new();
    for rel in &osm.relations {
        if value(&rel.tags, "type") != "restriction" {
            continue;
        }
        if value(&rel.tags, "except")
            .split(';')
            .any(|v| matches!(v.trim(), "motorcar" | "motor_vehicle" | "vehicle"))
        {
            count(&mut stats, "excepted_restrictions");
            continue;
        }
        if rel.tags.keys().any(|k| k.contains("conditional")) {
            continue;
        }
        let rule = rel
            .tags
            .get("restriction:motorcar")
            .or_else(|| rel.tags.get("restriction:motor_vehicle"))
            .or_else(|| rel.tags.get("restriction:vehicle"))
            .or_else(|| rel.tags.get("restriction"));
        let Some(rule) = rule else {
            count(&mut stats, "other_mode_restrictions");
            continue;
        };
        let from: Vec<_> = rel
            .members
            .iter()
            .filter(|m| m.role == "from" && m.kind == 1)
            .collect();
        let to: Vec<_> = rel
            .members
            .iter()
            .filter(|m| m.role == "to" && m.kind == 1)
            .collect();
        if from.iter().all(|m| !by_way.contains_key(&m.id))
            || to.iter().all(|m| !by_way.contains_key(&m.id))
        {
            count(&mut stats, "non_driving_restrictions");
            continue;
        }
        if from.len() != 1 || to.len() != 1 {
            return Err(invalid("restriction needs one from/to"));
        }
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
            return Err(invalid(format!("unsupported static restriction {rule}")));
        }
        let via: Vec<_> = rel.members.iter().filter(|m| m.role == "via").collect();
        let mut sequences = Vec::new();
        if via.len() == 1 && via[0].kind == 0 {
            let Some(&node) = mapping.get(&via[0].id) else {
                return Err(invalid("missing via node"));
            };
            for &a in by_way.get(&from[0].id).into_iter().flatten() {
                if edges[a].to != node {
                    continue;
                }
                for &b in &outgoing[node] {
                    if edges[b].way != to[0].id {
                        continue;
                    }
                    if from[0].id == to[0].id {
                        let reverse = edges[b].to == edges[a].from;
                        if rule.ends_with("u_turn") != reverse {
                            continue;
                        }
                    }
                    sequences.push(vec![a, b]);
                }
            }
        } else if !via.is_empty() && via.iter().all(|m| m.kind == 1) {
            let ways: Vec<i64> = via
                .iter()
                .map(|m| m.id)
                .chain(std::iter::once(to[0].id))
                .collect();
            let mut work = 0;
            for &start in by_way.get(&from[0].id).into_iter().flatten() {
                restriction_paths(
                    &edges,
                    &outgoing,
                    &ways,
                    0,
                    vec![start],
                    false,
                    &mut sequences,
                    &mut work,
                )?;
            }
        } else {
            return Err(invalid("unsupported restriction via members"));
        }
        if sequences.is_empty() {
            return Err(invalid("unresolved driving restriction"));
        }
        for sequence in sequences {
            if rule.starts_with("only_") {
                // An only-via-way rule constrains every exit along its prefix,
                // not merely the final transition to the to-way.
                for split in 1..sequence.len() {
                    only.entry(sequence[..split].to_vec())
                        .or_default()
                        .insert(sequence[split]);
                }
            } else {
                forbidden.insert(sequence);
            }
        }
        count(&mut stats, "resolved_restrictions");
    }
    // Union allowed exits for identical only-prefixes before generating exclusions.
    for (prefix, allowed) in only {
        let last = *prefix.last().ok_or_else(|| invalid("only prefix"))?;
        for &next in &outgoing[edges[last].to] {
            if !allowed.contains(&next) {
                let mut seq = prefix.clone();
                seq.push(next);
                forbidden.insert(seq);
            }
        }
    }
    let graph = Graph {
        nodes,
        edges,
        forbidden: forbidden.into_iter().collect(),
    };
    graph.validate(Default::default())?;
    Ok((graph, stats))
}
#[allow(clippy::too_many_arguments)]
fn restriction_paths(
    edges: &[Edge],
    outgoing: &[Vec<usize>],
    ways: &[i64],
    index: usize,
    path: Vec<usize>,
    entered: bool,
    out: &mut Vec<Vec<usize>>,
    work: &mut usize,
) -> Result<()> {
    *work += 1;
    if *work > 100_000 || path.len() > 256 || out.len() > 10_000 {
        return Err(Error::LimitExceeded);
    }
    let last = *path.last().ok_or_else(|| invalid("restriction path"))?;
    for &next in &outgoing[edges[last].to] {
        if path.contains(&next) || edges[next].to == edges[last].from {
            continue;
        }
        if edges[next].way == ways[index] {
            let mut p = path.clone();
            p.push(next);
            if index == ways.len() - 1 {
                out.push(p);
            } else {
                restriction_paths(edges, outgoing, ways, index, p, true, out, work)?;
            }
        }
        if entered && index + 1 < ways.len() && edges[next].way == ways[index + 1] {
            let mut p = path.clone();
            p.push(next);
            if index + 1 == ways.len() - 1 {
                out.push(p);
            } else {
                restriction_paths(edges, outgoing, ways, index + 1, p, true, out, work)?;
            }
        }
    }
    Ok(())
}
pub fn build(input: &Path, output: &Path) -> Result<Manifest> {
    if input == output {
        return Err(invalid("input and output must differ"));
    }
    if std::fs::metadata(input)?.len() > 512 * 1024 * 1024 {
        return Err(Error::LimitExceeded);
    }
    let source = routeweft_dataset::hash_file(input)?;
    let osm = pbf::read(input)?;
    if routeweft_dataset::hash_file(input)? != source {
        return Err(invalid("input changed while building"));
    }
    let (graph, stats) = graph(&osm)?;
    routeweft_dataset::write(output, &graph, source, osm.snapshot, stats)
}
