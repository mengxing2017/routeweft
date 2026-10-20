//! Offline PBF preprocessing with a deliberately conservative driving profile.
mod diagnostics;
pub mod pbf;
mod restrictions;
pub use diagnostics::{
    BuildOptions, Error, InputReport, InvalidRestrictionPolicy, MemberDiagnostic,
    RestrictionDiagnostic, Result,
};
use pbf::{Osm, Tags};
use routeweft_dataset::{Adjacency, Edge, Error as DataError, Graph, Manifest, Point};
fn invalid(message: impl Into<String>) -> Error {
    routeweft_dataset::invalid(message).into()
}
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
    let (graph, report) = prepare(osm, BuildOptions::default())?;
    Ok((graph, report.diagnostics))
}
fn graph_attempt(osm: &Osm, excluded: &BTreeSet<i64>) -> Result<(Graph, BTreeMap<String, u64>)> {
    let mut stats = BTreeMap::from([
        ("input_nodes".into(), osm.nodes.len() as u64),
        ("input_ways".into(), osm.ways.len() as u64),
        ("input_relations".into(), osm.relations.len() as u64),
    ]);
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
        if restrictions::applies(rel) && restrictions::conditional(rel) {
            for m in &rel.members {
                if m.kind == 1 {
                    conditional_ways.insert(m.id);
                }
            }
            count(&mut stats, "conditional_restrictions_blocked");
        }
    }
    for way in osm.ways.values() {
        if excluded.contains(&way.id) {
            continue;
        }
        let class = value(&way.tags, "highway");
        let Some(default) = default_speed(class) else {
            count(&mut stats, "unsupported_way_class");
            continue;
        };
        let exclusion = if !directional_access(&way.tags, "forward")
            && !directional_access(&way.tags, "backward")
        {
            Some("blocked_way_access")
        } else if value(&way.tags, "area") == "yes" {
            Some("blocked_way_area")
        } else if value(&way.tags, "service") == "emergency_access" {
            Some("blocked_way_emergency_access")
        } else if way.tags.keys().any(|k| {
            [
                "maxheight",
                "maxwidth",
                "maxweight",
                "maxlength",
                "maxaxleload",
            ]
            .iter()
            .any(|base| k == base || k.starts_with(&format!("{base}:")))
        }) {
            Some("blocked_way_dimensions")
        } else if conditional_ways.contains(&way.id) {
            Some("blocked_way_conditional_restriction")
        } else if way.tags.keys().any(|k| k.contains(":conditional")) {
            Some("blocked_way_conditional_tag")
        } else {
            None
        };
        if let Some(reason) = exclusion {
            count(&mut stats, reason);
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
            _ => {
                return Err(invalid(format!(
                    "way {}: unsupported oneway {}",
                    way.id,
                    value(&way.tags, "oneway")
                )))
            }
        };
        let contextual = |e: Error| invalid(format!("way {}: {e}", way.id));
        let base = speed(value(&way.tags, "maxspeed"), default).map_err(contextual)?;
        let forward = speed(value(&way.tags, "maxspeed:forward"), base).map_err(contextual)?;
        let backward = speed(value(&way.tags, "maxspeed:backward"), base).map_err(contextual)?;
        for &node in &way.refs {
            if !osm.nodes.contains_key(&node) {
                return Err(invalid(format!(
                    "way {} references missing node {node}",
                    way.id
                )));
            }
            used.insert(node);
        }
        selected.push((way, direction, forward, backward));
    }
    let mut nodes = Vec::new();
    let mapping: Vec<i64> = used.into_iter().collect();
    for &id in &mapping {
        if nodes.len() >= routeweft_dataset::LoadOptions::default().max_nodes {
            return Err(routeweft_dataset::resource_limit(
                "graph_nodes",
                1_000_000,
                nodes.len() as u64 + 1,
                "graph_build",
            )
            .into());
        }
        nodes.push(osm.nodes[&id].point);
    }
    if let Some(first) = nodes.first() {
        let (mut min_lon, mut max_lon, mut min_lat, mut max_lat) =
            (first.lon, first.lon, first.lat, first.lat);
        for p in &nodes {
            min_lon = min_lon.min(p.lon);
            max_lon = max_lon.max(p.lon);
            min_lat = min_lat.min(p.lat);
            max_lat = max_lat.max(p.lat);
        }
        if max_lon - min_lon > 5. || max_lat - min_lat > 5. {
            return Err(invalid(
                "unsupported geographic extent: city graph must span at most 5 degrees per axis",
            ));
        }
    }
    let mut edges = Vec::new();
    let mut by_way: BTreeMap<i64, Vec<usize>> = BTreeMap::new();
    for (way, direction, forward, backward) in selected {
        for pair in way.refs.windows(2) {
            if blocked.contains(&pair[0]) || blocked.contains(&pair[1]) {
                continue;
            }
            let from = mapping
                .binary_search(&pair[0])
                .map_err(|_| invalid("selected node mapping"))?;
            let to = mapping
                .binary_search(&pair[1])
                .map_err(|_| invalid("selected node mapping"))?;
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
                    return Err(routeweft_dataset::resource_limit(
                        "graph_edges",
                        2_000_000,
                        edges.len() as u64 + 1,
                        "graph_build",
                    )
                    .into());
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
    drop(blocked);
    drop(conditional_ways);
    let outgoing = Adjacency::new(nodes.len(), &edges)?;
    let mut forbidden = BTreeSet::new();
    let mut only: BTreeMap<Vec<usize>, BTreeSet<usize>> = BTreeMap::new();
    let mut failures = Vec::new();
    for rel in &osm.relations {
        if value(&rel.tags, "type") != "restriction" {
            continue;
        }
        if !restrictions::applies(rel) {
            let key = if value(&rel.tags, "except")
                .split(';')
                .any(|v| matches!(v.trim(), "motorcar" | "motor_vehicle" | "vehicle"))
            {
                "excepted_restrictions"
            } else {
                "other_mode_restrictions"
            };
            count(&mut stats, key);
            continue;
        }
        if restrictions::conditional(rel) {
            continue;
        }
        match restrictions::resolve(rel, osm, &mapping, &by_way, &edges, &outgoing) {
            Ok(Some((rule, sequences))) => {
                for sequence in sequences {
                    if rule.starts_with("only_") {
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
            Ok(None) => count(&mut stats, "non_driving_restrictions"),
            Err(diagnostic) => {
                if failures.len() >= 256 {
                    return Err(DataError::LimitExceeded.into());
                }
                failures.push(*diagnostic);
            }
        }
    }
    if !failures.is_empty() {
        return Err(Error {
            source: routeweft_dataset::invalid("invalid driving restrictions"),
            report: Box::new(InputReport {
                nodes: nodes.len(),
                edges: edges.len(),
                diagnostics: stats,
                restriction_diagnostics: failures,
                failure_code: Some("InvalidRestriction".into()),
                ..Default::default()
            }),
        });
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
    drop(mapping);
    drop(by_way);
    drop(outgoing);
    let graph = Graph {
        nodes,
        edges,
        forbidden: forbidden.into_iter().collect(),
    };
    graph.validate(Default::default())?;
    Ok((graph, stats))
}
/// Validate exactly the graph that build would publish, without writing files.
/// BlockWays re-runs resolution after every conservative road exclusion, since
/// removing one road can invalidate another restriction's directed path.
pub fn prepare(osm: &Osm, options: BuildOptions) -> Result<(Graph, InputReport)> {
    let mut relation_ids = BTreeSet::new();
    if osm.relations.iter().any(|r| !relation_ids.insert(r.id)) {
        return Err(invalid("duplicate relation identity"));
    }
    let mut excluded = BTreeSet::new();
    let mut audit = Vec::new();
    loop {
        match graph_attempt(osm, &excluded) {
            Ok((graph, mut stats)) => {
                stats.insert("invalid_restrictions_blocked".into(), audit.len() as u64);
                stats.insert(
                    "invalid_restriction_ways_blocked".into(),
                    excluded.len() as u64,
                );
                let report = InputReport {
                    valid: true,
                    policy: options.invalid_restrictions,
                    nodes: graph.nodes.len(),
                    edges: graph.edges.len(),
                    restrictions: graph.forbidden.len(),
                    source_snapshot: osm.snapshot,
                    diagnostics: stats,
                    restriction_diagnostics: audit,
                    blocked_way_ids: excluded.into_iter().collect(),
                    ..Default::default()
                };
                return Ok((graph, report));
            }
            Err(mut error) => {
                error.report.policy = options.invalid_restrictions;
                error.report.source_snapshot = osm.snapshot;
                if options.invalid_restrictions == InvalidRestrictionPolicy::Reject
                    || error.report.restriction_diagnostics.is_empty()
                {
                    audit.append(&mut error.report.restriction_diagnostics);
                    error.report.restriction_diagnostics = audit;
                    error.report.blocked_way_ids = excluded.into_iter().collect();
                    return Err(error);
                }
                if audit.len() + error.report.restriction_diagnostics.len() > 256 {
                    return Err(DataError::LimitExceeded.into());
                }
                let before = excluded.len();
                for d in &mut error.report.restriction_diagnostics {
                    // Only a complete from/to way can safely stop the affected
                    // movement. A relation-only or unknown member is not guessed.
                    let rel = osm
                        .relations
                        .iter()
                        .find(|r| r.id == d.relation_id)
                        .ok_or_else(|| invalid("relation identity"))?;
                    if !rel.members.iter().any(|m| {
                        m.kind == 1
                            && matches!(m.role.as_str(), "from" | "to")
                            && osm.ways.contains_key(&m.id)
                    }) {
                        return Err(error);
                    }
                    for m in &rel.members {
                        if m.kind == 1 && osm.ways.contains_key(&m.id) {
                            excluded.insert(m.id);
                        }
                    }
                    d.action = "blocked_ways".into();
                }
                audit.append(&mut error.report.restriction_diagnostics);
                if before == excluded.len() {
                    return Err(invalid("restriction blocking made no progress"));
                }
            }
        }
    }
}
fn input(input: &Path, options: BuildOptions) -> Result<(Graph, InputReport)> {
    let input_error = |source| Error {
        source,
        report: Box::new(InputReport {
            policy: options.invalid_restrictions,
            failure_code: Some("InputReadError".into()),
            ..Default::default()
        }),
    };
    let input_bytes = std::fs::metadata(input)
        .map_err(|e| input_error(DataError::Io(e)))?
        .len();
    if input_bytes > 512 * 1024 * 1024 {
        return Err(input_error(routeweft_dataset::resource_limit(
            "input_bytes",
            512 * 1024 * 1024,
            input_bytes,
            "input",
        )));
    }
    let source = routeweft_dataset::hash_file(input).map_err(input_error)?;
    let result = (|| {
        let osm = pbf::read_for_driving(input).map_err(|e| {
            let mut error = Error::from(e);
            error.report.failure_code = Some("PbfReadError".into());
            error
        })?;
        if routeweft_dataset::hash_file(input)? != source {
            return Err(invalid("input changed while reading"));
        }
        prepare(&osm, options)
    })();
    match result {
        Ok((graph, mut report)) => {
            report.source_sha256 = source;
            Ok((graph, report))
        }
        Err(mut error) => {
            error.report.policy = options.invalid_restrictions;
            error.report.source_sha256 = source;
            if error.report.failure_code.is_none() {
                error.report.failure_code = Some("GraphValidationError".into());
            }
            Err(error)
        }
    }
}
pub fn preflight(input_path: &Path, options: BuildOptions) -> Result<InputReport> {
    input(input_path, options).map(|(_, report)| report)
}
pub fn build_with_options(
    input_path: &Path,
    output: &Path,
    options: BuildOptions,
) -> Result<(Manifest, InputReport)> {
    if input_path == output {
        return Err(invalid("input and output must differ"));
    }
    let (graph, report) = input(input_path, options)?;
    let manifest = routeweft_dataset::write(
        output,
        &graph,
        report.source_sha256.clone(),
        report.source_snapshot,
        report.diagnostics.clone(),
    )
    .map_err(|source| {
        let mut failed_report = report.clone();
        failed_report.valid = false;
        failed_report.failure_code = Some("DatasetWriteError".into());
        Error {
            source,
            report: Box::new(failed_report),
        }
    })?;
    Ok((manifest, report))
}
pub fn build(input_path: &Path, output: &Path) -> Result<Manifest> {
    build_with_options(input_path, output, BuildOptions::default()).map(|(m, _)| m)
}
