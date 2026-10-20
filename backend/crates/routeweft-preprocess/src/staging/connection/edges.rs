//! Stream direction edges and a disk way-to-edge range index.
mod adjacency;
mod static_restrictions;
use super::{key, ConnectionReport, Cursor, DiskRun, Row, Source, Workspace, Writer, WIDTH};
use crate::staging::{LimitedBytes, Record, StageOptions, NEXT};
use crate::{
    blocked_node, count, directional_access, edge_duration, segment_length, select_way, value,
    Result,
};
pub use adjacency::{index_adjacency, AccessBudget, AdjacencyReport, DiskEdgeAccess};
use routeweft_dataset::{hash_file, invalid, resource_limit, Edge, Point};
use serde::{Deserialize, Serialize};
pub use static_restrictions::{resolve_restrictions, RestrictionOptions, RestrictionReport};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

/// Intermediate direction edges. Restrictions have not been resolved.
#[derive(Debug, Serialize, Deserialize)]
pub struct EdgeReport {
    #[serde(default)]
    pub graph_capacity: routeweft_dataset::GraphCapacity,
    pub format_version: u32,
    pub source_records_sha256: String,
    pub connection_manifest_sha256: String,
    pub source_sha256: String,
    pub source_snapshot: Option<i64>,
    pub profile: String,
    pub nodes: u64,
    pub edges: u64,
    pub indexed_ways: u64,
    pub diagnostics: BTreeMap<String, u64>,
    pub files: BTreeMap<String, (u64, String)>,
    pub peak_scratch_bytes: u64,
    pub restrictions_resolved: bool,
    pub semantic_validation: bool,
}
fn regular(path: &Path) -> Result<()> {
    if !fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(invalid("edge input must be a regular file").into());
    }
    Ok(())
}
fn manifest<T: serde::de::DeserializeOwned>(dir: &Path) -> Result<T> {
    let path = dir.join("manifest.json");
    regular(&path)?;
    let file = File::open(path)?;
    if file.metadata()?.len() > 1024 * 1024 {
        return Err(invalid("edge manifest size").into());
    }
    serde_json::from_reader(file).map_err(|e| invalid(e.to_string()).into())
}
fn descriptor(
    dir: &Path,
    files: &BTreeMap<String, (u64, String)>,
    name: &str,
    count: u64,
) -> Result<DiskRun> {
    let (bytes, digest) = files
        .get(name)
        .ok_or_else(|| invalid("missing edge input descriptor"))?;
    let expected = count
        .checked_mul(WIDTH)
        .and_then(|n| n.checked_add(16))
        .ok_or_else(|| invalid("edge input size overflow"))?;
    if *bytes != expected {
        return Err(invalid("edge input count/size").into());
    }
    let path = dir.join(name);
    regular(&path)?;
    let run = DiskRun {
        path,
        count,
        bytes: *bytes,
        digest: digest.clone(),
    };
    let _ = Cursor::open(&run)?;
    Ok(run)
}
struct NodeTable {
    file: File,
    count: u64,
}
impl NodeTable {
    fn get(&mut self, index: u64) -> Result<Row> {
        if index >= self.count {
            return Err(invalid("edge reference node index").into());
        }
        self.file.seek(SeekFrom::Start(16 + WIDTH * index))?;
        let mut bytes = [0; 48];
        self.file.read_exact(&mut bytes)?;
        let row = std::array::from_fn(|i| {
            u64::from_le_bytes(bytes[8 * i..8 * i + 8].try_into().unwrap())
        });
        if row[1] != index || row[4] > 1 || row[5] != 0 {
            return Err(invalid("edge node row").into());
        }
        Ok(row)
    }
}
fn point(row: Row) -> Point {
    Point {
        lon: f64::from_bits(row[2]),
        lat: f64::from_bits(row[3]),
    }
}
struct EdgeWriter {
    path: PathBuf,
    output: BufWriter<File>,
    count: u64,
    bytes: u64,
}
impl EdgeWriter {
    fn new(work: &mut Workspace) -> Result<Self> {
        let path = work.path()?;
        Ok(Self {
            output: BufWriter::new(File::options().write(true).create_new(true).open(&path)?),
            path,
            count: 0,
            bytes: 0,
        })
    }
    fn push(&mut self, edge: Edge, work: &mut Workspace) -> Result<()> {
        let limit = work.options.graph_capacity.limits().max_edges as u64;
        if self.count >= limit {
            return Err(resource_limit("graph_edges", limit, self.count + 1, "graph_build").into());
        }
        if edge.name.len() > 4096 || edge.class.len() > 64 {
            return Err(invalid("edge").into());
        }
        let mut encoded = LimitedBytes {
            bytes: Vec::new(),
            limit: work.options.record_bytes,
            exceeded: false,
        };
        let result = serde_json::to_writer(&mut encoded, &edge);
        if encoded.exceeded {
            return Err(resource_limit(
                "edge_record_bytes",
                encoded.limit,
                encoded.limit.saturating_add(1),
                "external_edges",
            )
            .into());
        }
        result.map_err(|e| invalid(e.to_string()))?;
        let size = encoded.bytes.len() as u64 + 1;
        work.reserve(size)?;
        self.output.write_all(&encoded.bytes)?;
        self.output.write_all(b"\n")?;
        self.count += 1;
        self.bytes += size;
        Ok(())
    }
    fn finish(mut self) -> Result<(u64, String)> {
        self.output.flush()?;
        self.output.get_ref().sync_all()?;
        drop(self.output);
        let digest = hash_file(&self.path)?;
        let path = self.path.with_file_name("edges.jsonl");
        fs::rename(self.path, path)?;
        Ok((self.bytes, digest))
    }
}
/// Emit edges from matching source/connection directories without a full Graph.
/// The result has no resolved static restrictions; explicit connection exclusions are honored.
pub fn emit_edges(
    staged: &Path,
    connected: &Path,
    output: &Path,
    options: StageOptions,
) -> Result<EdgeReport> {
    if options.record_bytes == 0 || options.record_bytes > u32::MAX as u64 || options.run_files == 0
    {
        return Err(invalid("invalid edge budgets").into());
    }
    if output.exists() {
        return Err(invalid("edge output already exists").into());
    }
    let source = Source::open(staged, options.record_bytes)?;
    let connection: ConnectionReport = manifest(connected)?;
    let connection_digest = hash_file(&connected.join("manifest.json"))?;
    if connection.format_version != 1
        || connection.profile != "driving-v1"
        || !connection.reference_closure_validated
        || connection.graph_capacity != options.graph_capacity
        || connection.nodes > options.graph_capacity.limits().max_nodes as u64
        || connection.blocked_way_ids.len() > 32_768
        || connection.blocked_way_ids.windows(2).any(|w| w[0] >= w[1])
        || connection.selected_ways > source.report.counts[1]
        || connection.source_records_sha256 != source.run.digest
        || connection.source_sha256 != source.report.source_sha256
        || connection.source_snapshot != source.report.source_snapshot
        || connection.files.len() != 3
    {
        return Err(invalid("edge source/connection mismatch").into());
    }
    let node_run = descriptor(connected, &connection.files, "nodes.run", connection.nodes)?;
    let refs_run = descriptor(
        connected,
        &connection.files,
        "references.run",
        connection.references,
    )?;
    let ways_run = descriptor(
        connected,
        &connection.files,
        "ways.run",
        connection.selected_ways,
    )?;
    let mut speed_audit = Vec::new();
    let mut node_rows = Cursor::open(&node_run)?;
    let mut node = node_rows.next()?;
    let mut refs = Cursor::open(&refs_run)?;
    let mut ways = Cursor::open(&ways_run)?;
    let mut selected = ways.next()?;
    let mut table = NodeTable {
        file: File::open(&node_run.path)?,
        count: connection.nodes,
    };
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let dir = parent.join(format!(
        ".routeweft-edges-{}-{}",
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
        scope: "external_edges",
    };
    let mut edges = EdgeWriter::new(&mut work)?;
    let mut index = Writer::new(&mut work)?;
    let mut stats = connection.diagnostics.clone();
    let mut checked_nodes = 0;
    let mut previous_node = None;
    let mut previous_way = None;
    let mut unsupported_node_latitude = false;
    source.visit(|record| {
        match record {
            Record::Node {
                id: source_id,
                lon_bits,
                lat_bits,
                tags,
            } => {
                if node.is_some_and(|row| row[0] < key(source_id)) {
                    return Err(invalid("joined node absent from source").into());
                }
                if node.is_some_and(|row| row[0] == key(source_id)) {
                    let row = node.take().unwrap();
                    if row
                        != [
                            key(source_id),
                            checked_nodes,
                            lon_bits,
                            lat_bits,
                            u64::from(blocked_node(&tags)),
                            0,
                        ]
                        || previous_node.is_some_and(|old| old >= row[0])
                    {
                        return Err(invalid("joined node differs from source").into());
                    }
                    unsupported_node_latitude |= f64::from_bits(lat_bits).abs() > 85.;
                    previous_node = Some(row[0]);
                    checked_nodes += 1;
                    node = node_rows.next()?;
                }
            }
            Record::Way(way) => {
                if node.is_some() {
                    return Err(invalid("joined node absent from source").into());
                }
                if selected.is_some_and(|row| row[0] < key(way.id)) {
                    return Err(invalid("selected way absent from source").into());
                }
                if selected.is_some_and(|row| row[0] == key(way.id)) {
                    let row = selected.take().unwrap();
                    if connection.blocked_way_ids.binary_search(&way.id).is_ok() {
                        return Err(invalid("selected way is explicitly blocked").into());
                    }
                    if previous_way.is_some_and(|old| old >= row[0]) {
                        return Err(invalid("selected way order").into());
                    }
                    previous_way = Some(row[0]);
                    let expected = select_way(
                        &way,
                        false,
                        &mut BTreeMap::new(),
                        connection.maxspeed_policy,
                        &mut speed_audit,
                    )?
                    .ok_or_else(|| invalid("selected way excluded by profile"))?;
                    if row
                        != [
                            key(way.id),
                            expected.direction as i64 as u64,
                            expected.forward.to_bits(),
                            expected.backward.to_bits(),
                            0,
                            0,
                        ]
                    {
                        return Err(invalid("selected way profile").into());
                    }
                    let start = edges.count;
                    let byte_start = edges.bytes;
                    let mut previous: Option<(Row, Row)> = None;
                    for (position, source_node) in way.refs.iter().enumerate() {
                        let reference = refs
                            .next()?
                            .ok_or_else(|| invalid("missing joined reference"))?;
                        if reference[0] != key(way.id)
                            || reference[1] != position as u64
                            || reference[4] != key(*source_node)
                            || reference[5] != 0
                        {
                            return Err(invalid("joined reference order/identity").into());
                        }
                        let node = table.get(reference[2])?;
                        if node[0] != reference[4] || node[4] != reference[3] {
                            return Err(invalid("joined reference node/blocked flag").into());
                        }
                        if let Some((prior_ref, prior_node)) = previous {
                            if prior_ref[3] == 0 && reference[3] == 0 {
                                let length = segment_length(point(prior_node), point(node))?;
                                if length == 0 {
                                    count(&mut stats, "zero_length_segments_omitted");
                                } else {
                                    for (from, to, allowed, speed) in [
                                        (
                                            prior_ref[2],
                                            reference[2],
                                            expected.direction >= 0
                                                && directional_access(&way.tags, "forward"),
                                            expected.forward,
                                        ),
                                        (
                                            reference[2],
                                            prior_ref[2],
                                            expected.direction <= 0
                                                && directional_access(&way.tags, "backward"),
                                            expected.backward,
                                        ),
                                    ] {
                                        if allowed {
                                            edges.push(
                                                Edge {
                                                    from: from as usize,
                                                    to: to as usize,
                                                    way: way.id,
                                                    distance_mm: length,
                                                    duration_ms: edge_duration(length, speed),
                                                    name: value(&way.tags, "name").into(),
                                                    class: value(&way.tags, "highway").into(),
                                                    roundabout: value(&way.tags, "junction")
                                                        == "roundabout",
                                                },
                                                &mut work,
                                            )?;
                                        }
                                    }
                                }
                            }
                        }
                        previous = Some((reference, node));
                    }
                    if edges.count > start {
                        index.push(
                            [
                                key(way.id),
                                start,
                                edges.count - start,
                                byte_start,
                                edges.bytes - byte_start,
                                0,
                            ],
                            &mut work,
                        )?;
                    }
                    selected = ways.next()?;
                }
            }
            _ => {}
        }
        Ok(())
    })?;
    if node.is_some()
        || checked_nodes != connection.nodes
        || selected.is_some()
        || refs.next()?.is_some()
    {
        return Err(invalid("unconsumed edge input rows").into());
    }
    if speed_audit != connection.maxspeed_diagnostics {
        return Err(invalid("connection maxspeed audit differs from source").into());
    }
    if connection.nodes == 0 || edges.count == 0 {
        return Err(invalid("no driving roads").into());
    }
    if unsupported_node_latitude {
        return Err(invalid("supported latitude is [-85,85]").into());
    }
    for run in [&node_run, &refs_run, &ways_run] {
        if hash_file(&run.path)? != run.digest {
            return Err(invalid("connection changed during edge generation").into());
        }
    }
    if hash_file(&connected.join("manifest.json"))? != connection_digest {
        return Err(invalid("connection manifest changed").into());
    }
    let edge_count = edges.count;
    let edge_file = edges.finish()?;
    let index = index.finish()?;
    let report = EdgeReport {
        graph_capacity: options.graph_capacity,
        format_version: 1,
        source_records_sha256: source.run.digest,
        connection_manifest_sha256: connection_digest,
        source_sha256: source.report.source_sha256,
        source_snapshot: source.report.source_snapshot,
        profile: "driving-v1".into(),
        nodes: connection.nodes,
        edges: edge_count,
        indexed_ways: index.count,
        diagnostics: stats,
        files: BTreeMap::from([
            ("edges.jsonl".into(), edge_file),
            ("way-edges.run".into(), (index.bytes, index.digest)),
        ]),
        peak_scratch_bytes: work.peak,
        restrictions_resolved: false,
        semantic_validation: false,
    };
    fs::rename(index.path, work.dir.join("way-edges.run"))?;
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
/// A verified way-range index. Opening verifies the index, not edges.jsonl.
/// Keep the directory immutable for the lifetime of this reader.
pub struct WayEdgeIndex {
    file: File,
    count: u64,
    edges: u64,
    bytes: u64,
}
/// Both logical edge IDs and the directly seekable edges.jsonl byte span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WayEdgeLocation {
    pub edge_ids: Range<u64>,
    pub byte_range: Range<u64>,
}
impl WayEdgeIndex {
    pub fn open(directory: &Path) -> Result<Self> {
        let report: EdgeReport = manifest(directory)?;
        if report.format_version != 1
            || report.profile != "driving-v1"
            || report.edges > report.graph_capacity.limits().max_edges as u64
            || report.indexed_ways > report.edges
            || report.edges == 0
        {
            return Err(invalid("way edge index manifest").into());
        }
        let run = descriptor(
            directory,
            &report.files,
            "way-edges.run",
            report.indexed_ways,
        )?;
        let (edge_bytes, _) = report
            .files
            .get("edges.jsonl")
            .ok_or_else(|| invalid("missing edge payload descriptor"))?;
        let edge_path = directory.join("edges.jsonl");
        regular(&edge_path)?;
        if fs::metadata(edge_path)?.len() != *edge_bytes {
            return Err(invalid("edge payload length").into());
        }
        let mut rows = Cursor::open(&run)?;
        let mut previous = None;
        let mut end = 0;
        let mut byte_end = 0;
        while let Some(row) = rows.next()? {
            let next = row[1]
                .checked_add(row[2])
                .ok_or_else(|| invalid("way edge range overflow"))?;
            let next_byte = row[3]
                .checked_add(row[4])
                .ok_or_else(|| invalid("way byte range overflow"))?;
            if previous.is_some_and(|old| old >= row[0])
                || row[1] != end
                || row[2] == 0
                || next > report.edges
                || row[3] != byte_end
                || row[4] == 0
                || next_byte > *edge_bytes
                || row[5] != 0
            {
                return Err(invalid("way edge range/order").into());
            }
            previous = Some(row[0]);
            end = next;
            byte_end = next_byte;
        }
        if end != report.edges || byte_end != *edge_bytes {
            return Err(invalid("way edge index coverage").into());
        }
        Ok(Self {
            file: File::open(run.path)?,
            count: run.count,
            edges: report.edges,
            bytes: *edge_bytes,
        })
    }
    /// Return the contiguous edge-ID range, or None for ways with no edges.
    pub fn edge_range(&mut self, way: i64) -> Result<Option<Range<u64>>> {
        Ok(self.edge_location(way)?.map(|location| location.edge_ids))
    }
    /// Return a byte span that can be read with seek + take, without scanning
    /// preceding JSON records. The caller must verify the payload if required.
    pub fn edge_location(&mut self, way: i64) -> Result<Option<WayEdgeLocation>> {
        let (mut low, mut high) = (0, self.count);
        while low < high {
            let mid = low + (high - low) / 2;
            self.file.seek(SeekFrom::Start(16 + WIDTH * mid))?;
            let mut bytes = [0; 48];
            self.file.read_exact(&mut bytes)?;
            let values: [u64; 6] = std::array::from_fn(|i| {
                u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap())
            });
            match values[0].cmp(&key(way)) {
                std::cmp::Ordering::Equal => {
                    let end = values[1]
                        .checked_add(values[2])
                        .filter(|end| *end <= self.edges && values[2] > 0)
                        .ok_or_else(|| invalid("way edge range changed"))?;
                    let byte_end = values[3]
                        .checked_add(values[4])
                        .filter(|end| *end <= self.bytes && values[4] > 0)
                        .ok_or_else(|| invalid("way byte range changed"))?;
                    if values[5] != 0 {
                        return Err(invalid("way edge reserved field changed").into());
                    }
                    return Ok(Some(WayEdgeLocation {
                        edge_ids: values[1]..end,
                        byte_range: values[3]..byte_end,
                    }));
                }
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pbf::{Node, Osm, Tags, Way};
    use crate::staging::connection::tests::{fixture, source, temporary};
    use std::collections::BTreeSet;
    use std::io::BufRead;
    fn expanded_fixture() -> Osm {
        let mut osm = fixture();
        osm.nodes.insert(
            11,
            Node {
                id: 11,
                point: osm.nodes[&9].point,
                tags: Tags::new(),
            },
        );
        for (id, refs, tags) in [
            (
                -50,
                vec![2, 9],
                vec![
                    ("oneway", "-1"),
                    ("maxspeed", "10 mph"),
                    ("maxspeed:backward", "8"),
                    ("name", "南路\n\"桥\""),
                ],
            ),
            (0, vec![2, 9], vec![("junction", "roundabout")]),
            (
                2,
                vec![2, 2, 9],
                vec![
                    ("oneway", "no"),
                    ("access:forward", "no"),
                    ("access:backward", "yes"),
                ],
            ),
            (3, vec![2, 9], vec![("highway", "motorway")]),
            (
                4,
                vec![2, 9, 2],
                vec![
                    ("oneway", "-1"),
                    ("access:forward", "yes"),
                    ("access:backward", "no"),
                ],
            ),
            (5, vec![2], vec![]),
            (
                6,
                vec![2, 9],
                vec![("motorcar:forward", "private"), ("vehicle:forward", "yes")],
            ),
            (10, vec![], vec![]),
            (11, vec![9, 11], vec![]),
        ] {
            let mut map = Tags::from([("highway".into(), "residential".into())]);
            map.extend(tags.into_iter().map(|(k, v)| (k.into(), v.into())));
            osm.ways.insert(
                id,
                Way {
                    id,
                    refs,
                    tags: map,
                },
            );
        }
        osm
    }
    fn inputs(parent: &Path, osm: &Osm) -> (PathBuf, PathBuf) {
        let staged = source(parent, osm);
        let joined = parent.join("join");
        crate::staging::connect(
            &staged,
            &joined,
            StageOptions {
                batch_records: 1,
                ..Default::default()
            },
        )
        .unwrap();
        (staged, joined)
    }
    fn read_edges(dir: &Path, report: &EdgeReport) -> Vec<Edge> {
        assert_eq!(
            hash_file(&dir.join("edges.jsonl")).unwrap(),
            report.files["edges.jsonl"].1
        );
        std::io::BufReader::new(File::open(dir.join("edges.jsonl")).unwrap())
            .lines()
            .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
            .collect()
    }
    fn assert_graph(graph: &routeweft_dataset::Graph, output: &Path, report: &EdgeReport) {
        let actual = read_edges(output, report);
        assert_eq!(
            serde_json::to_value(&actual).unwrap(),
            serde_json::to_value(&graph.edges).unwrap()
        );
        assert_eq!(actual.len() as u64, report.edges);
        let mut ranges = BTreeMap::<i64, Range<u64>>::new();
        for (i, edge) in graph.edges.iter().enumerate() {
            ranges
                .entry(edge.way)
                .and_modify(|range| range.end = i as u64 + 1)
                .or_insert(i as u64..i as u64 + 1);
        }
        let mut index = WayEdgeIndex::open(output).unwrap();
        let mut payload = File::open(output.join("edges.jsonl")).unwrap();
        for (way, range) in &ranges {
            assert_eq!(index.edge_range(*way).unwrap(), Some(range.clone()));
            let location = index.edge_location(*way).unwrap().unwrap();
            assert_eq!(location.edge_ids, *range);
            payload
                .seek(SeekFrom::Start(location.byte_range.start))
                .unwrap();
            let lines = std::io::BufReader::new(
                (&mut payload).take(location.byte_range.end - location.byte_range.start),
            );
            let edges: Vec<Edge> = lines
                .lines()
                .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
                .collect();
            assert_eq!(
                serde_json::to_value(edges).unwrap(),
                serde_json::to_value(&graph.edges[range.start as usize..range.end as usize])
                    .unwrap()
            );
        }
        assert_eq!(report.indexed_ways, ranges.len() as u64);
        assert_eq!(index.edge_range(i64::MIN).unwrap(), None);
        assert_eq!(index.edge_range(i64::MAX).unwrap(), None);
    }
    #[test]
    fn speed_fallback_and_audit_are_revalidated_against_the_staged_source() {
        let parent = temporary();
        let mut osm = expanded_fixture();
        osm.ways
            .get_mut(&-50)
            .unwrap()
            .tags
            .insert("maxspeed".into(), "辅路".into());
        let staged = source(&parent, &osm);
        let joined = parent.join("join");
        let mut connection = crate::staging::connection::connect_excluding(
            &staged,
            &joined,
            Default::default(),
            &BTreeSet::new(),
            crate::InvalidMaxspeedPolicy::UseDefault,
        )
        .unwrap();
        let output = parent.join("edges");
        let report = emit_edges(&staged, &joined, &output, Default::default()).unwrap();
        let (graph, _) = crate::graph_attempt(
            &osm,
            &BTreeSet::new(),
            crate::InvalidMaxspeedPolicy::UseDefault,
            &mut Vec::new(),
        )
        .unwrap();
        assert_graph(&graph, &output, &report);
        connection.maxspeed_diagnostics[0].effective_kmh = Some(99.);
        fs::write(
            joined.join("manifest.json"),
            serde_json::to_vec(&connection).unwrap(),
        )
        .unwrap();
        let rejected = parent.join("rejected");
        let error = emit_edges(&staged, &joined, &rejected, Default::default()).unwrap_err();
        assert!(error
            .to_string()
            .contains("maxspeed audit differs from source"));
        assert!(!rejected.exists());
        connection.maxspeed_policy = crate::InvalidMaxspeedPolicy::Reject;
        fs::write(
            joined.join("manifest.json"),
            serde_json::to_vec(&connection).unwrap(),
        )
        .unwrap();
        assert!(emit_edges(&staged, &joined, &rejected, Default::default()).is_err());
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn streamed_edges_and_ranges_match_graph_order_and_costs() {
        let parent = temporary();
        let osm = expanded_fixture();
        let (staged, joined) = inputs(&parent, &osm);
        let output = parent.join("edges");
        let (graph, stats) =
            crate::graph_attempt(&osm, &BTreeSet::new(), Default::default(), &mut Vec::new())
                .unwrap();
        let report = emit_edges(&staged, &joined, &output, StageOptions::default()).unwrap();
        assert_graph(&graph, &output, &report);
        for (key, value) in &report.diagnostics {
            assert_eq!(stats.get(key), Some(value), "{key}");
        }
        assert_eq!(report.diagnostics["zero_length_segments_omitted"], 2);
        assert!(!report.restrictions_resolved && !report.semantic_validation);
        let mut index = WayEdgeIndex::open(&output).unwrap();
        for way in [1, 4, 5, 7, 8, 10, 11] {
            assert_eq!(index.edge_range(way).unwrap(), None);
        }
        assert_eq!(fs::read_dir(&output).unwrap().count(), 3);
        let again = parent.join("again");
        let second = emit_edges(&staged, &joined, &again, StageOptions::default()).unwrap();
        assert_eq!(report.files, second.files);
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn no_driving_edges_and_invalid_names_match_existing_graph_failure() {
        for invalid_name in [false, true] {
            let parent = temporary();
            let mut osm = fixture();
            if invalid_name {
                osm.ways
                    .get_mut(&-8)
                    .unwrap()
                    .tags
                    .insert("name".into(), "x".repeat(4097));
            } else {
                osm.ways.get_mut(&-8).unwrap().refs = vec![2, 2];
            }
            let expected =
                crate::graph_attempt(&osm, &BTreeSet::new(), Default::default(), &mut Vec::new())
                    .err()
                    .unwrap()
                    .to_string();
            let (staged, joined) = inputs(&parent, &osm);
            assert_eq!(
                emit_edges(
                    &staged,
                    &joined,
                    &parent.join("edges"),
                    StageOptions::default()
                )
                .unwrap_err()
                .to_string(),
                expected
            );
            assert_eq!(fs::read_dir(&parent).unwrap().count(), 2);
            fs::remove_dir_all(parent).unwrap();
        }
    }
    #[test]
    fn budget_failure_is_structured_and_preserves_inputs_and_existing_output() {
        let parent = temporary();
        let (staged, joined) = inputs(&parent, &fixture());
        for (options, resource) in [
            (
                StageOptions {
                    scratch_bytes: 16,
                    ..Default::default()
                },
                "edge_scratch_bytes",
            ),
            (
                StageOptions {
                    run_files: 1,
                    ..Default::default()
                },
                "edge_run_files",
            ),
        ] {
            let error = emit_edges(&staged, &joined, &parent.join("edges"), options).unwrap_err();
            let routeweft_dataset::Error::ResourceLimit(limit) = error.source else {
                panic!("resource error expected")
            };
            assert_eq!(limit.resource, resource);
            assert_eq!(fs::read_dir(&parent).unwrap().count(), 2);
        }
        let out = parent.join("existing");
        fs::create_dir(&out).unwrap();
        fs::write(out.join("sentinel"), b"keep").unwrap();
        assert!(emit_edges(&staged, &joined, &out, StageOptions::default()).is_err());
        assert_eq!(fs::read(out.join("sentinel")).unwrap(), b"keep");
        fs::remove_dir_all(parent).unwrap();
    }
    fn update_descriptor(directory: &Path, name: &str) {
        let mut report: ConnectionReport = manifest(directory).unwrap();
        report.files.insert(
            name.into(),
            (
                fs::metadata(directory.join(name)).unwrap().len(),
                hash_file(&directory.join(name)).unwrap(),
            ),
        );
        fs::write(
            directory.join("manifest.json"),
            serde_json::to_vec(&report).unwrap(),
        )
        .unwrap();
    }
    #[test]
    fn inconsistent_reference_and_profile_rows_fail_even_with_updated_checksum() {
        for file in ["references.run", "ways.run", "nodes.run"] {
            let parent = temporary();
            let (staged, joined) = inputs(&parent, &fixture());
            let path = joined.join(file);
            let mut data = fs::read(&path).unwrap();
            // Alter reference position, direction, or node index respectively.
            data[24..32].copy_from_slice(&u64::MAX.to_le_bytes());
            fs::write(&path, data).unwrap();
            update_descriptor(&joined, file);
            assert!(
                emit_edges(
                    &staged,
                    &joined,
                    &parent.join("edges"),
                    StageOptions::default()
                )
                .is_err(),
                "{file}"
            );
            assert_eq!(fs::read_dir(&parent).unwrap().count(), 2);
            fs::remove_dir_all(parent).unwrap();
        }
    }
    #[test]
    fn corrupt_index_ranges_and_post_open_overflow_are_rejected() {
        let parent = temporary();
        let (staged, joined) = inputs(&parent, &fixture());
        let output = parent.join("edges");
        emit_edges(&staged, &joined, &output, StageOptions::default()).unwrap();
        let mut index = WayEdgeIndex::open(&output).unwrap();
        let path = output.join("way-edges.run");
        let mut bytes = fs::read(&path).unwrap();
        let original = bytes.clone();
        bytes[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
        fs::write(&path, &bytes).unwrap();
        assert!(index.edge_range(-8).is_err());
        assert!(WayEdgeIndex::open(&output).is_err());
        let mut report: EdgeReport = manifest(&output).unwrap();
        report.files.get_mut("way-edges.run").unwrap().1 = hash_file(&path).unwrap();
        fs::write(
            output.join("manifest.json"),
            serde_json::to_vec(&report).unwrap(),
        )
        .unwrap();
        assert!(WayEdgeIndex::open(&output).is_err());
        let mut byte_corruption = original;
        byte_corruption[40..48].copy_from_slice(&u64::MAX.to_le_bytes());
        fs::write(&path, byte_corruption).unwrap();
        report.files.get_mut("way-edges.run").unwrap().1 = hash_file(&path).unwrap();
        fs::write(
            output.join("manifest.json"),
            serde_json::to_vec(&report).unwrap(),
        )
        .unwrap();
        assert!(index.edge_location(-8).is_err());
        assert!(WayEdgeIndex::open(&output).is_err());
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn encoded_edge_record_budget_is_checked_before_writing() {
        let parent = temporary();
        let mut osm = fixture();
        osm.ways
            .get_mut(&-8)
            .unwrap()
            .tags
            .insert("name".into(), "\n".repeat(128));
        let (staged, joined) = inputs(&parent, &osm);
        let source = Source::open(&staged, 4 * 1024 * 1024).unwrap();
        let limit = crate::staging::RunReader::open(&source.run, 4 * 1024 * 1024)
            .unwrap()
            .map(|entry| entry.unwrap().bytes.len() as u64)
            .max()
            .unwrap();
        let error = emit_edges(
            &staged,
            &joined,
            &parent.join("edges"),
            StageOptions {
                record_bytes: limit,
                ..Default::default()
            },
        )
        .unwrap_err();
        let routeweft_dataset::Error::ResourceLimit(limit) = error.source else {
            panic!("resource error expected")
        };
        assert_eq!(limit.resource, "edge_record_bytes");
        assert_eq!(fs::read_dir(&parent).unwrap().count(), 2);
        fs::remove_dir_all(parent).unwrap();
    }
    /// Full-Graph oracle used only in an explicit local acceptance test.
    #[test]
    #[ignore = "requires ROUTEWEFT_EDGES_PBF pointing at a local fixture"]
    fn real_pbf_edges_match_graph_before_static_restrictions() {
        let input = PathBuf::from(std::env::var("ROUTEWEFT_EDGES_PBF").unwrap());
        let parent = temporary();
        let staged = parent.join("stage");
        let joined = parent.join("join");
        let output = parent.join("edges");
        crate::staging::stage(&input, &staged, StageOptions::default()).unwrap();
        crate::staging::connect(
            &staged,
            &joined,
            StageOptions {
                batch_records: 512,
                ..Default::default()
            },
        )
        .unwrap();
        let report = emit_edges(&staged, &joined, &output, StageOptions::default()).unwrap();
        let mut osm = crate::pbf::read_for_driving(&input).unwrap();
        // Keep conditional restrictions (selection), defer all static relations
        // just as emit_edges does; no BlockWays retry has happened at this stage.
        osm.relations.retain(crate::restrictions::conditional);
        let (graph, _) =
            crate::graph_attempt(&osm, &BTreeSet::new(), Default::default(), &mut Vec::new())
                .unwrap();
        assert_graph(&graph, &output, &report);
        println!("{}", serde_json::to_string(&report).unwrap());
        fs::remove_dir_all(parent).unwrap();
    }
}
