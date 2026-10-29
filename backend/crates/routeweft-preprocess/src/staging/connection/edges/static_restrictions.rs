//! Resolve static restrictions through bounded disk graph access. Reject policy only.
use super::super::{key, ConnectionReport, Cursor, DiskRun, Row, Source, Workspace, Writer, WIDTH};
use super::{
    descriptor, manifest, regular, AccessBudget, AdjacencyReport, DiskEdgeAccess, EdgeReport,
    WayEdgeIndex,
};
use crate::restrictions::{self, Access, ResolveError, TurnEdge};
use crate::staging::{LimitedBytes, Record, StageOptions, NEXT};
use crate::{count, value, Error, InputReport, Result};
use routeweft_dataset::{hash_file, invalid, resource_limit};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::Path;
use std::sync::atomic::Ordering;

/// Budgets for this intermediate Reject-policy stage, not process RSS limits.
#[derive(Clone, Copy)]
pub struct RestrictionOptions {
    pub stage: StageOptions,
    pub relation_access_bytes: u64,
    /// Conservative logical charge for retained global only-prefixes and sequences.
    pub aggregation_bytes: u64,
    pub audit: crate::RestrictionAuditLimits,
}
impl Default for RestrictionOptions {
    fn default() -> Self {
        Self {
            stage: StageOptions::default(),
            relation_access_bytes: 64 * 1024 * 1024,
            aggregation_bytes: 64 * 1024 * 1024,
            audit: Default::default(),
        }
    }
}
/// Static restrictions for the input edge numbering; not a published dataset.
#[derive(Debug, Serialize, Deserialize)]
pub struct RestrictionReport {
    pub format_version: u32,
    pub policy: crate::InvalidRestrictionPolicy,
    pub relation_access_bytes: u64,
    pub aggregation_limit_bytes: u64,
    pub edge_manifest_sha256: String,
    pub adjacency_manifest_sha256: String,
    pub source_records_sha256: String,
    pub nodes: u64,
    pub edges: u64,
    pub restrictions: u64,
    pub diagnostics: BTreeMap<String, u64>,
    #[serde(default)]
    pub build_metrics: BTreeMap<String, u64>,
    pub files: BTreeMap<String, (u64, String)>,
    pub peak_scratch_bytes: u64,
    pub aggregation_charged_bytes: u64,
    pub restrictions_resolved: bool,
    pub semantic_validation: bool,
}
// Four-way bounded cache of exact rows. Large-page prefetch amplified reads
// on provincial inputs whose binary-search probes span distant node IDs.
const CACHE_SETS: usize = 4096;
const CACHE_WAYS: usize = 4;
#[derive(Clone, Copy)]
struct CachedRow {
    index: u64,
    row: Row,
    used: u64,
}
struct Table {
    file: File,
    count: u64,
    cache: Vec<[CachedRow; CACHE_WAYS]>,
    lookups: u64,
    probes: u64,
    reads: u64,
    read_bytes: u64,
}
impl Table {
    fn open(run: &DiskRun) -> Result<Self> {
        Ok(Self {
            file: File::open(&run.path)?,
            count: run.count,
            cache: vec![
                [CachedRow {
                    index: u64::MAX,
                    row: [0; 6],
                    used: 0
                }; CACHE_WAYS];
                CACHE_SETS
            ],
            lookups: 0,
            probes: 0,
            reads: 0,
            read_bytes: 0,
        })
    }
    fn find(&mut self, id: i64, budget: &mut AccessBudget) -> Result<Option<Row>> {
        self.lookups += 1;
        let (mut lo, mut hi) = (0, self.count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            // Always charge the same logical probe, including warm-cache hits.
            budget.charge(WIDTH)?;
            self.probes += 1;
            // Multiplicative hashing avoids collisions of aligned tree indices.
            let slot = (mid.wrapping_mul(0x9e3779b97f4a7c15) >> 52) as usize;
            let set = &mut self.cache[slot];
            let row = if let Some(cached) = set.iter_mut().find(|entry| entry.index == mid) {
                cached.used = self.probes;
                cached.row
            } else {
                self.file.seek(SeekFrom::Start(16 + WIDTH * mid))?;
                let mut bytes = [0; 48];
                self.file.read_exact(&mut bytes)?;
                self.reads += 1;
                self.read_bytes += WIDTH;
                let row: Row = std::array::from_fn(|i| {
                    u64::from_le_bytes(bytes[8 * i..8 * i + 8].try_into().unwrap())
                });
                let target = set.iter_mut().min_by_key(|entry| entry.used).unwrap();
                *target = CachedRow {
                    index: mid,
                    row,
                    used: self.probes,
                };
                row
            };
            match row[0].cmp(&key(id)) {
                std::cmp::Ordering::Equal => return Ok(Some(row)),
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
            }
        }
        Ok(None)
    }
}

struct DiskAccess {
    graph: DiskEdgeAccess,
    ways: WayEdgeIndex,
    raw_nodes: Table,
    raw_ways: Table,
    nodes: Table,
    budget: AccessBudget,
    cached_way: Option<(i64, Option<Range<u64>>)>,
}
impl DiskAccess {
    fn way_range(&mut self, way: i64) -> Result<Option<Range<u64>>> {
        if let Some((cached, range)) = &self.cached_way {
            if *cached == way {
                return Ok(range.clone());
            }
        }
        // The verified index has at most 2M rows: at most 21 binary-search reads.
        // Charge a conservative 32-row bound before invoking the shared lookup.
        self.budget.charge(32 * WIDTH)?;
        let range = self.ways.edge_range(way)?;
        self.cached_way = Some((way, range.clone()));
        Ok(range)
    }
}
impl Access for DiskAccess {
    fn has_way(&mut self, way: i64) -> Result<bool> {
        Ok(self.way_range(way)?.is_some())
    }
    fn way_edge(&mut self, way: i64, at: usize) -> Result<Option<usize>> {
        Ok(self
            .way_range(way)?
            .and_then(|range| {
                range
                    .start
                    .checked_add(at as u64)
                    .filter(|id| *id < range.end)
            })
            .map(|id| id as usize))
    }
    fn edge(&mut self, id: usize) -> Result<TurnEdge> {
        Ok((&self.graph.edge(id as u64, &mut self.budget)?).into())
    }
    fn outgoing(&mut self, node: usize, at: usize) -> Result<Option<usize>> {
        Ok(self
            .graph
            .outgoing_edge_at(node as u64, at as u64, &mut self.budget)?
            .map(|id| id as usize))
    }
    fn node(&mut self, id: i64) -> Result<Option<usize>> {
        Ok(self
            .nodes
            .find(id, &mut self.budget)?
            .map(|row| row[1] as usize))
    }
    fn reference_state(&mut self, kind: u64, id: i64) -> Result<u8> {
        if kind == 0 {
            Ok(if self.raw_nodes.find(id, &mut self.budget)?.is_some() {
                2
            } else {
                0
            })
        } else {
            Ok(self
                .raw_ways
                .find(id, &mut self.budget)?
                .map_or(0, |row| row[1] as u8))
        }
    }
}
struct Aggregation {
    forbidden: BTreeSet<Vec<usize>>,
    only: BTreeMap<Vec<usize>, BTreeSet<usize>>,
    charged: u64,
    limit: u64,
}
impl Aggregation {
    fn charge(&mut self, bytes: u64) -> Result<()> {
        let observed = self
            .charged
            .checked_add(bytes)
            .ok_or_else(|| invalid("restriction aggregation overflow"))?;
        if observed > self.limit {
            return Err(resource_limit(
                "restriction_aggregation_bytes",
                self.limit,
                observed,
                "external_restrictions",
            )
            .into());
        }
        self.charged = observed;
        Ok(())
    }
    fn forbid(&mut self, sequence: &[usize]) -> Result<()> {
        if !self.forbidden.contains(sequence) {
            self.charge(128 + 8 * sequence.len() as u64)?;
            self.forbidden.insert(sequence.to_vec());
        }
        Ok(())
    }
    fn allow(&mut self, prefix: &[usize], next: usize) -> Result<()> {
        if !self.only.contains_key(prefix) {
            self.charge(256 + 8 * prefix.len() as u64)?;
            self.only.insert(prefix.to_vec(), BTreeSet::new());
        }
        if !self.only[prefix].contains(&next) {
            self.charge(128)?;
            self.only.get_mut(prefix).unwrap().insert(next);
        }
        Ok(())
    }
}
/// Resolve static no/only restrictions without loading OSM or Graph in memory.
/// Invalid relations return structured Reject diagnostics and publish no output.
/// BlockWays retries, final dataset validation and production build integration are separate.
pub fn resolve_restrictions(
    staged: &Path,
    connected: &Path,
    edges: &Path,
    adjacency: &Path,
    output: &Path,
    options: RestrictionOptions,
) -> Result<RestrictionReport> {
    let mut audit = crate::audit::AuditCollector::new(options.audit, "external_restrictions")?;
    audit.begin(1);
    resolve_with_audit(
        staged, connected, edges, adjacency, output, options, &mut audit,
    )
    .map_err(|mut error| {
        audit.attach(&mut error.report);
        error
    })
}
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_with_audit(
    staged: &Path,
    connected: &Path,
    edges: &Path,
    adjacency: &Path,
    output: &Path,
    options: RestrictionOptions,
    audit: &mut crate::audit::AuditCollector,
) -> Result<RestrictionReport> {
    if options.stage.record_bytes == 0
        || options.stage.record_bytes > u32::MAX as u64
        || options.stage.run_files == 0
    {
        return Err(invalid("restriction options").into());
    }
    if output.exists() {
        return Err(invalid("restriction output exists").into());
    }
    let source = Source::open(staged, options.stage.record_bytes)?;
    let edge_report: EdgeReport = manifest(edges)?;
    let connection: ConnectionReport = manifest(connected)?;
    let edge_digest = hash_file(&edges.join("manifest.json"))?;
    let connection_digest = hash_file(&connected.join("manifest.json"))?;
    let adjacency_report: AdjacencyReport = manifest(adjacency)?;
    let stage_digest = hash_file(&staged.join("manifest.json"))?;
    regular(&adjacency.join("manifest.json"))?;
    let adjacency_digest = hash_file(&adjacency.join("manifest.json"))?;
    if edge_report.source_records_sha256 != source.run.digest
        || edge_report.source_sha256 != source.report.source_sha256
        || edge_report.source_snapshot != source.report.source_snapshot
        || edge_report.connection_manifest_sha256 != connection_digest
        || connection.source_records_sha256 != source.run.digest
        || connection.source_sha256 != source.report.source_sha256
        || connection.source_snapshot != source.report.source_snapshot
        || connection.nodes != edge_report.nodes
        || !connection.reference_closure_validated
        || connection.format_version != 1
        || connection.profile != "driving-v1"
    {
        return Err(invalid("restriction input identity mismatch").into());
    }
    let graph = DiskEdgeAccess::open(edges, adjacency)?;
    let ways = WayEdgeIndex::open(edges)?;
    let node_run = descriptor(connected, &connection.files, "nodes.run", connection.nodes)?;
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let dir = parent.join(format!(
        ".routeweft-restrictions-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&dir)?;
    let mut work = Workspace {
        dir,
        options: options.stage,
        live: 0,
        files: 0,
        peak: 0,
        peak_batch: 0,
        serial: 0,
        published: false,
        scope: "external_restrictions",
    };
    let mut raw_nodes = Writer::new(&mut work)?;
    let mut joined = Cursor::open(&node_run)?;
    let mut node = joined.next()?;
    let mut checked = 0;
    source.visit(|record| {
        if let Record::Node {
            id,
            lon_bits,
            lat_bits,
            tags,
        } = record
        {
            raw_nodes.push([key(id), 0, 0, 0, 0, 0], &mut work)?;
            if let Some(row) = node {
                if row[0] < key(id) {
                    return Err(invalid("restriction retained node missing").into());
                }
                if row[0] == key(id) {
                    if row
                        != [
                            key(id),
                            checked,
                            lon_bits,
                            lat_bits,
                            crate::blocked_node(&tags) as u64,
                            0,
                        ]
                    {
                        return Err(invalid("restriction retained node mismatch").into());
                    }
                    checked += 1;
                    node = joined.next()?;
                }
            }
        }
        Ok(())
    })?;
    if node.is_some() || checked != connection.nodes {
        return Err(invalid("restriction node coverage").into());
    }
    let raw_nodes = raw_nodes.finish()?;
    let mut node_table = Table::open(&raw_nodes)?;
    let mut raw_ways = Writer::new(&mut work)?;
    let closure_start = std::time::Instant::now();
    let mut closure_ways = 0u64;
    let mut build_budget = AccessBudget::new(u64::MAX);
    source.visit(|record| {
        if let Record::Way(way) = record {
            closure_ways += 1;
            if closure_ways % 100_000 == 0 {
                options.stage.observe(crate::staging::BuildProgress {
                    stage: "way_closure",
                    attempt: audit.summary.attempt as u64,
                    completed: closure_ways,
                    diagnostics_observed: audit.summary.records_observed,
                    scratch_peak_bytes: work.peak,
                    scratch_scope: "restrictions",
                    ..Default::default()
                });
            }
            let mut closed = true;
            for id in &way.refs {
                if node_table.find(*id, &mut build_budget)?.is_none() {
                    closed = false;
                    break;
                }
            }
            raw_ways.push(
                [key(way.id), if closed { 2 } else { 1 }, 0, 0, 0, 0],
                &mut work,
            )?;
        }
        Ok(())
    })?;
    let raw_ways = raw_ways.finish()?;
    let closure_metrics = [
        (
            "build_way_closure_elapsed_ms",
            closure_start.elapsed().as_millis() as u64,
        ),
        ("build_way_closure_node_lookups", node_table.lookups),
        ("build_way_closure_row_probes", node_table.probes),
        ("build_way_closure_disk_reads", node_table.reads),
        ("build_way_closure_disk_bytes", node_table.read_bytes),
    ];
    let mut access = DiskAccess {
        graph,
        ways,
        raw_nodes: node_table,
        raw_ways: Table::open(&raw_ways)?,
        nodes: Table::open(&node_run)?,
        budget: AccessBudget::new(0),
        cached_way: None,
    };
    let mut aggregation = Aggregation {
        forbidden: BTreeSet::new(),
        only: BTreeMap::new(),
        charged: 0,
        limit: options.aggregation_bytes,
    };
    let mut stats = edge_report.diagnostics.clone();
    let build_metrics: BTreeMap<String, u64> = closure_metrics
        .into_iter()
        .map(|(k, v)| (k.into(), v))
        .collect();
    let start = audit.records.len();
    source
        .visit(|record| {
            let Record::Relation(rel) = record else {
                return Ok(());
            };
            if value(&rel.tags, "type") != "restriction" {
                return Ok(());
            }
            if !restrictions::applies(&rel) {
                let key = if value(&rel.tags, "except")
                    .split(';')
                    .any(|v| matches!(v.trim(), "motorcar" | "motor_vehicle" | "vehicle"))
                {
                    "excepted_restrictions"
                } else {
                    "other_mode_restrictions"
                };
                count(&mut stats, key);
                return Ok(());
            }
            if restrictions::conditional(&rel) {
                return Ok(());
            }
            access.budget = AccessBudget::new(options.relation_access_bytes);
            access.cached_way = None;
            match restrictions::resolve_with(&rel, &mut access) {
                Ok(Some((rule, sequences))) => {
                    for sequence in sequences {
                        if rule.starts_with("only_") {
                            for split in 1..sequence.len() {
                                aggregation.allow(&sequence[..split], sequence[split])?;
                            }
                        } else {
                            aggregation.forbid(&sequence)?;
                        }
                    }
                    count(&mut stats, "resolved_restrictions");
                }
                Ok(None) => count(&mut stats, "non_driving_restrictions"),
                Err(ResolveError::Diagnostic(diagnostic)) => {
                    audit.push(*diagnostic)?;
                }
                Err(ResolveError::Access(error)) => return Err(error),
            }
            Ok(())
        })
        .map_err(|mut error| {
            error.report.nodes = edge_report.nodes as usize;
            error.report.edges = edge_report.edges as usize;
            error.report.diagnostics = stats.clone();
            error.report.build_metrics = build_metrics.clone();
            error.report.source_sha256 = source.report.source_sha256.clone();
            error.report.source_snapshot = source.report.source_snapshot;
            error
        })?;
    audit.complete();
    if audit.records.len() > start {
        return Err(Error {
            source: invalid("invalid driving restrictions"),
            report: Box::new(InputReport {
                nodes: edge_report.nodes as usize,
                edges: edge_report.edges as usize,
                diagnostics: stats,
                build_metrics: build_metrics.clone(),
                failure_code: Some("InvalidRestriction".into()),
                source_sha256: source.report.source_sha256.clone(),
                source_snapshot: source.report.source_snapshot,
                ..Default::default()
            }),
        });
    }
    // Merge allowed exits across all identical prefixes before emitting exclusions.
    let only = std::mem::take(&mut aggregation.only);
    for (prefix, allowed) in only {
        access.budget = AccessBudget::new(options.relation_access_bytes);
        let last = *prefix.last().ok_or_else(|| invalid("only prefix"))?;
        let node = access.edge(last)?.to;
        let mut at = 0;
        while let Some(next) = access.outgoing(node, at)? {
            at += 1;
            if !allowed.contains(&next) {
                let mut sequence = prefix.clone();
                sequence.push(next);
                aggregation.forbid(&sequence)?;
            }
        }
    }
    let path = work.path()?;
    let mut file = BufWriter::new(File::options().write(true).create_new(true).open(&path)?);
    let mut bytes = 0;
    for sequence in &aggregation.forbidden {
        let mut encoded = LimitedBytes {
            bytes: Vec::new(),
            limit: options.stage.record_bytes,
            exceeded: false,
        };
        if let Err(error) = serde_json::to_writer(&mut encoded, sequence) {
            if encoded.exceeded {
                return Err(resource_limit(
                    "restriction_record_bytes",
                    options.stage.record_bytes,
                    options.stage.record_bytes + 1,
                    "external_restrictions",
                )
                .into());
            }
            return Err(invalid(error.to_string()).into());
        }
        work.reserve(encoded.bytes.len() as u64 + 1)?;
        file.write_all(&encoded.bytes)?;
        file.write_all(b"\n")?;
        bytes += encoded.bytes.len() as u64 + 1;
    }
    file.flush()?;
    file.get_ref().sync_all()?;
    drop(file);
    let digest = hash_file(&path)?;
    if hash_file(&edges.join("manifest.json"))? != edge_digest
        || hash_file(&connected.join("manifest.json"))? != connection_digest
        || hash_file(&adjacency.join("manifest.json"))? != adjacency_digest
        || hash_file(&node_run.path)? != node_run.digest
    {
        return Err(invalid("restriction input changed").into());
    }
    if hash_file(&source.run.path)? != source.run.digest
        || hash_file(&staged.join("manifest.json"))? != stage_digest
    {
        return Err(invalid("restriction source changed").into());
    }
    for (directory, files) in [
        (edges, &edge_report.files),
        (adjacency, &adjacency_report.files),
    ] {
        for (name, (bytes, digest)) in files {
            // These file maps have already been checked against fixed names on open.
            let path = directory.join(name);
            regular(&path)?;
            if fs::metadata(&path)?.len() != *bytes || hash_file(&path)? != *digest {
                return Err(invalid("restriction graph changed").into());
            }
        }
    }
    // Work tables are owned scratch, never part of the completed result.
    drop(access);
    work.remove(raw_nodes)?;
    work.remove(raw_ways)?;
    fs::rename(path, work.dir.join("forbidden.jsonl"))?;
    stats.insert("invalid_restrictions_blocked".into(), 0);
    stats.insert("invalid_restriction_ways_blocked".into(), 0);
    let result = RestrictionReport {
        format_version: 1,
        policy: crate::InvalidRestrictionPolicy::Reject,
        relation_access_bytes: options.relation_access_bytes,
        aggregation_limit_bytes: options.aggregation_bytes,
        edge_manifest_sha256: edge_digest,
        adjacency_manifest_sha256: adjacency_digest,
        source_records_sha256: source.run.digest,
        nodes: edge_report.nodes,
        edges: edge_report.edges,
        restrictions: aggregation.forbidden.len() as u64,
        diagnostics: stats,
        build_metrics,
        files: BTreeMap::from([("forbidden.jsonl".into(), (bytes, digest))]),
        peak_scratch_bytes: work.peak,
        aggregation_charged_bytes: aggregation.charged,
        restrictions_resolved: true,
        semantic_validation: false,
    };
    let mut file = File::create(work.dir.join("manifest.json"))?;
    serde_json::to_writer(&mut file, &result).map_err(|e| invalid(e.to_string()))?;
    file.flush()?;
    file.sync_all()?;
    File::open(&work.dir)?.sync_all()?;
    fs::create_dir(output)?;
    if let Err(error) = fs::rename(&work.dir, output) {
        let _ = fs::remove_dir(output);
        return Err(error.into());
    }
    work.published = true;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pbf::{Member, Node, Osm, Relation, Tags, Way};
    use crate::staging::connection::tests::{source, temporary};
    use crate::staging::{connect, emit_edges, index_adjacency};
    use routeweft_dataset::Point;
    use std::io::{BufRead, BufReader};

    fn fixture() -> Osm {
        let mut osm = Osm::default();
        for id in -2..=8 {
            osm.nodes.insert(
                id,
                Node {
                    id,
                    point: Point {
                        lon: 108.0 + (id + 2) as f64 * 0.001,
                        lat: 22.0,
                    },
                    tags: Tags::new(),
                },
            );
        }
        for (id, refs) in [
            (-10, vec![-2, -1, 0]),
            (20, vec![0, 1, 2]),
            (30, vec![2, 3]),
            (40, vec![2, 4]),
            (50, vec![0, 5]),
            (60, vec![5, 6]),
            (70, vec![4, 7]),
        ] {
            osm.ways.insert(
                id,
                Way {
                    id,
                    refs,
                    tags: Tags::from([("highway".into(), "residential".into())]),
                },
            );
        }
        osm
    }
    fn relation(id: i64, rule: &str, members: &[(u64, i64, &str)]) -> Relation {
        Relation {
            id,
            tags: Tags::from([
                ("type".into(), "restriction".into()),
                ("restriction".into(), rule.into()),
            ]),
            members: members
                .iter()
                .map(|(kind, id, role)| Member {
                    kind: *kind,
                    id: *id,
                    role: (*role).into(),
                })
                .collect(),
        }
    }
    fn pipeline(parent: &Path, osm: &Osm) {
        let staged = source(parent, osm);
        connect(&staged, &parent.join("joined"), StageOptions::default()).unwrap();
        emit_edges(
            &staged,
            &parent.join("joined"),
            &parent.join("edges"),
            StageOptions::default(),
        )
        .unwrap();
        index_adjacency(
            &parent.join("edges"),
            &parent.join("adjacency"),
            StageOptions::default(),
        )
        .unwrap();
    }
    fn resolve(parent: &Path, options: RestrictionOptions) -> Result<RestrictionReport> {
        resolve_restrictions(
            &parent.join("source"),
            &parent.join("joined"),
            &parent.join("edges"),
            &parent.join("adjacency"),
            &parent.join("resolved"),
            options,
        )
    }
    #[test]
    fn disk_static_rules_match_memory_including_only_union_and_via_ways() {
        for case in [
            "node",
            "via_way",
            "multi_via",
            "only_union",
            "no_entry",
            "no_exit",
            "u_turn",
            "except",
            "conditional",
            "unknown_role",
        ] {
            let parent = temporary();
            let mut osm = fixture();
            let members = match case {
                "via_way" => vec![(1, -10, "from"), (1, 20, "via"), (1, 30, "to")],
                "multi_via" => vec![
                    (1, -10, "from"),
                    (1, 20, "via"),
                    (1, 40, "via"),
                    (1, 70, "to"),
                ],
                "no_entry" => vec![
                    (1, -10, "from"),
                    (1, 50, "from"),
                    (0, 0, "via"),
                    (1, 20, "to"),
                ],
                "no_exit" => vec![
                    (1, -10, "from"),
                    (0, 0, "via"),
                    (1, 20, "to"),
                    (1, 50, "to"),
                ],
                "u_turn" => vec![(1, -10, "from"), (0, 0, "via"), (1, -10, "to")],
                _ => vec![(1, -10, "from"), (0, 0, "via"), (1, 20, "to")],
            };
            let rule = match case {
                "only_union" => "only_straight_on",
                "no_entry" => "no_entry",
                "no_exit" => "no_exit",
                "u_turn" => "no_u_turn",
                _ => "no_right_turn",
            };
            osm.relations.push(relation(-3, rule, &members));
            if case == "only_union" {
                osm.relations.push(relation(
                    4,
                    rule,
                    &[(1, -10, "from"), (0, 0, "via"), (1, 50, "to")],
                ));
            }
            if case == "except" {
                osm.relations[0]
                    .tags
                    .insert("except".into(), "motorcar".into());
            }
            if case == "conditional" {
                osm.relations[0].tags.insert(
                    "restriction:conditional".into(),
                    "no_right_turn @ (Mo)".into(),
                );
            }
            if case == "unknown_role" {
                osm.relations[0].members.push(Member {
                    kind: 1,
                    id: 40,
                    role: "unknown".into(),
                });
            }
            let (graph, memory) = crate::prepare(&osm, Default::default()).unwrap();
            pipeline(&parent, &osm);
            let report = resolve(&parent, RestrictionOptions::default()).unwrap();
            let sequences: Vec<Vec<usize>> =
                BufReader::new(File::open(parent.join("resolved/forbidden.jsonl")).unwrap())
                    .lines()
                    .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
                    .collect();
            assert_eq!(sequences, graph.forbidden, "{case}");
            assert_eq!(report.diagnostics, memory.diagnostics, "{case}");
            assert_eq!(report.nodes as usize, graph.nodes.len());
            assert_eq!(report.edges as usize, graph.edges.len());
            fs::remove_dir_all(parent).unwrap();
        }
    }
    #[test]
    fn disk_diagnostics_match_memory_and_never_publish_invalid_relations() {
        for case in [
            "missing_via",
            "missing_from",
            "missing_node",
            "excluded_way_missing_node",
            "excluded_only",
            "bad_rule",
            "bad_kind",
            "bad_via",
            "repeated_via",
            "unresolved",
        ] {
            let parent = temporary();
            let mut osm = fixture();
            let mut members = vec![(1, -10, "from"), (0, 0, "via"), (1, 20, "to")];
            match case {
                "missing_via" => {
                    members.remove(1);
                }
                "missing_from" => {
                    members.remove(0);
                }
                "missing_node" => members[1].1 = 999,
                "excluded_way_missing_node" | "excluded_only" => {
                    osm.ways.insert(
                        99,
                        Way {
                            id: 99,
                            refs: vec![999],
                            tags: Tags::from([("highway".into(), "footway".into())]),
                        },
                    );
                    if case == "excluded_only" {
                        osm.ways.get_mut(&99).unwrap().refs = vec![0, 1];
                    }
                    members[2].1 = 99;
                }
                "bad_kind" => members[0].0 = 2,
                "bad_via" => members.push((1, 40, "via")),
                "repeated_via" => members = vec![(1, -10, "from"), (1, 20, "via"), (1, 20, "to")],
                "unresolved" => members[1].1 = 8,
                _ => (),
            }
            let rule = if case == "bad_rule" {
                "no_flying"
            } else if case == "excluded_only" {
                "only_right_turn"
            } else {
                "no_right_turn"
            };
            osm.relations.push(relation(3, rule, &members));
            let memory = crate::prepare(&osm, Default::default()).unwrap_err();
            pipeline(&parent, &osm);
            let disk = resolve(&parent, RestrictionOptions::default()).unwrap_err();
            assert_eq!(
                serde_json::to_value(&disk.report.restriction_diagnostics).unwrap(),
                serde_json::to_value(&memory.report.restriction_diagnostics).unwrap(),
                "{case}"
            );
            assert_eq!(disk.report.diagnostics, memory.report.diagnostics, "{case}");
            assert!(!parent.join("resolved").exists());
            fs::remove_dir_all(parent).unwrap();
        }
    }
    #[test]
    fn inconsistent_sources_and_corruption_fail_without_overwriting_output() {
        let parent = temporary();
        pipeline(&parent, &fixture());
        let path = parent.join("joined/manifest.json");
        let original = fs::read(&path).unwrap();
        let mut report: serde_json::Value = serde_json::from_slice(&original).unwrap();
        report["source_records_sha256"] = "different".into();
        fs::write(&path, serde_json::to_vec(&report).unwrap()).unwrap();
        assert!(resolve(&parent, RestrictionOptions::default())
            .unwrap_err()
            .to_string()
            .contains("identity mismatch"));
        assert!(!parent.join("resolved").exists());
        fs::write(&path, original).unwrap();
        let payload = parent.join("edges/edges.jsonl");
        let original = fs::read(&payload).unwrap();
        fs::write(&payload, b"corrupt").unwrap();
        assert!(resolve(&parent, RestrictionOptions::default()).is_err());
        assert!(!parent.join("resolved").exists());
        fs::write(&payload, original).unwrap();
        resolve(&parent, RestrictionOptions::default()).unwrap();
        let manifest = fs::read(parent.join("resolved/manifest.json")).unwrap();
        assert!(resolve(&parent, RestrictionOptions::default()).is_err());
        assert_eq!(
            fs::read(parent.join("resolved/manifest.json")).unwrap(),
            manifest
        );
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn disk_resource_failures_are_not_misclassified_as_bad_restrictions() {
        let parent = temporary();
        let mut osm = fixture();
        osm.relations.push(relation(
            1,
            "only_right_turn",
            &[(1, -10, "from"), (0, 0, "via"), (1, 20, "to")],
        ));
        pipeline(&parent, &osm);
        for (options, resource) in [
            (
                RestrictionOptions {
                    relation_access_bytes: 0,
                    ..Default::default()
                },
                "edge_access_bytes",
            ),
            (
                RestrictionOptions {
                    aggregation_bytes: 0,
                    ..Default::default()
                },
                "restriction_aggregation_bytes",
            ),
            (
                RestrictionOptions {
                    stage: StageOptions {
                        scratch_bytes: 16,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                "restriction_scratch_bytes",
            ),
            (
                RestrictionOptions {
                    stage: StageOptions {
                        run_files: 1,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                "restriction_run_files",
            ),
        ] {
            let error = resolve(&parent, options).unwrap_err();
            assert!(error.to_string().contains(resource), "{error}");
            assert!(error.report.restriction_diagnostics.is_empty());
            assert!(!parent.join("resolved").exists());
            assert!(!fs::read_dir(&parent).unwrap().any(|e| e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".routeweft-restrictions")));
        }
        fs::remove_dir_all(parent).unwrap();
    }
}

#[cfg(test)]
mod local_acceptance {
    use super::*;
    use crate::staging::{connect, emit_edges, index_adjacency, stage};
    use std::io::{BufRead, BufReader};
    #[test]
    #[ignore = "requires ROUTEWEFT_RESTRICTIONS_PBF pointing at a local fixture"]
    fn real_pbf_restrictions_match_memory_result_or_reject_diagnostics() {
        let input = std::path::PathBuf::from(std::env::var("ROUTEWEFT_RESTRICTIONS_PBF").unwrap());
        let parent = std::env::temp_dir().join(format!(
            "routeweft-static-local-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&parent).unwrap();
        let staged = parent.join("stage");
        let joined = parent.join("join");
        let edges = parent.join("edges");
        let adjacency = parent.join("adjacency");
        let output = parent.join("resolved");
        stage(&input, &staged, StageOptions::default()).unwrap();
        connect(&staged, &joined, StageOptions::default()).unwrap();
        emit_edges(&staged, &joined, &edges, StageOptions::default()).unwrap();
        index_adjacency(&edges, &adjacency, StageOptions::default()).unwrap();
        let osm = crate::pbf::read_for_driving(&input).unwrap();
        let memory = crate::prepare(&osm, Default::default());
        let disk = resolve_restrictions(
            &staged,
            &joined,
            &edges,
            &adjacency,
            &output,
            RestrictionOptions::default(),
        );
        match (memory, disk) {
            (Ok((graph, expected)), Ok(report)) => {
                let sequences: Vec<Vec<usize>> =
                    BufReader::new(File::open(output.join("forbidden.jsonl")).unwrap())
                        .lines()
                        .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
                        .collect();
                assert_eq!(sequences, graph.forbidden);
                assert_eq!(report.diagnostics, expected.diagnostics);
                println!("{}", serde_json::to_string(&report).unwrap());
            }
            (Err(expected), Err(actual)) => {
                assert_eq!(actual.report.failure_code, expected.report.failure_code);
                assert_eq!(actual.report.diagnostics, expected.report.diagnostics);
                assert_eq!(
                    serde_json::to_value(&actual.report.restriction_diagnostics).unwrap(),
                    serde_json::to_value(&expected.report.restriction_diagnostics).unwrap()
                );
                assert_eq!(actual.report.nodes, expected.report.nodes);
                assert_eq!(actual.report.edges, expected.report.edges);
                assert!(!output.exists());
                println!("{}", serde_json::to_string(&actual.report).unwrap());
            }
            _ => panic!("disk and memory restriction outcomes differ"),
        }
        fs::remove_dir_all(parent).unwrap();
    }
}
