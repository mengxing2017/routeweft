//! Offline PBF preprocessing with a deliberately conservative driving profile.
mod audit;
mod diagnostics;
pub use audit::{RestrictionAuditLimits, RestrictionAuditSummary};
mod maxspeed;
pub mod pbf;
mod quarantine;
mod restrictions;
pub mod staging;
pub use diagnostics::{
    BlockWaysFailure, BuildOptions, Error, InputReport, InvalidMaxspeedPolicy,
    InvalidRestrictionPolicy, MaxspeedDiagnostic, MemberDiagnostic, RestrictionDiagnostic, Result,
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
fn segment_length(a: Point, b: Point) -> Result<u64> {
    if (a.lon - b.lon).abs() > 180. || a.lat.abs() > 85. || b.lat.abs() > 85. {
        return Err(invalid("unsupported geographic extent"));
    }
    Ok((distance(a, b) * 1000.).round() as u64)
}
fn edge_duration(length: u64, speed: f64) -> u64 {
    ((length as f64 * 3.6 / speed).round() as u64).max(1)
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
// Include every way member, even an unknown role: resolve uses all members to
// decide relevance before diagnosing malformed restriction shapes. Empty entries
// are removed after edge construction, so an excluded/missing way is not retained.
fn restriction_way_index(osm: &Osm) -> BTreeMap<i64, Vec<usize>> {
    let mut index = BTreeMap::new();
    let mut relations = osm
        .relations
        .iter()
        .filter(|rel| restrictions::applies(rel) && !restrictions::conditional(rel))
        .peekable();
    if relations.peek().is_none() {
        return index;
    }
    // Decoder-created maps have matching identities. Preserve the borrowed API's
    // existing diagnostics even for manually assembled maps with mismatched keys.
    let identities_match = osm.ways.iter().all(|(id, way)| *id == way.id);
    for rel in relations {
        for member in rel.members.iter().filter(|member| {
            member.kind == 1 && (!identities_match || osm.ways.contains_key(&member.id))
        }) {
            index.entry(member.id).or_insert_with(Vec::new);
        }
    }
    index
}
#[derive(Clone, Copy)]
struct WaySelection {
    direction: i32,
    forward: f64,
    backward: f64,
}
fn select_way(
    way: &pbf::Way,
    conditional: bool,
    stats: &mut BTreeMap<String, u64>,
    policy: InvalidMaxspeedPolicy,
    audit: &mut Vec<MaxspeedDiagnostic>,
) -> Result<Option<WaySelection>> {
    if policy == InvalidMaxspeedPolicy::UseDefault {
        stats.insert("maxspeed_policy_use_default".into(), 1);
    }
    let class = value(&way.tags, "highway");
    let Some(default) = default_speed(class) else {
        count(stats, "unsupported_way_class");
        return Ok(None);
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
    } else if conditional {
        Some("blocked_way_conditional_restriction")
    } else if way.tags.keys().any(|k| k.contains(":conditional")) {
        Some("blocked_way_conditional_tag")
    } else {
        None
    };
    if let Some(reason) = exclusion {
        count(stats, reason);
        count(stats, "access_or_conditional_ways_blocked");
        return Ok(None);
    }
    if !value(&way.tags, "reversible").is_empty()
        || matches!(value(&way.tags, "oneway"), "reversible" | "alternating")
    {
        count(stats, "reversible_ways_blocked");
        return Ok(None);
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
    let mut resolve =
        |field, fallback| maxspeed::resolve(way, field, fallback, policy, audit, stats);
    let base = resolve("maxspeed", default)?;
    let forward = resolve("maxspeed:forward", base)?;
    let backward = resolve("maxspeed:backward", base)?;
    Ok(Some(WaySelection {
        direction,
        forward,
        backward,
    }))
}
fn blocked_node(tags: &Tags) -> bool {
    !access(tags)
        || tags.keys().any(|k| k.contains(":conditional"))
        || (!value(tags, "barrier").is_empty()
            && !matches!(
                value(tags, "barrier"),
                "no" | "entrance" | "cattle_grid" | "toll_booth" | "border_control"
            ))
}
fn graph_attempt(
    osm: &Osm,
    excluded: &BTreeSet<i64>,
    policy: InvalidMaxspeedPolicy,
    speed_audit: &mut Vec<MaxspeedDiagnostic>,
) -> Result<(Graph, BTreeMap<String, u64>)> {
    let mut audit = audit::AuditCollector::new(Default::default(), "memory_restrictions")?;
    audit.begin(1);
    graph_attempt_with_audit(osm, excluded, policy, speed_audit, &mut audit).map_err(|mut error| {
        audit.attach(&mut error.report);
        error
    })
}
fn graph_attempt_with_audit(
    osm: &Osm,
    excluded: &BTreeSet<i64>,
    policy: InvalidMaxspeedPolicy,
    speed_audit: &mut Vec<MaxspeedDiagnostic>,
    audit: &mut audit::AuditCollector,
) -> Result<(Graph, BTreeMap<String, u64>)> {
    let mut stats = BTreeMap::from([
        ("input_nodes".into(), osm.nodes.len() as u64),
        ("input_ways".into(), osm.ways.len() as u64),
        ("input_relations".into(), osm.relations.len() as u64),
    ]);
    let mut selected = Vec::new();
    let mut used = BTreeSet::new();
    let mut blocked = BTreeSet::new();
    for (&id, node) in &osm.nodes {
        if blocked_node(&node.tags) {
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
        let Some(WaySelection {
            direction,
            forward,
            backward,
        }) = select_way(
            way,
            conditional_ways.contains(&way.id),
            &mut stats,
            policy,
            speed_audit,
        )?
        else {
            continue;
        };
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
    let mut by_way = restriction_way_index(osm);
    for (way, direction, forward, backward) in selected {
        let mut restriction_edges = by_way.get_mut(&way.id);
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
            let length = segment_length(a, b)?;
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
                if let Some(ids) = &mut restriction_edges {
                    ids.push(edges.len());
                }
                edges.push(Edge {
                    from,
                    to,
                    way: way.id,
                    distance_mm: length,
                    duration_ms: edge_duration(length, speed),
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
    by_way.retain(|_, ids| !ids.is_empty());
    // With no retained static restriction members, every resolve call would
    // return None before accessing adjacency. Do not allocate that index.
    let outgoing = (!by_way.is_empty())
        .then(|| Adjacency::new(nodes.len(), &edges))
        .transpose()?;
    let mut forbidden = BTreeSet::new();
    let mut only: BTreeMap<Vec<usize>, BTreeSet<usize>> = BTreeMap::new();
    let start = audit.records.len();
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
        let resolved = outgoing.as_ref().map_or(Ok(None), |outgoing| {
            restrictions::resolve(rel, osm, &mapping, &by_way, &edges, outgoing)
        });
        match resolved {
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
                if let Err(mut error) = audit.push(*diagnostic) {
                    error.report.nodes = nodes.len();
                    error.report.edges = edges.len();
                    error.report.diagnostics = stats;
                    return Err(error);
                }
            }
        }
    }
    audit.complete();
    if audit.records.len() > start {
        return Err(Error {
            source: routeweft_dataset::invalid("invalid driving restrictions"),
            report: Box::new(InputReport {
                nodes: nodes.len(),
                edges: edges.len(),
                diagnostics: stats,
                failure_code: Some("InvalidRestriction".into()),
                ..Default::default()
            }),
        });
    }
    // Union allowed exits for identical only-prefixes before generating exclusions.
    for (prefix, allowed) in only {
        let last = *prefix.last().ok_or_else(|| invalid("only prefix"))?;
        let outgoing = outgoing
            .as_ref()
            .ok_or_else(|| invalid("only restriction without adjacency"))?;
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
    let mut audit = audit::AuditCollector::new(options.restriction_audit, "memory_restrictions")?;
    let mut relation_ids = BTreeSet::new();
    if osm.relations.iter().any(|r| !relation_ids.insert(r.id)) {
        return Err(invalid("duplicate relation identity"));
    }
    let mut excluded = BTreeSet::new();
    let mut applied = Vec::new();
    let mut attempt = 0;
    loop {
        attempt += 1;
        audit.begin(attempt);
        let start = audit.records.len();
        let mut speed_audit = Vec::new();
        match graph_attempt_with_audit(
            osm,
            &excluded,
            options.invalid_maxspeed,
            &mut speed_audit,
            &mut audit,
        ) {
            Ok((graph, mut stats)) => {
                stats.insert(
                    "invalid_restrictions_blocked".into(),
                    audit.records.len() as u64,
                );
                stats.insert(
                    "invalid_restriction_ways_blocked".into(),
                    excluded.len() as u64,
                );
                if options.invalid_restrictions == InvalidRestrictionPolicy::QuarantineWays {
                    stats.insert(quarantine::MARKER.into(), 1);
                }
                let mut report = InputReport {
                    valid: true,
                    policy: options.invalid_restrictions,
                    maxspeed_policy: options.invalid_maxspeed,
                    maxspeed_diagnostics: speed_audit,
                    nodes: graph.nodes.len(),
                    edges: graph.edges.len(),
                    restrictions: graph.forbidden.len(),
                    source_snapshot: osm.snapshot,
                    diagnostics: stats,
                    planned_blocked_way_ids: excluded.iter().copied().collect(),
                    applied_blocked_way_ids: excluded.iter().copied().collect(),
                    blocked_way_ids: excluded.into_iter().collect(),
                    ..Default::default()
                };
                audit.attach(&mut report);
                return Ok((graph, report));
            }
            Err(mut error) => {
                if error.report.nodes > 0 && error.report.edges > 0 {
                    applied = excluded.iter().copied().collect();
                }
                let result = (|| -> Result<()> {
                    if options.invalid_restrictions == InvalidRestrictionPolicy::Reject
                        || !audit.can_block(&error)
                    {
                        return Ok(());
                    }
                    if attempt >= options.restriction_audit.max_blockways_attempts {
                        error.source = audit.attempt_limit().source;
                        error.report.failure_code = Some("BlockWaysAttemptLimitExceeded".into());
                        return Ok(());
                    }
                    let references =
                        audit.records[start..].iter().try_fold(0usize, |count, d| {
                            let rel = osm
                                .relations
                                .iter()
                                .find(|r| r.id == d.relation_id)
                                .ok_or_else(|| invalid("relation identity"))?;
                            Ok::<_, Error>(
                                count.saturating_add(
                                    rel.members.iter().filter(|m| m.kind == 1).count(),
                                ),
                            )
                        })?;
                    if references > 65_536 {
                        return Err(routeweft_dataset::resource_limit(
                            "blockways_member_references",
                            65_536,
                            references as u64,
                            "memory_restrictions",
                        )
                        .into());
                    }
                    let before = excluded.len();
                    for d in &mut audit.records[start..] {
                        let rel = osm
                            .relations
                            .iter()
                            .find(|r| r.id == d.relation_id)
                            .ok_or_else(|| invalid("relation identity"))?;
                        let present = rel
                            .members
                            .iter()
                            .filter(|m| m.kind == 1 && osm.ways.contains_key(&m.id))
                            .map(|m| m.id)
                            .collect();
                        if let Some(reason) = quarantine::failure(
                            options.invalid_restrictions,
                            rel,
                            &d.code,
                            &present,
                        ) {
                            error.report.blockways_failure = Some(BlockWaysFailure::new(d, reason));
                            return Ok(());
                        }
                        for m in &rel.members {
                            if m.kind == 1
                                && osm.ways.contains_key(&m.id)
                                && !excluded.contains(&m.id)
                            {
                                if excluded.len() == 32_768 {
                                    return Err(routeweft_dataset::resource_limit(
                                        "blockways_blocked_ways",
                                        32_768,
                                        excluded.len() as u64 + 1,
                                        "memory_restrictions",
                                    )
                                    .into());
                                }
                                excluded.insert(m.id);
                            }
                        }
                        d.action = "blocked_ways".into();
                    }
                    if before == excluded.len() {
                        error.report.blockways_failure = audit.records[start..]
                            .first()
                            .map(|d| BlockWaysFailure::new(d, "NoNewExclusions"));
                        return Ok(());
                    }
                    audit.refresh()?;
                    Ok(())
                })();
                if let Err(other) = result {
                    error.source = other.source;
                    error.report.failure_code = other.report.failure_code;
                } else if error.report.blockways_failure.is_none()
                    && audit.records[start..]
                        .iter()
                        .all(|d| d.action == "blocked_ways")
                    && !audit.records[start..].is_empty()
                {
                    continue;
                }
                error.report.policy = options.invalid_restrictions;
                error.report.maxspeed_policy = options.invalid_maxspeed;
                error.report.maxspeed_diagnostics = speed_audit;
                error.report.source_snapshot = osm.snapshot;
                error.report.planned_blocked_way_ids = excluded.iter().copied().collect();
                error.report.applied_blocked_way_ids = applied;
                error.report.blocked_way_ids = excluded.into_iter().collect();
                audit.refresh()?;
                audit.attach(&mut error.report);
                return Err(error);
            }
        }
    }
}
fn input(input: &Path, options: BuildOptions) -> Result<(Graph, InputReport)> {
    options.restriction_audit.validate()?;
    let input_error = |source| Error {
        source,
        report: Box::new(InputReport {
            policy: options.invalid_restrictions,
            maxspeed_policy: options.invalid_maxspeed,
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
            error.report.maxspeed_policy = options.invalid_maxspeed;
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
    build_with_format(
        input_path,
        output,
        options,
        routeweft_dataset::DatasetFormat::JsonV1,
    )
}
/// Build using an explicit storage format. The driving profile is unchanged.
pub fn build_with_format(
    input_path: &Path,
    output: &Path,
    options: BuildOptions,
    format: routeweft_dataset::DatasetFormat,
) -> Result<(Manifest, InputReport)> {
    if input_path == output {
        return Err(invalid("input and output must differ"));
    }
    let (graph, report) = input(input_path, options)?;
    let manifest = routeweft_dataset::write_with_format(
        output,
        &graph,
        report.source_sha256.clone(),
        report.source_snapshot,
        report.diagnostics.clone(),
        format,
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

#[cfg(test)]
mod restriction_index_tests {
    use super::*;
    use pbf::{Member, Node, Relation, Way};

    #[test]
    fn sparse_index_matches_full_index_resolution_including_malformed_members() {
        for case in [
            "static",
            "miskeyed",
            "missing",
            "unknown_role",
            "excluded",
            "excluded_only",
            "unsupported",
            "conditional",
            "excepted",
            "other_mode",
            "nonrestriction",
        ] {
            let mut osm = Osm::default();
            for id in 1..=5 {
                osm.nodes.insert(
                    id,
                    Node {
                        id,
                        point: Point {
                            lon: id as f64 * 0.001,
                            lat: 0.,
                        },
                        tags: Tags::new(),
                    },
                );
            }
            for (id, refs, highway) in [
                (10, vec![1, 2], "residential"),
                (20, vec![2, 3], "residential"),
                (30, vec![4, 5], "footway"),
                (40, vec![2, 4], "residential"),
            ] {
                osm.ways.insert(
                    id,
                    Way {
                        id,
                        refs,
                        tags: Tags::from([("highway".into(), highway.into())]),
                    },
                );
            }
            let (g, _) = graph(&osm).unwrap();
            let mut rel = Relation {
                id: 100,
                tags: Tags::from([
                    ("type".into(), "restriction".into()),
                    ("restriction".into(), "only_straight_on".into()),
                ]),
                members: [(10, 1, "from"), (2, 0, "via"), (20, 1, "to")]
                    .into_iter()
                    .map(|(id, kind, role)| Member {
                        id,
                        kind,
                        role: role.into(),
                    })
                    .collect(),
            };
            match case {
                "missing" => rel.members[0].id = i64::MAX,
                "unknown_role" => {
                    rel.members = vec![Member {
                        id: 40,
                        kind: 1,
                        role: "unexpected".into(),
                    }]
                }
                "excluded" => rel.members[0].id = 30,
                "excluded_only" => {
                    rel.members[0].id = 30;
                    rel.members[2].id = 30;
                }
                "unsupported" => {
                    rel.tags.insert("restriction".into(), "unsupported".into());
                }
                "conditional" => {
                    rel.tags.insert(
                        "restriction:conditional".into(),
                        "no_right_turn @ (Mo)".into(),
                    );
                }
                "excepted" => {
                    rel.tags.insert("except".into(), "motorcar".into());
                }
                "other_mode" => {
                    rel.tags.remove("restriction");
                    rel.tags
                        .insert("restriction:bicycle".into(), "no_right_turn".into());
                }
                "nonrestriction" => {
                    rel.tags.insert("type".into(), "multipolygon".into());
                }
                _ => {}
            }
            if case == "miskeyed" {
                let way = osm.ways.remove(&10).unwrap();
                osm.ways.insert(1000, way);
            }
            osm.relations.push(rel);
            let rel = &osm.relations[0];
            let mut full: BTreeMap<i64, Vec<usize>> = BTreeMap::new();
            let mut sparse = restriction_way_index(&osm);
            for (id, edge) in g.edges.iter().enumerate() {
                full.entry(edge.way).or_default().push(id);
                if let Some(ids) = sparse.get_mut(&edge.way) {
                    ids.push(id);
                }
            }
            sparse.retain(|_, ids| !ids.is_empty());
            assert!(!sparse.contains_key(&i64::MAX));
            if !restrictions::applies(rel) || restrictions::conditional(rel) {
                assert!(sparse.is_empty(), "{case}");
                continue;
            }
            let mapping: Vec<_> = osm.nodes.keys().copied().filter(|id| *id <= 4).collect();
            let outgoing = Adjacency::new(g.nodes.len(), &g.edges).unwrap();
            let expected = restrictions::resolve(rel, &osm, &mapping, &full, &g.edges, &outgoing);
            let actual = if sparse.is_empty() {
                Ok(None)
            } else {
                restrictions::resolve(rel, &osm, &mapping, &sparse, &g.edges, &outgoing)
            };
            assert_eq!(format!("{expected:?}"), format!("{actual:?}"), "{case}");
            assert!(!sparse.contains_key(&40) || case == "unknown_role");
            if case == "static" {
                let (restricted, _) = graph(&osm).unwrap();
                let from = g
                    .edges
                    .iter()
                    .position(|e| e.way == 10 && e.to == 1)
                    .unwrap();
                let branch = g
                    .edges
                    .iter()
                    .position(|e| e.way == 40 && e.from == 1)
                    .unwrap();
                assert!(restricted.forbidden.contains(&vec![from, branch]));
            }
            if case == "excluded_only" {
                let (_, diagnostics) = graph(&osm).unwrap();
                assert_eq!(diagnostics["non_driving_restrictions"], 1);
            }
        }
    }
}
