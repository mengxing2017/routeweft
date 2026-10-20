//! Immutable, disk-backed intermediate edge access; not a restriction-resolved graph.
use super::super::{Cursor, Row, Sorter, Workspace, Writer, WIDTH};
use super::{descriptor, manifest, regular, EdgeReport};
use crate::staging::{StageOptions, NEXT};
use crate::Result;
use routeweft_dataset::{hash_file, invalid, resource_limit, Edge};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::Ordering;

/// Intermediate index identity. Static restrictions and BlockWays remain unresolved.
#[derive(Debug, Serialize, Deserialize)]
pub struct AdjacencyReport {
    pub format_version: u32,
    pub edge_manifest_sha256: String,
    pub nodes: u64,
    pub edges: u64,
    pub edge_bytes: u64,
    pub record_bytes: u64,
    pub files: BTreeMap<String, (u64, String)>,
    pub peak_scratch_bytes: u64,
    pub peak_batch_bytes: u64,
}

/// Shared, cumulative byte allowance for a sequence of disk queries.
/// Opening verifies checksums and structure separately from this query allowance.
pub struct AccessBudget {
    remaining: u64,
}
impl AccessBudget {
    pub fn new(bytes: u64) -> Self {
        Self { remaining: bytes }
    }
    pub fn remaining(&self) -> u64 {
        self.remaining
    }
    pub(super) fn charge(&mut self, bytes: u64) -> Result<()> {
        if bytes > self.remaining {
            return Err(resource_limit(
                "edge_access_bytes",
                self.remaining,
                bytes,
                "external_adjacency_query",
            )
            .into());
        }
        self.remaining -= bytes;
        Ok(())
    }
}
fn edge_record(input: &mut impl BufRead, limit: u64) -> Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    loop {
        let available = input.fill_buf()?;
        if available.is_empty() {
            if bytes.is_empty() {
                return Ok(None);
            }
            return Err(invalid("unterminated edge record").into());
        }
        let take = available
            .iter()
            .position(|b| *b == b'\n')
            .map_or(available.len(), |n| n + 1);
        let observed = bytes.len() as u64 + take as u64;
        if observed > limit {
            return Err(resource_limit(
                "adjacency_record_bytes",
                limit,
                observed,
                "external_adjacency",
            )
            .into());
        }
        let done = available[take - 1] == b'\n';
        bytes.extend_from_slice(&available[..take]);
        input.consume(take);
        if done {
            return Ok(Some(bytes));
        }
    }
}
fn decode(bytes: &[u8], nodes: u64) -> Result<Edge> {
    let edge: Edge = serde_json::from_slice(bytes).map_err(|e| invalid(e.to_string()))?;
    if edge.from as u64 >= nodes
        || edge.to as u64 >= nodes
        || edge.duration_ms == 0
        || edge.distance_mm == 0
        || edge.name.len() > 4096
        || edge.class.len() > 64
    {
        return Err(invalid("adjacency edge fields").into());
    }
    Ok(edge)
}
fn source(directory: &Path) -> Result<(EdgeReport, String, u64)> {
    let report: EdgeReport = manifest(directory)?;
    let digest = hash_file(&directory.join("manifest.json"))?;
    if report.format_version != 1
        || report.profile != "driving-v1"
        || report.nodes == 0
        || report.nodes > 1_000_000
        || report.edges == 0
        || report.edges > 2_000_000
        || report.indexed_ways > report.edges
        || report.files.len() != 2
        || report.restrictions_resolved
        || report.semantic_validation
    {
        return Err(invalid("adjacency source manifest").into());
    }
    let (size, checksum) = report
        .files
        .get("edges.jsonl")
        .ok_or_else(|| invalid("edge payload descriptor"))?;
    let path = directory.join("edges.jsonl");
    regular(&path)?;
    if fs::metadata(&path)?.len() != *size || hash_file(&path)? != *checksum {
        return Err(invalid("edge payload checksum/length").into());
    }
    let size = *size;
    Ok((report, digest, size))
}
/// Build fixed-width edge offsets and outgoing rows sorted by (node, edge ID).
/// Scratch and sort batches follow StageOptions; no whole graph is retained.
pub fn index_adjacency(
    input: &Path,
    output: &Path,
    options: StageOptions,
) -> Result<AdjacencyReport> {
    if options.record_bytes == 0
        || options.record_bytes > u32::MAX as u64
        || options.batch_records == 0
        || options.batch_bytes < WIDTH
        || options.run_files == 0
    {
        return Err(invalid("adjacency options").into());
    }
    if output.exists() {
        return Err(invalid("adjacency output exists").into());
    }
    let (report, digest, edge_bytes) = source(input)?;
    let way_run = descriptor(input, &report.files, "way-edges.run", report.indexed_ways)?;
    let mut ways = Cursor::open(&way_run)?;
    let mut way = ways.next()?;
    let mut previous_way = None;
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let dir = parent.join(format!(
        ".routeweft-adjacency-{}-{}",
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
        scope: "external_adjacency",
    };
    let mut offsets = Writer::new(&mut work)?;
    let mut outgoing = Sorter::default();
    let mut input_file = BufReader::new(File::open(input.join("edges.jsonl"))?);
    let mut position = 0;
    for id in 0..report.edges {
        let row = way.ok_or_else(|| invalid("missing way range"))?;
        let end = row[1]
            .checked_add(row[2])
            .ok_or_else(|| invalid("way range overflow"))?;
        if id == row[1] && (row[3] != position || previous_way.is_some_and(|p| p >= row[0])) {
            return Err(invalid("way range order/offset").into());
        }
        if row[2] == 0
            || row[4] == 0
            || row[5] != 0
            || id < row[1]
            || id >= end
            || end > report.edges
        {
            return Err(invalid("way range bounds").into());
        }
        let bytes = edge_record(&mut input_file, options.record_bytes)?
            .ok_or_else(|| invalid("missing edge record"))?;
        let edge = decode(&bytes, report.nodes)?;
        if super::super::key(edge.way) != row[0] {
            return Err(invalid("way edge identity").into());
        }
        offsets.push(
            [
                position,
                bytes.len() as u64,
                edge.from as u64,
                edge.to as u64,
                0,
                0,
            ],
            &mut work,
        )?;
        outgoing.push([edge.from as u64, id, 0, 0, 0, 0], &mut work)?;
        position += bytes.len() as u64;
        if id + 1 == end {
            if row[3].checked_add(row[4]) != Some(position) {
                return Err(invalid("way byte range").into());
            }
            previous_way = Some(row[0]);
            way = ways.next()?;
            if way.is_some_and(|next| next[1] != id + 1) {
                return Err(invalid("way range gap").into());
            }
        }
    }
    if way.is_some()
        || position != edge_bytes
        || edge_record(&mut input_file, options.record_bytes)?.is_some()
    {
        return Err(invalid("trailing adjacency input").into());
    }
    let offsets = offsets.finish()?;
    let outgoing = outgoing.finish(&mut work)?;
    let mut rows = Cursor::open(&outgoing)?;
    let mut row = rows.next()?;
    let mut starts = Writer::new(&mut work)?;
    let mut start = 0;
    for node in 0..report.nodes {
        let begin = start;
        while row.is_some_and(|r| r[0] == node) {
            start += 1;
            row = rows.next()?;
        }
        starts.push([begin, start - begin, 0, 0, 0, 0], &mut work)?;
    }
    if row.is_some() || start != report.edges {
        return Err(invalid("outgoing node bounds").into());
    }
    let starts = starts.finish()?;
    let (_, final_digest, _) = source(input)?;
    if final_digest != digest || hash_file(&way_run.path)? != way_run.digest {
        return Err(invalid("adjacency source changed").into());
    }
    let mut result = AdjacencyReport {
        format_version: 1,
        edge_manifest_sha256: digest,
        nodes: report.nodes,
        edges: report.edges,
        edge_bytes,
        record_bytes: options.record_bytes,
        files: BTreeMap::new(),
        peak_scratch_bytes: work.peak,
        peak_batch_bytes: work.peak_batch,
    };
    for (name, run) in [
        ("edge-offsets.run", offsets),
        ("outgoing.run", outgoing),
        ("node-ranges.run", starts),
    ] {
        result.files.insert(name.into(), (run.bytes, run.digest));
        fs::rename(run.path, work.dir.join(name))?;
    }
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

/// Verified immutable intermediate access. Keep both directories immutable while open.
/// Outgoing traversal streams IDs; allocation is bounded by a single edge record.
pub struct DiskEdgeAccess {
    report: AdjacencyReport,
    offsets: File,
    outgoing: File,
    ranges: File,
    payload: File,
}
fn read_row(file: &mut File, index: u64, budget: &mut AccessBudget) -> Result<Row> {
    budget.charge(WIDTH)?;
    let offset = index
        .checked_mul(WIDTH)
        .and_then(|n| n.checked_add(16))
        .ok_or_else(|| invalid("adjacency offset overflow"))?;
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = [0; 48];
    file.read_exact(&mut bytes)?;
    Ok(std::array::from_fn(|i| {
        u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap())
    }))
}
impl DiskEdgeAccess {
    pub fn open(edges: &Path, index: &Path) -> Result<Self> {
        let (source, digest, bytes) = source(edges)?;
        let report: AdjacencyReport = manifest(index)?;
        if report.format_version != 1
            || report.edge_manifest_sha256 != digest
            || report.nodes != source.nodes
            || report.edges != source.edges
            || report.edge_bytes != bytes
            || report.files.len() != 3
            || report.record_bytes == 0
            || report.record_bytes > u32::MAX as u64
        {
            return Err(invalid("adjacency index identity").into());
        }
        let offsets = descriptor(index, &report.files, "edge-offsets.run", report.edges)?;
        let outgoing = descriptor(index, &report.files, "outgoing.run", report.edges)?;
        let ranges = descriptor(index, &report.files, "node-ranges.run", report.nodes)?;
        let mut result = Self {
            report,
            offsets: File::open(offsets.path)?,
            outgoing: File::open(outgoing.path)?,
            ranges: File::open(ranges.path)?,
            payload: File::open(edges.join("edges.jsonl"))?,
        };
        // Check structure independently of supplied checksums, including every edge association.
        let mut budget = AccessBudget::new(u64::MAX);
        let mut position = 0;
        for id in 0..result.report.edges {
            let row = read_row(&mut result.offsets, id, &mut budget)?;
            if row[0] != position {
                return Err(invalid("edge offset gap").into());
            }
            result.edge(id, &mut budget)?;
            position = row[0] + row[1];
        }
        if position != bytes {
            return Err(invalid("edge offset coverage").into());
        }
        let mut start = 0;
        for node in 0..result.report.nodes {
            let row = read_row(&mut result.ranges, node, &mut budget)?;
            if row[0] != start {
                return Err(invalid("node range gap").into());
            }
            let mut previous = None;
            result.outgoing(node, &mut budget, |id| {
                if previous.is_some_and(|p| p >= id) {
                    return Err(invalid("outgoing order").into());
                }
                previous = Some(id);
                Ok(())
            })?;
            for at in row[0]..row[0] + row[1] {
                let entry = read_row(&mut result.outgoing, at, &mut budget)?;
                let edge = read_row(&mut result.offsets, entry[1], &mut budget)?;
                if edge[2] != node {
                    return Err(invalid("outgoing edge source").into());
                }
            }
            start += row[1];
        }
        if start != result.report.edges {
            return Err(invalid("outgoing coverage").into());
        }
        Ok(result)
    }
    pub fn edge(&mut self, id: u64, budget: &mut AccessBudget) -> Result<Edge> {
        if id >= self.report.edges {
            return Err(invalid("edge ID bounds").into());
        }
        let row = read_row(&mut self.offsets, id, budget)?;
        if row[1] == 0
            || row[1] > self.report.record_bytes
            || row[0]
                .checked_add(row[1])
                .is_none_or(|end| end > self.report.edge_bytes)
            || row[4] != 0
            || row[5] != 0
        {
            return Err(invalid("edge offset bounds").into());
        }
        budget.charge(row[1])?;
        let mut bytes = vec![0; usize::try_from(row[1]).map_err(|_| invalid("edge record size"))?];
        self.payload.seek(SeekFrom::Start(row[0]))?;
        self.payload.read_exact(&mut bytes)?;
        if bytes.last() != Some(&b'\n') {
            return Err(invalid("edge newline").into());
        }
        let edge = decode(&bytes, self.report.nodes)?;
        if edge.from as u64 != row[2] || edge.to as u64 != row[3] {
            return Err(invalid("edge endpoint index").into());
        }
        Ok(edge)
    }
    /// Return one outgoing edge ID without materializing the node's adjacency.
    pub fn outgoing_edge_at(
        &mut self,
        node: u64,
        at: u64,
        budget: &mut AccessBudget,
    ) -> Result<Option<u64>> {
        if node >= self.report.nodes {
            return Err(invalid("outgoing node bounds").into());
        }
        let row = read_row(&mut self.ranges, node, budget)?;
        let end = row[0]
            .checked_add(row[1])
            .filter(|end| *end <= self.report.edges)
            .ok_or_else(|| invalid("outgoing range bounds"))?;
        if row[2..] != [0; 4] {
            return Err(invalid("node reserved fields").into());
        }
        if at >= row[1] {
            return Ok(None);
        }
        let position = row[0]
            .checked_add(at)
            .filter(|p| *p < end)
            .ok_or_else(|| invalid("outgoing position"))?;
        let row = read_row(&mut self.outgoing, position, budget)?;
        if row[0] != node || row[1] >= self.report.edges || row[2..] != [0; 4] {
            return Err(invalid("outgoing row").into());
        }
        Ok(Some(row[1]))
    }
    pub fn outgoing(
        &mut self,
        node: u64,
        budget: &mut AccessBudget,
        mut visit: impl FnMut(u64) -> Result<()>,
    ) -> Result<()> {
        if node >= self.report.nodes {
            return Err(invalid("outgoing node bounds").into());
        }
        let row = read_row(&mut self.ranges, node, budget)?;
        let end = row[0]
            .checked_add(row[1])
            .filter(|end| *end <= self.report.edges)
            .ok_or_else(|| invalid("outgoing range bounds"))?;
        if row[2..] != [0; 4] {
            return Err(invalid("node reserved fields").into());
        }
        for at in row[0]..end {
            let row = read_row(&mut self.outgoing, at, budget)?;
            if row[0] != node || row[1] >= self.report.edges || row[2..] != [0; 4] {
                return Err(invalid("outgoing row").into());
            }
            visit(row[1])?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::staging::connection::tests::{fixture, source, temporary};
    use crate::staging::{connect, emit_edges};

    fn input(parent: &Path) -> std::path::PathBuf {
        let mut osm = fixture();
        // Force many sort runs and a high-degree node, including signed way IDs.
        let tags = osm.ways[&-8].tags.clone();
        for id in 20..120 {
            let way = crate::pbf::Way {
                id,
                refs: vec![2, 9],
                tags: tags.clone(),
            };
            osm.ways.insert(id, way);
        }
        let staged = source(parent, &osm);
        let joined = parent.join("joined");
        connect(&staged, &joined, StageOptions::default()).unwrap();
        let edges = parent.join("edges");
        emit_edges(&staged, &joined, &edges, StageOptions::default()).unwrap();
        edges
    }
    #[test]
    fn random_edges_and_streamed_outgoing_match_payload_across_runs() {
        let parent = temporary();
        let edges = input(&parent);
        let output = parent.join("adjacency");
        let report = index_adjacency(
            &edges,
            &output,
            StageOptions {
                batch_records: 3,
                batch_bytes: 144,
                ..StageOptions::default()
            },
        )
        .unwrap();
        let expected: Vec<Edge> = BufReader::new(File::open(edges.join("edges.jsonl")).unwrap())
            .lines()
            .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
            .collect();
        let mut access = DiskEdgeAccess::open(&edges, &output).unwrap();
        let mut budget = AccessBudget::new(u64::MAX);
        for (id, edge) in expected.iter().enumerate().rev() {
            assert_eq!(&access.edge(id as u64, &mut budget).unwrap(), edge);
        }
        for node in 0..report.nodes {
            let mut ids = Vec::new();
            access
                .outgoing(node, &mut budget, |id| {
                    ids.push(id);
                    Ok(())
                })
                .unwrap();
            assert_eq!(
                ids,
                expected
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| e.from as u64 == node)
                    .map(|(id, _)| id as u64)
                    .collect::<Vec<_>>()
            );
        }
        assert!(report.peak_batch_bytes <= 144);
        let mut small = AccessBudget::new(48);
        assert!(access
            .edge(0, &mut small)
            .unwrap_err()
            .to_string()
            .contains("edge_access_bytes"));
        assert_eq!(small.remaining(), 0);
        assert!(access
            .outgoing(0, &mut small, |_| panic!("budget must precede callback"))
            .is_err());
        assert!(access.edge(u64::MAX, &mut budget).is_err());
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn scratch_and_record_failures_cleanup_without_publishing() {
        let parent = temporary();
        let edges = input(&parent);
        for (name, options, resource) in [
            (
                "scratch",
                StageOptions {
                    scratch_bytes: 16,
                    ..StageOptions::default()
                },
                "adjacency_scratch_bytes",
            ),
            (
                "record",
                StageOptions {
                    record_bytes: 10,
                    ..StageOptions::default()
                },
                "adjacency_record_bytes",
            ),
            (
                "files",
                StageOptions {
                    run_files: 1,
                    ..StageOptions::default()
                },
                "adjacency_run_files",
            ),
        ] {
            let output = parent.join(name);
            assert!(index_adjacency(&edges, &output, options)
                .unwrap_err()
                .to_string()
                .contains(resource));
            assert!(!output.exists());
            assert!(!fs::read_dir(&parent).unwrap().any(|e| e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".routeweft-adjacency")));
        }
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn resealed_outgoing_and_node_ranges_are_semantically_checked() {
        let parent = temporary();
        let edges = input(&parent);
        let output = parent.join("adjacency");
        let mut report = index_adjacency(&edges, &output, StageOptions::default()).unwrap();
        for (name, column, value) in [
            ("outgoing.run", 1, report.edges),
            ("outgoing.run", 2, 1),
            ("node-ranges.run", 0, 1),
            ("node-ranges.run", 1, u64::MAX),
            ("node-ranges.run", 5, 1),
            ("edge-offsets.run", 2, report.nodes),
            ("edge-offsets.run", 1, u64::MAX),
        ] {
            let path = output.join(name);
            let original = fs::read(&path).unwrap();
            let mut broken = original.clone();
            broken[16 + column * 8..24 + column * 8].copy_from_slice(&value.to_le_bytes());
            fs::write(&path, broken).unwrap();
            report.files.get_mut(name).unwrap().1 = hash_file(&path).unwrap();
            fs::write(
                output.join("manifest.json"),
                serde_json::to_vec(&report).unwrap(),
            )
            .unwrap();
            assert!(
                DiskEdgeAccess::open(&edges, &output).is_err(),
                "{name} column {column}"
            );
            fs::write(&path, original).unwrap();
            report.files.get_mut(name).unwrap().1 = hash_file(&path).unwrap();
        }
        fs::write(
            output.join("manifest.json"),
            serde_json::to_vec(&report).unwrap(),
        )
        .unwrap();
        DiskEdgeAccess::open(&edges, &output).unwrap();
        assert!(index_adjacency(&edges, &output, StageOptions::default()).is_err());
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn resealed_bad_offsets_and_modified_payload_are_rejected() {
        let parent = temporary();
        let edges = input(&parent);
        let output = parent.join("adjacency");
        let mut report = index_adjacency(&edges, &output, StageOptions::default()).unwrap();
        let path = output.join("edge-offsets.run");
        let original = fs::read(&path).unwrap();
        let mut broken = original.clone();
        broken[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
        fs::write(&path, broken).unwrap();
        report.files.get_mut("edge-offsets.run").unwrap().1 = hash_file(&path).unwrap();
        fs::write(
            output.join("manifest.json"),
            serde_json::to_vec(&report).unwrap(),
        )
        .unwrap();
        assert!(DiskEdgeAccess::open(&edges, &output).is_err());
        fs::write(&path, original).unwrap();
        report.files.get_mut("edge-offsets.run").unwrap().1 = hash_file(&path).unwrap();
        fs::write(
            output.join("manifest.json"),
            serde_json::to_vec(&report).unwrap(),
        )
        .unwrap();
        let mut opened = DiskEdgeAccess::open(&edges, &output).unwrap();
        let mut file = File::options().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(16)).unwrap();
        file.write_all(&u64::MAX.to_le_bytes()).unwrap();
        assert!(opened.edge(0, &mut AccessBudget::new(u64::MAX)).is_err());
        fs::write(edges.join("edges.jsonl"), b"tampered").unwrap();
        assert!(DiskEdgeAccess::open(&edges, &output).is_err());
        fs::remove_dir_all(parent).unwrap();
    }
}
