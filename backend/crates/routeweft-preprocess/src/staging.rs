//! Bounded external driving pipeline: record staging, graph preparation and v2 build.
//! Budgets cover serialized batches and scratch files, not total process RSS.
mod build;
mod connection;
pub use build::{
    build_external, ExternalBuildControl, ExternalBuildOptions, ExternalBuildResources,
    ExternalBuildResult,
};
pub use connection::{
    connect, emit_edges, index_adjacency, prepare_block_ways, publish_block_ways,
    resolve_restrictions, AccessBudget, AdjacencyReport, BlockWaysOptions, BlockWaysReport,
    ConnectionReport, DiskEdgeAccess, EdgeReport, PublicationReport, RestrictionOptions,
    RestrictionReport, WayEdgeIndex, WayEdgeLocation,
};

use crate::pbf::{self, Node, RecordSink, Relation, Way};
use routeweft_dataset::{hash_file, invalid, resource_limit, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MAGIC: &[u8; 8] = b"RWFTS001";
static NEXT: AtomicU64 = AtomicU64::new(0);

/// Hard limits for serialized records and live temporary run bytes.
#[derive(Clone, Copy)]
pub struct StageOptions {
    pub graph_capacity: routeweft_dataset::GraphCapacity,
    pub batch_bytes: u64,
    pub batch_records: usize,
    pub record_bytes: u64,
    pub scratch_bytes: u64,
    pub run_files: usize,
}
impl Default for StageOptions {
    fn default() -> Self {
        Self {
            graph_capacity: Default::default(),
            batch_bytes: 8 * 1024 * 1024,
            batch_records: 32_768,
            record_bytes: 4 * 1024 * 1024,
            scratch_bytes: 1024 * 1024 * 1024,
            run_files: 64,
        }
    }
}
#[derive(Serialize, Deserialize)]
enum Record {
    Node {
        id: i64,
        lon_bits: u64,
        lat_bits: u64,
        tags: pbf::Tags,
    },
    Way(Way),
    Relation(Relation),
}
impl Record {
    fn key(&self) -> (u8, i64) {
        match self {
            Self::Node { id, .. } => (0, *id),
            Self::Way(v) => (1, v.id),
            Self::Relation(v) => (2, v.id),
        }
    }
}
struct Entry {
    key: (u8, i64),
    bytes: Vec<u8>,
}
struct Run {
    path: PathBuf,
    count: u64,
    bytes: u64,
    digest: String,
}
/// A completed intermediate stage; no graph/reference semantics are implied.
#[derive(Debug, Serialize, Deserialize)]
pub struct StageReport {
    pub format_version: u32,
    pub source_sha256: String,
    pub source_snapshot: Option<i64>,
    pub counts: [u64; 3],
    pub records_sha256: String,
    pub records_bytes: u64,
    pub duplicate_ids_checked: bool,
    pub semantic_validation: bool,
    pub initial_runs: u64,
    pub peak_scratch_bytes: u64,
    pub peak_batch_bytes: u64,
}
struct Stage {
    dir: PathBuf,
    options: StageOptions,
    batch: Vec<Entry>,
    batch_bytes: u64,
    levels: Vec<Option<Run>>,
    serial: u64,
    initial_runs: u64,
    live: u64,
    files: usize,
    peak: u64,
    peak_batch: u64,
    published: bool,
}
impl Drop for Stage {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}
struct LimitedBytes {
    bytes: Vec<u8>,
    limit: u64,
    exceeded: bool,
}
impl Write for LimitedBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.bytes.len() as u64 + bytes.len() as u64 > self.limit {
            self.exceeded = true;
            return Err(std::io::Error::other("staging record budget"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Stage {
    fn add(&mut self, record: Record) -> Result<()> {
        let mut encoded = LimitedBytes {
            bytes: Vec::new(),
            limit: self
                .options
                .record_bytes
                .min(self.options.batch_bytes.saturating_sub(13)),
            exceeded: false,
        };
        let result = serde_json::to_writer(&mut encoded, &record);
        if encoded.exceeded {
            return Err(resource_limit(
                "stage_record_bytes",
                encoded.limit,
                encoded.limit.saturating_add(1),
                "pbf_stage",
            ));
        }
        result.map_err(|e| invalid(e.to_string()))?;
        let size = encoded.bytes.len() as u64 + 13;
        if self.batch_bytes + size > self.options.batch_bytes
            || self.batch.len() == self.options.batch_records
        {
            self.flush()?;
        }
        self.batch_bytes += size;
        self.peak_batch = self.peak_batch.max(self.batch_bytes);
        self.batch.push(Entry {
            key: record.key(),
            bytes: encoded.bytes,
        });
        Ok(())
    }
    fn allocate(&mut self, bytes: u64) -> Result<()> {
        let observed = self
            .live
            .checked_add(bytes)
            .ok_or_else(|| invalid("scratch size overflow"))?;
        if observed > self.options.scratch_bytes {
            return Err(resource_limit(
                "stage_scratch_bytes",
                self.options.scratch_bytes,
                observed,
                "pbf_stage",
            ));
        }
        self.live = observed;
        self.peak = self.peak.max(observed);
        Ok(())
    }
    fn write_run(
        &mut self,
        count: u64,
        entries: impl Iterator<Item = Result<Entry>>,
    ) -> Result<Run> {
        let path = self.dir.join(format!("{}.part", self.serial));
        self.serial += 1;
        if self.files >= self.options.run_files {
            return Err(resource_limit(
                "stage_run_files",
                self.options.run_files as u64,
                self.files as u64 + 1,
                "pbf_stage",
            ));
        }
        self.files += 1;
        self.allocate(16)?;
        let mut out = BufWriter::new(File::options().write(true).create_new(true).open(&path)?);
        out.write_all(MAGIC)?;
        out.write_all(&count.to_le_bytes())?;
        let mut bytes = 16;
        let mut previous = None;
        let mut actual = 0;
        for entry in entries {
            let entry = entry?;
            if previous.is_some_and(|key| key >= entry.key) {
                return Err(invalid(format!(
                    "duplicate or unordered staged object {:?}",
                    entry.key
                )));
            }
            previous = Some(entry.key);
            let size = entry.bytes.len() as u64 + 13;
            self.allocate(size)?;
            out.write_all(&[entry.key.0])?;
            out.write_all(&entry.key.1.to_le_bytes())?;
            out.write_all(&(entry.bytes.len() as u32).to_le_bytes())?;
            out.write_all(&entry.bytes)?;
            bytes += size;
            actual += 1;
        }
        if actual != count {
            return Err(invalid("staging count mismatch"));
        }
        out.flush()?;
        out.get_ref().sync_all()?;
        drop(out);
        let digest = hash_file(&path)?;
        let complete = path.with_extension("run");
        fs::rename(&path, &complete)?;
        Ok(Run {
            path: complete,
            count,
            bytes,
            digest,
        })
    }
    fn merge(&mut self, a: Run, b: Run) -> Result<Run> {
        let mut a_reader = RunReader::open(&a, self.options.record_bytes)?;
        let mut b_reader = RunReader::open(&b, self.options.record_bytes)?;
        let mut left = a_reader.next().transpose()?;
        let mut right = b_reader.next().transpose()?;
        let entries = std::iter::from_fn(|| {
            let (selected, reader) = match (&left, &right) {
                (None, None) => return None,
                (Some(_), None) => (&mut left, &mut a_reader),
                (None, Some(_)) => (&mut right, &mut b_reader),
                (Some(a), Some(b)) if a.key <= b.key => (&mut left, &mut a_reader),
                _ => (&mut right, &mut b_reader),
            };
            let entry = selected.take().expect("selected entry");
            match reader.next().transpose() {
                Ok(next) => {
                    *selected = next;
                    Some(Ok(entry))
                }
                Err(error) => Some(Err(error)),
            }
        });
        let run = self.write_run(a.count + b.count, entries)?;
        fs::remove_file(a.path)?;
        fs::remove_file(b.path)?;
        self.live -= a.bytes + b.bytes;
        self.files -= 2;
        Ok(run)
    }
    fn flush(&mut self) -> Result<()> {
        if self.batch.is_empty() {
            return Ok(());
        }
        let mut batch = std::mem::take(&mut self.batch);
        batch.sort_unstable_by_key(|entry| entry.key);
        self.batch_bytes = 0;
        let mut run = self.write_run(batch.len() as u64, batch.into_iter().map(Ok))?;
        self.initial_runs += 1;
        let mut level = 0;
        loop {
            if level == self.levels.len() {
                self.levels.push(None);
            }
            match self.levels[level].take() {
                Some(other) => run = self.merge(other, run)?,
                None => {
                    self.levels[level] = Some(run);
                    break;
                }
            }
            level += 1;
        }
        Ok(())
    }
}
impl RecordSink for Stage {
    fn node(&mut self, node: Node) -> Result<()> {
        self.add(Record::Node {
            id: node.id,
            lon_bits: node.point.lon.to_bits(),
            lat_bits: node.point.lat.to_bits(),
            tags: node.tags,
        })
    }
    fn way(&mut self, way: Way) -> Result<()> {
        self.add(Record::Way(way))
    }
    fn relation(&mut self, relation: Relation) -> Result<()> {
        self.add(Record::Relation(relation))
    }
}
struct RunReader {
    input: BufReader<File>,
    remaining: u64,
    limit: u64,
}
impl RunReader {
    fn open(run: &Run, limit: u64) -> Result<Self> {
        if fs::metadata(&run.path)?.len() != run.bytes || hash_file(&run.path)? != run.digest {
            return Err(invalid("staging run checksum/length"));
        }
        let mut input = BufReader::new(File::open(&run.path)?);
        let mut header = [0; 16];
        input.read_exact(&mut header)?;
        if &header[..8] != MAGIC || u64::from_le_bytes(header[8..].try_into().unwrap()) != run.count
        {
            return Err(invalid("staging run header"));
        }
        Ok(Self {
            input,
            remaining: run.count,
            limit,
        })
    }
}
impl Iterator for RunReader {
    type Item = Result<Entry>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        Some((|| {
            let mut header = [0; 13];
            self.input.read_exact(&mut header)?;
            let key = (
                header[0],
                i64::from_le_bytes(header[1..9].try_into().unwrap()),
            );
            let len = u32::from_le_bytes(header[9..].try_into().unwrap()) as u64;
            if key.0 > 2 {
                return Err(invalid("staging record header"));
            }
            if len > self.limit {
                return Err(resource_limit(
                    "stage_record_bytes",
                    self.limit,
                    len,
                    "stage_read",
                ));
            }
            let mut bytes = vec![0; len as usize];
            self.input.read_exact(&mut bytes)?;
            Ok(Entry { key, bytes })
        })())
    }
}
/// Decode and globally sort projected OSM records in a new output directory.
/// On failure, only this invocation's private temporary directory is removed.
/// Streaming work limits apply; batches, records and scratch have independent
/// budgets. The input remains capped at 512 MiB. A successful stage validates
/// duplicate IDs, but does not validate references, restrictions or routability.
pub fn stage(input: &Path, output: &Path, options: StageOptions) -> Result<StageReport> {
    if options.batch_bytes < 14
        || options.batch_records == 0
        || options.run_files == 0
        || options.record_bytes == 0
        || options.record_bytes > u32::MAX as u64
    {
        return Err(invalid("invalid staging budgets"));
    }
    if output.exists() {
        return Err(invalid("staging output already exists"));
    }
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let dir = parent.join(format!(
        ".routeweft-stage-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&dir)?;
    let mut stage = Stage {
        dir,
        options,
        batch: Vec::new(),
        batch_bytes: 0,
        levels: Vec::new(),
        serial: 0,
        initial_runs: 0,
        live: 0,
        files: 0,
        peak: 0,
        peak_batch: 0,
        published: false,
    };
    let input_bytes = fs::metadata(input)?.len();
    if input_bytes > 512 * 1024 * 1024 {
        return Err(resource_limit(
            "input_bytes",
            512 * 1024 * 1024,
            input_bytes,
            "pbf_read",
        ));
    }
    let source_sha256 = hash_file(input)?;
    let (counts, source_snapshot) = pbf::read_to_sink(input, &mut stage)?;
    if hash_file(input)? != source_sha256 {
        return Err(invalid("source changed during staging"));
    }
    stage.flush()?;
    let mut final_run = None;
    for run in std::mem::take(&mut stage.levels).into_iter().flatten() {
        final_run = Some(match final_run {
            Some(previous) => stage.merge(previous, run)?,
            None => run,
        });
    }
    let run = final_run.ok_or_else(|| invalid("empty staging output"))?;
    let report = StageReport {
        format_version: 1,
        source_sha256,
        source_snapshot,
        counts,
        records_sha256: run.digest,
        records_bytes: run.bytes,
        duplicate_ids_checked: true,
        semantic_validation: false,
        initial_runs: stage.initial_runs,
        peak_scratch_bytes: stage.peak,
        peak_batch_bytes: stage.peak_batch,
    };
    fs::rename(run.path, stage.dir.join("records.run"))?;
    let mut manifest = File::options()
        .write(true)
        .create_new(true)
        .open(stage.dir.join("manifest.json"))?;
    serde_json::to_writer(&mut manifest, &report).map_err(|e| invalid(e.to_string()))?;
    manifest.sync_all()?;
    File::open(&stage.dir)?.sync_all()?;
    // create_dir reserves the destination against clobbering another publisher.
    fs::create_dir(output)?;
    if let Err(error) = fs::rename(&stage.dir, output) {
        let _ = fs::remove_dir(output);
        return Err(error.into());
    }
    stage.published = true;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn temporary() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "routeweft-stage-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        p
    }
    fn state(dir: PathBuf, options: StageOptions) -> Stage {
        Stage {
            dir,
            options,
            batch: Vec::new(),
            batch_bytes: 0,
            levels: Vec::new(),
            serial: 0,
            initial_runs: 0,
            live: 0,
            files: 0,
            peak: 0,
            peak_batch: 0,
            published: false,
        }
    }
    fn node(id: i64) -> Node {
        Node {
            id,
            point: routeweft_dataset::Point {
                lon: 108.0,
                lat: 22.0,
            },
            tags: Default::default(),
        }
    }
    fn assert_projection(input: &Path, output: &Path, report: &StageReport) {
        let full = pbf::read_for_driving(input).unwrap();
        let mut expected = Vec::new();
        for (_, n) in full.nodes {
            expected.push(Record::Node {
                id: n.id,
                lon_bits: n.point.lon.to_bits(),
                lat_bits: n.point.lat.to_bits(),
                tags: n.tags,
            });
        }
        expected.extend(full.ways.into_values().map(Record::Way));
        expected.extend(full.relations.into_iter().map(Record::Relation));
        expected.sort_unstable_by_key(Record::key);
        let run = Run {
            path: output.join("records.run"),
            count: report.counts.iter().sum(),
            bytes: report.records_bytes,
            digest: report.records_sha256.clone(),
        };
        let entries = RunReader::open(&run, StageOptions::default().record_bytes)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(entries.len(), expected.len());
        for (entry, record) in entries.iter().zip(expected) {
            assert_eq!(entry.key, record.key());
            assert_eq!(entry.bytes, serde_json::to_vec(&record).unwrap());
        }
    }
    #[test]
    fn pbf_decode_to_published_stage_matches_in_memory_projection() {
        fn bytes(field: u8, payload: &[u8]) -> Vec<u8> {
            assert!(payload.len() < 128);
            [vec![field * 8 + 2, payload.len() as u8], payload.to_vec()].concat()
        }
        fn block(kind: &[u8], raw: &[u8]) -> Vec<u8> {
            let blob = bytes(1, raw);
            let header = [bytes(1, kind), vec![24, blob.len() as u8]].concat();
            [(header.len() as u32).to_be_bytes().to_vec(), header, blob].concat()
        }
        let parent = temporary();
        let input = parent.join("input.pbf");
        let output = parent.join("out");
        let table = [
            bytes(1, b""),
            bytes(1, b"highway"),
            bytes(1, b"residential"),
        ]
        .concat();
        let node1 = bytes(1, &[8, 2, 64, 0, 72, 0]);
        let node2 = bytes(1, &[8, 4, 64, 0, 72, 2]);
        let way = bytes(3, &[8, 1, 18, 1, 1, 26, 1, 2, 66, 2, 2, 2]);
        let raw = [bytes(1, &table), bytes(2, &[node2, node1, way].concat())].concat();
        fs::write(
            &input,
            [
                block(b"OSMHeader", &bytes(4, b"OsmSchema-V0.6")),
                block(b"OSMData", &raw),
            ]
            .concat(),
        )
        .unwrap();
        let report = stage(
            &input,
            &output,
            StageOptions {
                batch_records: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(report.counts, [2, 1, 0]);
        assert!(report.duplicate_ids_checked);
        assert!(!report.semantic_validation);
        assert_projection(&input, &output, &report);
        assert_eq!(fs::read_dir(&output).unwrap().count(), 2);
        let again = parent.join("again");
        let second = stage(&input, &again, StageOptions::default()).unwrap();
        assert_eq!(report.records_sha256, second.records_sha256);
        // A corrupt source must clean the private directory, leaving the two
        // successful outputs and the input untouched.
        fs::write(&input, b"invalid").unwrap();
        assert!(stage(&input, &parent.join("failed"), StageOptions::default()).is_err());
        assert_eq!(fs::read_dir(&parent).unwrap().count(), 3);
        fs::remove_dir_all(parent).unwrap();
    }
    /// Explicit local acceptance hook; no real fixture is bundled in the crate.
    #[test]
    #[ignore = "requires ROUTEWEFT_STAGE_PBF pointing at a local acceptance fixture"]
    fn real_pbf_projection_equivalence() {
        let input = PathBuf::from(std::env::var("ROUTEWEFT_STAGE_PBF").unwrap());
        let parent = temporary();
        let output = parent.join("out");
        let report = stage(
            &input,
            &output,
            StageOptions {
                batch_records: 1024,
                ..Default::default()
            },
        )
        .unwrap();
        assert_projection(&input, &output, &report);
        println!("{}", serde_json::to_string(&report).unwrap());
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn multipass_sort_preserves_payload_and_bounds() {
        let dir = temporary();
        let options = StageOptions {
            batch_records: 2,
            batch_bytes: 1024,
            ..Default::default()
        };
        let mut stage = state(dir.clone(), options);
        for id in (0..19).rev() {
            stage.node(node(id)).unwrap();
        }
        stage.flush().unwrap();
        let mut final_run = None;
        for run in std::mem::take(&mut stage.levels).into_iter().flatten() {
            final_run = Some(match final_run {
                Some(a) => stage.merge(a, run).unwrap(),
                None => run,
            });
        }
        let run = final_run.unwrap();
        assert_eq!(stage.initial_runs, 10);
        assert!(stage.peak_batch <= options.batch_bytes);
        assert!(stage.peak <= options.scratch_bytes);
        assert_eq!(stage.live, run.bytes);
        let entries = RunReader::open(&run, options.record_bytes)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        for (id, entry) in entries.iter().enumerate() {
            assert_eq!(entry.key, (0, id as i64));
            let record: Record = serde_json::from_slice(&entry.bytes).unwrap();
            assert_eq!(record.key(), entry.key);
            let Record::Node { lon_bits, .. } = record else {
                panic!("node expected")
            };
            assert_eq!(lon_bits, 108.0_f64.to_bits());
        }
        drop(stage);
        assert!(!dir.exists());
    }
    #[test]
    fn duplicate_ids_all_types_across_runs_and_same_batch() {
        for kind in 0..3 {
            for batch_records in [1, 10] {
                let dir = temporary();
                let mut stage = state(
                    dir.clone(),
                    StageOptions {
                        batch_records,
                        ..Default::default()
                    },
                );
                let record = || match kind {
                    0 => Record::Node {
                        id: -7,
                        lon_bits: 0,
                        lat_bits: 0,
                        tags: Default::default(),
                    },
                    1 => Record::Way(Way {
                        id: -7,
                        refs: vec![1, 2],
                        tags: Default::default(),
                    }),
                    _ => Record::Relation(Relation {
                        id: -7,
                        members: vec![],
                        tags: Default::default(),
                    }),
                };
                stage.add(record()).unwrap();
                let result = stage.add(record()).and_then(|()| stage.flush());
                assert!(result.is_err());
                drop(stage);
                assert!(!dir.exists());
            }
        }
    }
    #[test]
    fn file_budget_is_enforced_and_id_namespaces_are_independent() {
        let dir = temporary();
        let mut limited = state(
            dir.clone(),
            StageOptions {
                batch_records: 1,
                run_files: 1,
                ..Default::default()
            },
        );
        limited.node(node(1)).unwrap();
        limited.node(node(2)).unwrap();
        assert!(limited.flush().is_err());
        drop(limited);
        assert!(!dir.exists());
        let mut stage = state(temporary(), StageOptions::default());
        stage.node(node(1)).unwrap();
        stage
            .way(Way {
                id: 1,
                refs: vec![1],
                tags: Default::default(),
            })
            .unwrap();
        stage
            .relation(Relation {
                id: 1,
                members: vec![],
                tags: Default::default(),
            })
            .unwrap();
        stage.flush().unwrap();
        assert_eq!(stage.levels[0].as_ref().unwrap().count, 3);
    }
    #[test]
    fn scratch_record_budgets_and_corruption_fail_closed() {
        let dir = temporary();
        let mut stage = state(
            dir.clone(),
            StageOptions {
                scratch_bytes: 16,
                ..Default::default()
            },
        );
        stage.node(node(1)).unwrap();
        assert!(stage.flush().is_err());
        drop(stage);
        assert!(!dir.exists());
        let dir = temporary();
        let mut stage = state(
            dir.clone(),
            StageOptions {
                record_bytes: 1,
                ..Default::default()
            },
        );
        assert!(stage.node(node(1)).is_err());
        drop(stage);
        assert!(!dir.exists());
        let dir = temporary();
        let mut stage = state(dir, StageOptions::default());
        stage.node(node(1)).unwrap();
        stage.flush().unwrap();
        let run = stage.levels[0].as_ref().unwrap();
        fs::write(&run.path, b"corrupt").unwrap();
        assert!(RunReader::open(run, stage.options.record_bytes).is_err());
    }
    #[test]
    fn failure_cleans_private_directory_and_preserves_existing_output() {
        let parent = temporary();
        let output = parent.join("out");
        assert!(stage(
            &parent.join("missing.pbf"),
            &output,
            StageOptions::default()
        )
        .is_err());
        assert_eq!(fs::read_dir(&parent).unwrap().count(), 0);
        fs::create_dir(&output).unwrap();
        fs::write(output.join("sentinel"), b"keep").unwrap();
        assert!(stage(
            &parent.join("missing.pbf"),
            &output,
            StageOptions::default()
        )
        .is_err());
        assert_eq!(fs::read(output.join("sentinel")).unwrap(), b"keep");
        fs::remove_dir_all(parent).unwrap();
    }
}
