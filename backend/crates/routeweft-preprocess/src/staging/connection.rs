//! External node/reference join for a checksummed staging directory.
mod blockways;
mod inventory;
pub use inventory::{audit_staged_restrictions, InventorySummary};
mod publication;
pub use publication::{publish_block_ways, PublicationReport};
mod edges;
use super::{Record, Run, RunReader, StageOptions, StageReport, NEXT};
use crate::{blocked_node, count, select_way, Result};
pub use blockways::prepare_with_policy;
pub use blockways::{prepare_block_ways, BlockWaysOptions, BlockWaysReport};
pub use edges::{
    emit_edges, index_adjacency, resolve_restrictions, AccessBudget, AdjacencyReport,
    DiskEdgeAccess, EdgeReport, RestrictionOptions, RestrictionReport, WayEdgeIndex,
    WayEdgeLocation,
};
use routeweft_dataset::{hash_file, invalid, resource_limit, Point};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

const MAGIC: &[u8; 8] = b"RWFTJ001";
const WIDTH: u64 = 48;
type Row = [u64; 6];
fn key(id: i64) -> u64 {
    (id as u64) ^ (1 << 63)
}
fn id(key: u64) -> i64 {
    (key ^ (1 << 63)) as i64
}

/// A joined intermediate, not a routing dataset or restriction validation result.
#[derive(Debug, Serialize, Deserialize)]
pub struct ConnectionReport {
    #[serde(default)]
    pub graph_capacity: routeweft_dataset::GraphCapacity,
    #[serde(default)]
    pub maxspeed_policy: crate::InvalidMaxspeedPolicy,
    #[serde(default)]
    pub maxspeed_diagnostics: Vec<crate::MaxspeedDiagnostic>,
    pub format_version: u32,
    pub source_records_sha256: String,
    pub source_sha256: String,
    pub source_snapshot: Option<i64>,
    pub profile: String,
    pub reference_closure_validated: bool,
    pub nodes: u64,
    pub selected_ways: u64,
    pub references: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_way_ids: Vec<i64>,
    pub diagnostics: BTreeMap<String, u64>,
    pub files: BTreeMap<String, (u64, String)>,
    pub peak_scratch_bytes: u64,
    pub peak_batch_bytes: u64,
    pub semantic_validation: bool,
}
struct Workspace {
    dir: PathBuf,
    options: StageOptions,
    live: u64,
    files: usize,
    peak: u64,
    peak_batch: u64,
    serial: u64,
    published: bool,
    scope: &'static str,
}
impl Drop for Workspace {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}
impl Workspace {
    fn reserve(&mut self, bytes: u64) -> Result<()> {
        let observed = self
            .live
            .checked_add(bytes)
            .ok_or_else(|| invalid("join scratch overflow"))?;
        if observed > self.options.scratch_bytes {
            return Err(resource_limit(
                if self.scope == "external_restrictions" {
                    "restriction_scratch_bytes"
                } else if self.scope == "external_adjacency" {
                    "adjacency_scratch_bytes"
                } else if self.scope == "external_edges" {
                    "edge_scratch_bytes"
                } else {
                    "join_scratch_bytes"
                },
                self.options.scratch_bytes,
                observed,
                self.scope,
            )
            .into());
        }
        self.live = observed;
        self.peak = self.peak.max(observed);
        Ok(())
    }
    fn path(&mut self) -> Result<PathBuf> {
        if self.files >= self.options.run_files {
            return Err(resource_limit(
                if self.scope == "external_restrictions" {
                    "restriction_run_files"
                } else if self.scope == "external_adjacency" {
                    "adjacency_run_files"
                } else if self.scope == "external_edges" {
                    "edge_run_files"
                } else {
                    "join_run_files"
                },
                self.options.run_files as u64,
                self.files as u64 + 1,
                self.scope,
            )
            .into());
        }
        self.files += 1;
        let path = self.dir.join(format!("{}.part", self.serial));
        self.serial += 1;
        Ok(path)
    }
    fn remove(&mut self, run: DiskRun) -> Result<()> {
        fs::remove_file(run.path)?;
        self.live -= run.bytes;
        self.files -= 1;
        Ok(())
    }
}
struct DiskRun {
    path: PathBuf,
    count: u64,
    bytes: u64,
    digest: String,
}
struct Writer {
    path: PathBuf,
    output: BufWriter<File>,
    count: u64,
}
impl Writer {
    fn new(work: &mut Workspace) -> Result<Self> {
        let path = work.path()?;
        work.reserve(16)?;
        let mut output = BufWriter::new(File::options().write(true).create_new(true).open(&path)?);
        output.write_all(MAGIC)?;
        output.write_all(&0_u64.to_le_bytes())?;
        Ok(Self {
            path,
            output,
            count: 0,
        })
    }
    fn push(&mut self, row: Row, work: &mut Workspace) -> Result<()> {
        work.reserve(WIDTH)?;
        for value in row {
            self.output.write_all(&value.to_le_bytes())?;
        }
        self.count += 1;
        Ok(())
    }
    fn finish(mut self) -> Result<DiskRun> {
        self.output.seek(SeekFrom::Start(8))?;
        self.output.write_all(&self.count.to_le_bytes())?;
        self.output.flush()?;
        self.output.get_ref().sync_all()?;
        drop(self.output);
        let digest = hash_file(&self.path)?;
        let path = self.path.with_extension("run");
        fs::rename(self.path, &path)?;
        Ok(DiskRun {
            path,
            count: self.count,
            bytes: 16 + WIDTH * self.count,
            digest,
        })
    }
}
struct Cursor {
    input: BufReader<File>,
    remaining: u64,
}
impl Cursor {
    fn open(run: &DiskRun) -> Result<Self> {
        if fs::metadata(&run.path)?.len() != run.bytes
            || run.bytes != 16 + WIDTH * run.count
            || hash_file(&run.path)? != run.digest
        {
            return Err(invalid("join run checksum/length").into());
        }
        let mut input = BufReader::new(File::open(&run.path)?);
        let mut header = [0; 16];
        input.read_exact(&mut header)?;
        if &header[..8] != MAGIC || u64::from_le_bytes(header[8..].try_into().unwrap()) != run.count
        {
            return Err(invalid("join run header").into());
        }
        Ok(Self {
            input,
            remaining: run.count,
        })
    }
    fn next(&mut self) -> Result<Option<Row>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        let mut bytes = [0; 48];
        self.input.read_exact(&mut bytes)?;
        Ok(Some(std::array::from_fn(|i| {
            u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap())
        })))
    }
}
#[derive(Default)]
struct Sorter {
    batch: Vec<Row>,
    levels: Vec<Option<DiskRun>>,
}
impl Sorter {
    fn push(&mut self, row: Row, work: &mut Workspace) -> Result<()> {
        if self.batch.len() >= work.options.batch_records
            || (self.batch.len() as u64 + 1) * WIDTH > work.options.batch_bytes
        {
            self.flush(work)?;
        }
        self.batch.push(row);
        work.peak_batch = work.peak_batch.max(self.batch.len() as u64 * WIDTH);
        Ok(())
    }
    fn flush(&mut self, work: &mut Workspace) -> Result<()> {
        if self.batch.is_empty() {
            return Ok(());
        }
        let mut batch = std::mem::take(&mut self.batch);
        batch.sort_unstable();
        let mut writer = Writer::new(work)?;
        for row in batch {
            writer.push(row, work)?;
        }
        let mut run = writer.finish()?;
        let mut level = 0;
        loop {
            if level == self.levels.len() {
                self.levels.push(None);
            }
            match self.levels[level].take() {
                Some(other) => run = merge(other, run, work)?,
                None => {
                    self.levels[level] = Some(run);
                    break;
                }
            }
            level += 1;
        }
        Ok(())
    }
    fn finish(mut self, work: &mut Workspace) -> Result<DiskRun> {
        self.flush(work)?;
        let mut result = None;
        for run in self.levels.into_iter().flatten() {
            result = Some(match result {
                Some(a) => merge(a, run, work)?,
                None => run,
            });
        }
        match result {
            Some(run) => Ok(run),
            None => Writer::new(work)?.finish(),
        }
    }
}
fn merge(a: DiskRun, b: DiskRun, work: &mut Workspace) -> Result<DiskRun> {
    let mut ar = Cursor::open(&a)?;
    let mut br = Cursor::open(&b)?;
    let mut left = ar.next()?;
    let mut right = br.next()?;
    let mut writer = Writer::new(work)?;
    while left.is_some() || right.is_some() {
        if right.is_none() || left.is_some_and(|v| v <= right.unwrap()) {
            writer.push(left.take().unwrap(), work)?;
            left = ar.next()?;
        } else {
            writer.push(right.take().unwrap(), work)?;
            right = br.next()?;
        }
    }
    let run = writer.finish()?;
    work.remove(a)?;
    work.remove(b)?;
    Ok(run)
}
fn contains(run: &DiskRun, file: &mut File, target: u64) -> Result<bool> {
    let (mut low, mut high) = (0, run.count);
    while low < high {
        let mid = low + (high - low) / 2;
        file.seek(SeekFrom::Start(16 + WIDTH * mid))?;
        let mut bytes = [0; 8];
        file.read_exact(&mut bytes)?;
        match u64::from_le_bytes(bytes).cmp(&target) {
            std::cmp::Ordering::Equal => return Ok(true),
            std::cmp::Ordering::Less => low = mid + 1,
            std::cmp::Ordering::Greater => high = mid,
        }
    }
    Ok(false)
}
struct Source {
    run: Run,
    report: StageReport,
    limit: u64,
}
impl Source {
    fn open(dir: &Path, limit: u64) -> Result<Self> {
        for name in ["manifest.json", "records.run"] {
            if !fs::symlink_metadata(dir.join(name))?.file_type().is_file() {
                return Err(invalid("join source must contain regular files").into());
            }
        }
        let manifest = File::open(dir.join("manifest.json"))?;
        if manifest.metadata()?.len() > 1024 * 1024 {
            return Err(invalid("join manifest size").into());
        }
        let report: StageReport =
            serde_json::from_reader(manifest).map_err(|e| invalid(e.to_string()))?;
        let count = report
            .counts
            .iter()
            .try_fold(0_u64, |a, b| a.checked_add(*b))
            .ok_or_else(|| invalid("stage counts overflow"))?;
        if report.format_version != 1
            || !report.duplicate_ids_checked
            || count > crate::pbf::STREAM_OBJECTS
            || report.records_bytes > crate::pbf::STREAM_OBJECT_BYTES
        {
            return Err(invalid("unsupported join source manifest").into());
        }
        let run = Run {
            path: dir.join("records.run"),
            count,
            bytes: report.records_bytes,
            digest: report.records_sha256.clone(),
        };
        Ok(Self { run, report, limit })
    }
    fn visit(&self, visitor: impl FnMut(Record) -> Result<()>) -> Result<()> {
        self.visit_kinds(&[0, 1, 2], visitor)
    }
    // Header counts, ordering, lengths and the full run checksum remain checked.
    // Staging already validated payload identities; only selected kinds need to
    // be decoded again, avoiding millions of JSON node parses in relation scans.
    fn visit_kinds(
        &self,
        kinds: &[u8],
        mut visitor: impl FnMut(Record) -> Result<()>,
    ) -> Result<()> {
        let reader = RunReader::open(&self.run, self.limit)?;
        let mut previous = None;
        let mut counts = [0; 3];
        let mut bytes = 16_u64;
        for entry in reader {
            let entry = entry?;
            if previous.is_some_and(|key| key >= entry.key) {
                return Err(invalid("stage record identity/order").into());
            }
            previous = Some(entry.key);
            counts[entry.key.0 as usize] += 1;
            bytes += 13 + entry.bytes.len() as u64;
            if !kinds.contains(&entry.key.0) {
                continue;
            }
            let record: Record =
                serde_json::from_slice(&entry.bytes).map_err(|e| invalid(e.to_string()))?;
            if record.key() != entry.key {
                return Err(invalid("stage record identity/order").into());
            }
            if let Record::Node {
                lon_bits, lat_bits, ..
            } = &record
            {
                let (lon, lat) = (f64::from_bits(*lon_bits), f64::from_bits(*lat_bits));
                if !lon.is_finite() || !lat.is_finite() || lon.abs() > 180. || lat.abs() > 90. {
                    return Err(invalid("stage node coordinates").into());
                }
            }
            visitor(record)?;
        }
        if counts != self.report.counts
            || bytes != self.run.bytes
            || hash_file(&self.run.path)? != self.run.digest
        {
            return Err(invalid("stage source counts/trailing bytes").into());
        }
        Ok(())
    }
}
/// Join selected driving-way references to nodes using disk sorting and lookup.
/// The source is a completed `stage` directory. No full OSM/ID collection is
/// loaded. This entry point applies no explicit exclusions; BlockWays uses a retry coordinator.
pub fn connect(source: &Path, output: &Path, options: StageOptions) -> Result<ConnectionReport> {
    connect_excluding(
        source,
        output,
        options,
        &BTreeSet::new(),
        Default::default(),
    )
}
fn connect_excluding(
    source: &Path,
    output: &Path,
    options: StageOptions,
    excluded: &BTreeSet<i64>,
    policy: crate::InvalidMaxspeedPolicy,
) -> Result<ConnectionReport> {
    let mut audit = Vec::new();
    connect_with_audit(source, output, options, excluded, policy, &mut audit).map_err(
        |mut error| {
            error.report.maxspeed_policy = policy;
            error.report.maxspeed_diagnostics = audit;
            error
        },
    )
}
fn connect_with_audit(
    source: &Path,
    output: &Path,
    options: StageOptions,
    excluded: &BTreeSet<i64>,
    policy: crate::InvalidMaxspeedPolicy,
    audit: &mut Vec<crate::MaxspeedDiagnostic>,
) -> Result<ConnectionReport> {
    if options.batch_bytes < WIDTH
        || options.batch_records == 0
        || options.record_bytes == 0
        || options.record_bytes > u32::MAX as u64
        || options.run_files == 0
    {
        return Err(invalid("invalid join budgets").into());
    }
    if output.exists() {
        return Err(invalid("join output already exists").into());
    }
    let source = Source::open(source, options.record_bytes)?;
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let dir = parent.join(format!(
        ".routeweft-join-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&dir)?;
    let mut work = Workspace {
        dir,
        options,
        live: 0,
        files: 0,
        peak: 0,
        peak_batch: 0,
        serial: 0,
        published: false,
        scope: "external_join",
    };
    let mut stats = BTreeMap::from([
        ("input_nodes".into(), source.report.counts[0]),
        ("input_ways".into(), source.report.counts[1]),
        ("input_relations".into(), source.report.counts[2]),
    ]);
    let mut conditionals = Sorter::default();
    source.visit(|record| {
        match record {
            Record::Relation(rel)
                if crate::restrictions::applies(&rel) && crate::restrictions::conditional(&rel) =>
            {
                count(&mut stats, "conditional_restrictions_blocked");
                for member in rel.members.into_iter().filter(|m| m.kind == 1) {
                    conditionals.push([key(member.id), 0, 0, 0, 0, 0], &mut work)?;
                }
            }
            Record::Node { tags, .. } if blocked_node(&tags) => count(&mut stats, "blocked_nodes"),
            _ => {}
        }
        Ok(())
    })?;
    let conditional = conditionals.finish(&mut work)?;
    // Verify the completed run before random-access membership lookups.
    let _ = Cursor::open(&conditional)?;
    let mut conditional_file = File::open(&conditional.path)?;
    let mut refs = Sorter::default();
    let mut selected = Writer::new(&mut work)?;
    let mut profile_error = None;
    source.visit(|record| {
        if let Record::Way(way) = record {
            if excluded.contains(&way.id) {
                return Ok(());
            }
            if profile_error.is_some() {
                return Ok(());
            }
            let selection = match select_way(
                &way,
                contains(&conditional, &mut conditional_file, key(way.id))?,
                &mut stats,
                policy,
                audit,
            ) {
                Ok(selection) => selection,
                Err(error) => {
                    profile_error = Some(error);
                    return Ok(());
                }
            };
            if let Some(selection) = selection {
                selected.push(
                    [
                        key(way.id),
                        selection.direction as i64 as u64,
                        selection.forward.to_bits(),
                        selection.backward.to_bits(),
                        0,
                        0,
                    ],
                    &mut work,
                )?;
                for (position, node) in way.refs.into_iter().enumerate() {
                    refs.push(
                        [key(node), key(way.id), position as u64, 0, 0, 0],
                        &mut work,
                    )?;
                }
            }
        }
        Ok(())
    })?;
    drop(conditional_file);
    work.remove(conditional)?;
    let selected = selected.finish()?;
    let refs = refs.finish(&mut work)?;
    let mut reference_reader = Cursor::open(&refs)?;
    let mut reference = reference_reader.next()?;
    let mut nodes = Writer::new(&mut work)?;
    let mut joined = Sorter::default();
    let mut missing: Option<(i64, u64, i64)> = None;
    let mut extent: Option<(f64, f64, f64, f64)> = None;
    source.visit(|record| {
        if let Record::Node {
            id: node,
            lon_bits,
            lat_bits,
            tags,
        } = record
        {
            let node_key = key(node);
            while reference.is_some_and(|r| r[0] < node_key) {
                let r = reference.take().unwrap();
                remember_missing(&mut missing, r);
                reference = reference_reader.next()?;
            }
            if reference.is_some_and(|r| r[0] == node_key) {
                let index = nodes.count;
                let blocked = u64::from(blocked_node(&tags));
                nodes.push([node_key, index, lon_bits, lat_bits, blocked, 0], &mut work)?;
                let point = Point {
                    lon: f64::from_bits(lon_bits),
                    lat: f64::from_bits(lat_bits),
                };
                extent = Some(match extent {
                    None => (point.lon, point.lon, point.lat, point.lat),
                    Some((a, b, c, d)) => (
                        a.min(point.lon),
                        b.max(point.lon),
                        c.min(point.lat),
                        d.max(point.lat),
                    ),
                });
                while reference.is_some_and(|r| r[0] == node_key) {
                    let r = reference.take().unwrap();
                    joined.push([r[1], r[2], index, blocked, node_key, 0], &mut work)?;
                    reference = reference_reader.next()?;
                }
            }
        }
        Ok(())
    })?;
    while let Some(r) = reference {
        remember_missing(&mut missing, r);
        reference = reference_reader.next()?;
    }
    if let Some((way, _, node)) = missing {
        return Err(invalid(format!("way {way} references missing node {node}")).into());
    }
    if let Some(error) = profile_error {
        return Err(error);
    }
    let limits = options.graph_capacity.limits();
    if nodes.count > limits.max_nodes as u64 {
        return Err(resource_limit(
            "graph_nodes",
            limits.max_nodes as u64,
            nodes.count,
            "graph_build",
        )
        .into());
    }
    if extent.is_some_and(|(a, b, c, d)| {
        b - a > options.graph_capacity.max_span_degrees()
            || d - c > options.graph_capacity.max_span_degrees()
    }) {
        return Err(invalid(format!(
            "unsupported geographic extent: {} graph must span at most {} degrees per axis",
            if options.graph_capacity == routeweft_dataset::GraphCapacity::City {
                "city"
            } else {
                "regional"
            },
            options.graph_capacity.max_span_degrees()
        ))
        .into());
    }
    let nodes = nodes.finish()?;
    work.remove(refs)?;
    let references = joined.finish(&mut work)?;
    if options.graph_capacity == routeweft_dataset::GraphCapacity::Regional {
        stats.insert("graph_capacity_regional".into(), 1);
    }
    let mut report = ConnectionReport {
        graph_capacity: options.graph_capacity,
        maxspeed_policy: policy,
        maxspeed_diagnostics: audit.clone(),
        format_version: 1,
        source_records_sha256: source.run.digest,
        source_sha256: source.report.source_sha256,
        source_snapshot: source.report.source_snapshot,
        profile: "driving-v1".into(),
        reference_closure_validated: true,
        nodes: nodes.count,
        selected_ways: selected.count,
        references: references.count,
        blocked_way_ids: excluded.iter().copied().collect(),
        diagnostics: stats,
        files: BTreeMap::new(),
        peak_scratch_bytes: work.peak,
        peak_batch_bytes: work.peak_batch,
        semantic_validation: false,
    };
    for (name, run) in [
        ("nodes.run", nodes),
        ("references.run", references),
        ("ways.run", selected),
    ] {
        report.files.insert(name.into(), (run.bytes, run.digest));
        fs::rename(run.path, work.dir.join(name))?;
    }
    let mut manifest = File::options()
        .write(true)
        .create_new(true)
        .open(work.dir.join("manifest.json"))?;
    serde_json::to_writer(&mut manifest, &report).map_err(|e| invalid(e.to_string()))?;
    manifest.sync_all()?;
    File::open(&work.dir)?.sync_all()?;
    fs::create_dir(output)?;
    if let Err(error) = fs::rename(&work.dir, output) {
        let _ = fs::remove_dir(output);
        return Err(error.into());
    }
    work.published = true;
    Ok(report)
}
fn remember_missing(missing: &mut Option<(i64, u64, i64)>, row: Row) {
    let candidate = (id(row[1]), row[2], id(row[0]));
    if missing.is_none_or(|old| candidate < old) {
        *missing = Some(candidate);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pbf::{Member, Node, Osm, Relation, Tags, Way};
    use std::collections::BTreeSet;
    pub(super) fn temporary() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "routeweft-join-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        path
    }
    pub(super) fn fixture() -> Osm {
        let mut osm = Osm::default();
        for (id, lon, blocked) in [
            (-3, 108.0, true),
            (2, 108.001, false),
            (9, 108.002, false),
            (100, 109.0, true),
        ] {
            osm.nodes.insert(
                id,
                Node {
                    id,
                    point: Point { lon, lat: 22.0 },
                    tags: if blocked {
                        Tags::from([("barrier".into(), "gate".into())])
                    } else {
                        Tags::new()
                    },
                },
            );
        }
        for (id, refs, class) in [
            (-8, vec![9, -3, 9, 2], "residential"),
            (1, vec![-3], "residential"),
            (7, vec![999], "footway"),
            (8, vec![-999], "residential"),
        ] {
            osm.ways.insert(
                id,
                Way {
                    id,
                    refs,
                    tags: Tags::from([("highway".into(), class.into())]),
                },
            );
        }
        osm.relations.push(Relation {
            id: 1,
            members: vec![Member {
                id: 8,
                kind: 1,
                role: "from".into(),
            }],
            tags: Tags::from([
                ("type".into(), "restriction".into()),
                (
                    "restriction:conditional".into(),
                    "no_left_turn @ (Mo)".into(),
                ),
            ]),
        });
        osm
    }
    pub(super) fn source(parent: &Path, osm: &Osm) -> PathBuf {
        let dir = parent.join("source");
        fs::create_dir(&dir).unwrap();
        let mut records = Vec::new();
        for node in osm.nodes.values() {
            records.push(Record::Node {
                id: node.id,
                lon_bits: node.point.lon.to_bits(),
                lat_bits: node.point.lat.to_bits(),
                tags: node.tags.clone(),
            });
        }
        for way in osm.ways.values() {
            records.push(Record::Way(Way {
                id: way.id,
                refs: way.refs.clone(),
                tags: way.tags.clone(),
            }));
        }
        for rel in &osm.relations {
            records.push(Record::Relation(Relation {
                id: rel.id,
                tags: rel.tags.clone(),
                members: rel
                    .members
                    .iter()
                    .map(|m| Member {
                        id: m.id,
                        kind: m.kind,
                        role: m.role.clone(),
                    })
                    .collect(),
            }));
        }
        records.sort_unstable_by_key(Record::key);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(super::super::MAGIC);
        bytes.extend_from_slice(&(records.len() as u64).to_le_bytes());
        let mut counts = [0; 3];
        for record in records {
            let key = record.key();
            let payload = serde_json::to_vec(&record).unwrap();
            counts[key.0 as usize] += 1;
            bytes.push(key.0);
            bytes.extend_from_slice(&key.1.to_le_bytes());
            bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&payload);
        }
        fs::write(dir.join("records.run"), &bytes).unwrap();
        let report = StageReport {
            format_version: 1,
            source_sha256: "0".repeat(64),
            source_snapshot: None,
            counts,
            records_sha256: hash_file(&dir.join("records.run")).unwrap(),
            records_bytes: bytes.len() as u64,
            duplicate_ids_checked: true,
            semantic_validation: false,
            initial_runs: 1,
            peak_scratch_bytes: bytes.len() as u64,
            peak_batch_bytes: 0,
        };
        fs::write(
            dir.join("manifest.json"),
            serde_json::to_vec(&report).unwrap(),
        )
        .unwrap();
        dir
    }
    fn rows(dir: &Path, name: &str, report: &ConnectionReport) -> Vec<Row> {
        let (bytes, digest) = &report.files[name];
        let run = DiskRun {
            path: dir.join(name),
            bytes: *bytes,
            digest: digest.clone(),
            count: (bytes - 16) / WIDTH,
        };
        let mut cursor = Cursor::open(&run).unwrap();
        let mut rows = Vec::new();
        while let Some(row) = cursor.next().unwrap() {
            rows.push(row);
        }
        rows
    }
    fn assert_memory_projection(osm: &Osm, out: &Path, report: &ConnectionReport) {
        let mut conditional = BTreeSet::new();
        let mut stats = BTreeMap::from([
            ("input_nodes".into(), osm.nodes.len() as u64),
            ("input_ways".into(), osm.ways.len() as u64),
            ("input_relations".into(), osm.relations.len() as u64),
        ]);
        for node in osm.nodes.values() {
            if blocked_node(&node.tags) {
                count(&mut stats, "blocked_nodes");
            }
        }
        for rel in &osm.relations {
            if crate::restrictions::applies(rel) && crate::restrictions::conditional(rel) {
                count(&mut stats, "conditional_restrictions_blocked");
                conditional.extend(rel.members.iter().filter(|m| m.kind == 1).map(|m| m.id));
            }
        }
        let mut used = BTreeSet::new();
        let mut selected = Vec::new();
        let mut references = Vec::new();
        for way in osm.ways.values() {
            if let Some(selection) = select_way(
                way,
                conditional.contains(&way.id),
                &mut stats,
                Default::default(),
                &mut Vec::new(),
            )
            .unwrap()
            {
                selected.push([
                    key(way.id),
                    selection.direction as i64 as u64,
                    selection.forward.to_bits(),
                    selection.backward.to_bits(),
                    0,
                    0,
                ]);
                for node in &way.refs {
                    assert!(osm.nodes.contains_key(node));
                    used.insert(*node);
                }
                references.extend(
                    way.refs
                        .iter()
                        .enumerate()
                        .map(|(position, node)| (way.id, position as u64, *node)),
                );
            }
        }
        let mapping: BTreeMap<_, _> = used
            .into_iter()
            .enumerate()
            .map(|(index, node)| (node, index as u64))
            .collect();
        let expected_nodes: Vec<Row> = mapping
            .iter()
            .map(|(id, index)| {
                let n = &osm.nodes[id];
                [
                    key(*id),
                    *index,
                    n.point.lon.to_bits(),
                    n.point.lat.to_bits(),
                    u64::from(blocked_node(&n.tags)),
                    0,
                ]
            })
            .collect();
        let expected_refs: Vec<Row> = references
            .iter()
            .map(|(way, position, node)| {
                [
                    key(*way),
                    *position,
                    mapping[node],
                    u64::from(blocked_node(&osm.nodes[node].tags)),
                    key(*node),
                    0,
                ]
            })
            .collect();
        assert_eq!(report.diagnostics, stats);
        assert_eq!(rows(out, "nodes.run", report), expected_nodes);
        assert_eq!(rows(out, "references.run", report), expected_refs);
        assert_eq!(rows(out, "ways.run", report), selected);
    }
    /// Explicit real-input acceptance, with full in-memory oracle only in tests.
    #[test]
    #[ignore = "requires ROUTEWEFT_CONNECT_PBF pointing at a local fixture"]
    fn real_pbf_join_matches_memory_projection() {
        let input = PathBuf::from(std::env::var("ROUTEWEFT_CONNECT_PBF").unwrap());
        let parent = temporary();
        let staged = parent.join("stage");
        let out = parent.join("joined");
        super::super::stage(
            &input,
            &staged,
            StageOptions {
                batch_records: 1024,
                ..Default::default()
            },
        )
        .unwrap();
        let report = connect(
            &staged,
            &out,
            StageOptions {
                batch_records: 512,
                ..Default::default()
            },
        )
        .unwrap();
        let osm = crate::pbf::read_for_driving(&input).unwrap();
        assert_memory_projection(&osm, &out, &report);
        println!("{}", serde_json::to_string(&report).unwrap());
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn external_join_matches_graph_nodes_and_preserves_reference_order() {
        let parent = temporary();
        let osm = fixture();
        let input = source(&parent, &osm);
        let (graph, stats) =
            crate::graph_attempt(&osm, &BTreeSet::new(), Default::default(), &mut Vec::new())
                .unwrap();
        let out = parent.join("joined");
        let report = connect(
            &input,
            &out,
            StageOptions {
                batch_records: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert_memory_projection(&osm, &out, &report);
        assert_eq!(report.nodes, 3);
        assert_eq!(report.references, 5);
        assert_eq!(report.selected_ways, 2);
        assert!(!report.semantic_validation);
        for (key, value) in &report.diagnostics {
            assert_eq!(stats.get(key), Some(value), "{key}");
        }
        let nodes = rows(&out, "nodes.run", &report);
        assert_eq!(
            nodes.iter().map(|r| id(r[0])).collect::<Vec<_>>(),
            [-3, 2, 9]
        );
        for (i, row) in nodes.iter().enumerate() {
            assert_eq!(row[1], i as u64);
            assert_eq!(row[2], graph.nodes[i].lon.to_bits());
            assert_eq!(row[3], graph.nodes[i].lat.to_bits());
        }
        assert_eq!(nodes[0][4], 1); // A blocked singleton remains in numbering.
        assert_eq!(report.diagnostics["blocked_nodes"], 2); // Includes unused node 100.
        let references = rows(&out, "references.run", &report);
        assert_eq!(
            references
                .iter()
                .map(|r| (id(r[0]), r[1], r[2], r[3], id(r[4])))
                .collect::<Vec<_>>(),
            [
                (-8, 0, 2, 0, 9),
                (-8, 1, 0, 1, -3),
                (-8, 2, 2, 0, 9),
                (-8, 3, 1, 0, 2),
                (1, 0, 0, 1, -3)
            ]
        );
        let again = parent.join("again");
        let second = connect(&input, &again, StageOptions::default()).unwrap();
        assert_eq!(report.files, second.files);
        assert_eq!(fs::read_dir(&out).unwrap().count(), 4);
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn missing_reference_and_profile_errors_follow_source_way_and_member_order() {
        for (refs, bad_id) in [(vec![999, -999], 99), (vec![9], -99)] {
            let parent = temporary();
            let mut osm = fixture();
            osm.ways.get_mut(&-8).unwrap().refs = refs;
            osm.ways.insert(
                bad_id,
                Way {
                    id: bad_id,
                    refs: vec![-888],
                    tags: Tags::from([
                        ("highway".into(), "residential".into()),
                        ("oneway".into(), "broken".into()),
                    ]),
                },
            );
            let expected =
                crate::graph_attempt(&osm, &BTreeSet::new(), Default::default(), &mut Vec::new())
                    .err()
                    .unwrap()
                    .to_string();
            let input = source(&parent, &osm);
            let result = connect(
                &input,
                &parent.join("out"),
                StageOptions {
                    batch_records: 1,
                    ..Default::default()
                },
            );
            assert_eq!(result.unwrap_err().to_string(), expected);
            assert_eq!(fs::read_dir(&parent).unwrap().count(), 1);
            fs::remove_dir_all(parent).unwrap();
        }
    }
    #[test]
    fn profile_and_extent_outcomes_match_existing_attempt() {
        for (tag, value) in [
            ("maxspeed", "broken"),
            ("maxspeed:backward", "301"),
            ("access", "private"),
            ("reversible", "yes"),
            ("maxwidth:physical", "2"),
            ("access:conditional", "no @ (Mo)"),
            ("area", "yes"),
            ("service", "emergency_access"),
            ("oneway", "-1"),
        ] {
            let parent = temporary();
            let mut osm = fixture();
            osm.ways.insert(
                2,
                Way {
                    id: 2,
                    refs: vec![2, 9],
                    tags: Tags::from([("highway".into(), "residential".into())]),
                },
            );
            osm.ways
                .get_mut(&-8)
                .unwrap()
                .tags
                .insert(tag.into(), value.into());
            let input = source(&parent, &osm);
            let result = connect(&input, &parent.join("out"), StageOptions::default());
            match crate::graph_attempt(&osm, &BTreeSet::new(), Default::default(), &mut Vec::new())
            {
                Ok((graph, stats)) => {
                    let report = result.unwrap();
                    assert_eq!(report.nodes as usize, graph.nodes.len());
                    for (key, value) in report.diagnostics {
                        assert_eq!(stats.get(&key), Some(&value), "{tag} {key}");
                    }
                }
                Err(error) => assert_eq!(result.unwrap_err().to_string(), error.to_string()),
            }
            fs::remove_dir_all(parent).unwrap();
        }
        let parent = temporary();
        let mut osm = fixture();
        osm.nodes.get_mut(&9).unwrap().point.lon = 118.;
        let expected =
            crate::graph_attempt(&osm, &BTreeSet::new(), Default::default(), &mut Vec::new())
                .err()
                .unwrap()
                .to_string();
        let input = source(&parent, &osm);
        assert_eq!(
            connect(&input, &parent.join("out"), StageOptions::default())
                .unwrap_err()
                .to_string(),
            expected
        );
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn streaming_manifest_capacity_does_not_bypass_record_validation() {
        let parent = temporary();
        let input = source(&parent, &fixture());
        let manifest_path = input.join("manifest.json");
        let mut manifest: StageReport =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest.counts = [2_000_001, 0, 0];
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let source = Source::open(&input, StageOptions::default().record_bytes).unwrap();
        // A larger declared workload is supported, but a forged count must not
        // let truncated/mismatched records pass the authenticated reader.
        assert!(source.visit(|_| Ok(())).is_err());
        manifest.counts = [crate::pbf::STREAM_OBJECTS + 1, 0, 0];
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(Source::open(&input, StageOptions::default().record_bytes).is_err());
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn source_mutation_during_a_pass_is_rejected() {
        let parent = temporary();
        let input = source(&parent, &fixture());
        let source = Source::open(&input, StageOptions::default().record_bytes).unwrap();
        let mut changed = false;
        let result = source.visit(|_| {
            if !changed {
                fs::write(input.join("records.run"), b"changed while reading")?;
                changed = true;
            }
            Ok(())
        });
        assert!(result.is_err());
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn empty_selection_and_manifest_corruption_are_handled() {
        let parent = temporary();
        let mut osm = fixture();
        osm.ways.clear();
        osm.relations.clear();
        let input = source(&parent, &osm);
        let out = parent.join("empty");
        let report = connect(&input, &out, StageOptions::default()).unwrap();
        assert_eq!(
            (report.nodes, report.selected_ways, report.references),
            (0, 0, 0)
        );
        assert!(rows(&out, "nodes.run", &report).is_empty());
        assert_eq!(report.diagnostics["blocked_nodes"], 2);
        let manifest_path = input.join("manifest.json");
        let mut manifest: StageReport =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        // The checksum is valid, but an authenticated framing/count mismatch
        // must still fail instead of silently accepting the extra bytes.
        let mut data = fs::read(input.join("records.run")).unwrap();
        data.push(0);
        fs::write(input.join("records.run"), &data).unwrap();
        manifest.records_bytes = data.len() as u64;
        manifest.records_sha256 = hash_file(&input.join("records.run")).unwrap();
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(connect(&input, &parent.join("invalid"), StageOptions::default()).is_err());
        assert_eq!(fs::read_dir(&parent).unwrap().count(), 2);
        manifest.counts[0] = u64::MAX;
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(connect(&input, &parent.join("invalid"), StageOptions::default()).is_err());
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn budget_corruption_cleanup_and_existing_output() {
        let parent = temporary();
        let input = source(&parent, &fixture());
        for options in [
            StageOptions {
                scratch_bytes: 16,
                ..Default::default()
            },
            StageOptions {
                run_files: 1,
                ..Default::default()
            },
            StageOptions {
                record_bytes: 1,
                ..Default::default()
            },
        ] {
            let error = connect(&input, &parent.join("out"), options).unwrap_err();
            assert!(matches!(
                error.source,
                routeweft_dataset::Error::ResourceLimit(_)
            ));
            assert_eq!(fs::read_dir(&parent).unwrap().count(), 1);
        }
        let out = parent.join("out");
        fs::create_dir(&out).unwrap();
        fs::write(out.join("sentinel"), b"keep").unwrap();
        assert!(connect(&input, &out, StageOptions::default()).is_err());
        assert_eq!(fs::read(out.join("sentinel")).unwrap(), b"keep");
        fs::write(input.join("records.run"), b"corrupt").unwrap();
        assert!(connect(&input, &parent.join("corrupt"), StageOptions::default()).is_err());
        assert_eq!(fs::read_dir(&parent).unwrap().count(), 2);
        fs::remove_dir_all(parent).unwrap();
    }
}
