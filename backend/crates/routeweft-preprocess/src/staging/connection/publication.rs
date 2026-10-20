//! Convert completed, authenticated external graph artifacts to a final v2 dataset.
use super::BlockWaysReport;
use crate::{InputReport, InvalidRestrictionPolicy, Result};
use routeweft_dataset::{
    hash, hash_file, invalid, write_binary_streaming, BinaryMetadata, BinarySource,
    BinaryWriteOptions, Edge, Manifest, Point, Result as DataResult,
};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

const FILES: [&str; 13] = [
    "graph/connection/manifest.json",
    "graph/connection/nodes.run",
    "graph/connection/references.run",
    "graph/connection/ways.run",
    "graph/edges/manifest.json",
    "graph/edges/edges.jsonl",
    "graph/edges/way-edges.run",
    "graph/adjacency/manifest.json",
    "graph/adjacency/edge-offsets.run",
    "graph/adjacency/outgoing.run",
    "graph/adjacency/node-ranges.run",
    "graph/restrictions/manifest.json",
    "graph/restrictions/forbidden.jsonl",
];
/// Complete final dataset plus the conservative exclusions' audit trail.
#[derive(Serialize)]
pub struct PublicationReport {
    pub manifest: Manifest,
    pub source_manifest_sha256: String,
    pub input: InputReport,
}
struct Source {
    directory: PathBuf,
    report: BlockWaysReport,
    manifest_sha256: String,
}
fn regular(path: &Path) -> DataResult<File> {
    if !fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(invalid("publication input must be a regular file"));
    }
    Ok(File::open(path)?)
}
impl Source {
    fn open(directory: &Path) -> DataResult<Self> {
        let mut bytes = Vec::new();
        regular(&directory.join("manifest.json"))?
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 1024 * 1024 {
            return Err(invalid("publication source manifest size"));
        }
        let report: BlockWaysReport =
            serde_json::from_slice(&bytes).map_err(|e| invalid(e.to_string()))?;
        let defaults = routeweft_dataset::LoadOptions::default();
        if report.format_version != 1
            || !report.restrictions_resolved
            || report.nodes == 0
            || report.nodes > defaults.max_nodes as u64
            || report.edges > defaults.max_edges as u64
            || report.restrictions > defaults.max_restriction_edges as u64 / 2
            || report.files.len() != FILES.len()
            || FILES.iter().any(|name| !report.files.contains_key(*name))
            || (report.policy == InvalidRestrictionPolicy::Reject
                && (!report.blocked_way_ids.is_empty()
                    || !report.restriction_diagnostics.is_empty()
                    || report.attempts != 1))
            || report.blocked_way_ids.len() > 32768
            || report.restriction_diagnostics.len() > 256
            || report.blocked_way_ids.windows(2).any(|w| w[0] >= w[1])
        {
            return Err(invalid("publication requires a completed BlockWays source"));
        }
        Ok(Self {
            directory: directory.into(),
            report,
            manifest_sha256: hash(&bytes),
        })
    }
    fn bindings(&self) -> DataResult<()> {
        fn read<T: serde::de::DeserializeOwned>(path: &Path) -> DataResult<T> {
            let file = regular(path)?;
            if file.metadata()?.len() > 1024 * 1024 {
                return Err(invalid("publication child manifest size"));
            }
            serde_json::from_reader(file.take(1024 * 1024 + 1)).map_err(|e| invalid(e.to_string()))
        }
        let connection: super::ConnectionReport =
            read(&self.directory.join("graph/connection/manifest.json"))?;
        let edges: super::EdgeReport = read(&self.directory.join("graph/edges/manifest.json"))?;
        let adjacency: super::AdjacencyReport =
            read(&self.directory.join("graph/adjacency/manifest.json"))?;
        let restrictions: super::RestrictionReport =
            read(&self.directory.join("graph/restrictions/manifest.json"))?;
        let m = &self.report;
        for (prefix, files, expected) in [
            ("graph/connection", &connection.files, 3),
            ("graph/edges", &edges.files, 2),
            ("graph/adjacency", &adjacency.files, 3),
            ("graph/restrictions", &restrictions.files, 1),
        ] {
            if files.len() != expected
                || files
                    .iter()
                    .any(|(name, value)| m.files.get(&format!("{prefix}/{name}")) != Some(value))
            {
                return Err(invalid("publication child descriptors"));
            }
        }
        let mut diagnostics = restrictions.diagnostics.clone();
        diagnostics.insert(
            "invalid_restrictions_blocked".into(),
            m.restriction_diagnostics.len() as u64,
        );
        diagnostics.insert(
            "invalid_restriction_ways_blocked".into(),
            m.blocked_way_ids.len() as u64,
        );
        if [
            connection.format_version,
            edges.format_version,
            adjacency.format_version,
            restrictions.format_version,
        ]
        .iter()
        .any(|&v| v != 1)
            || [
                connection.nodes,
                edges.nodes,
                adjacency.nodes,
                restrictions.nodes,
            ]
            .iter()
            .any(|&n| n != m.nodes)
            || [edges.edges, adjacency.edges, restrictions.edges]
                .iter()
                .any(|&n| n != m.edges)
            || restrictions.restrictions != m.restrictions
            || restrictions.policy != InvalidRestrictionPolicy::Reject
            || !restrictions.restrictions_resolved
            || !connection.reference_closure_validated
            || connection.profile != "driving-v1"
            || edges.profile != "driving-v1"
            || connection.source_sha256 != m.source_sha256
            || edges.source_sha256 != m.source_sha256
            || connection.source_snapshot != m.source_snapshot
            || edges.source_snapshot != m.source_snapshot
            || [
                connection.source_records_sha256,
                edges.source_records_sha256,
                restrictions.source_records_sha256,
            ]
            .iter()
            .any(|v| v != &m.source_records_sha256)
            || connection.blocked_way_ids != m.blocked_way_ids
            || diagnostics != m.diagnostics
            || m.restriction_diagnostics
                .iter()
                .any(|d| d.action != "blocked_ways")
            || edges.connection_manifest_sha256 != m.files["graph/connection/manifest.json"].1
            || adjacency.edge_manifest_sha256 != m.files["graph/edges/manifest.json"].1
            || restrictions.edge_manifest_sha256 != m.files["graph/edges/manifest.json"].1
            || restrictions.adjacency_manifest_sha256 != m.files["graph/adjacency/manifest.json"].1
            || adjacency.edge_bytes != m.files["graph/edges/edges.jsonl"].0
        {
            return Err(invalid("publication child identity/counts/audit"));
        }
        Ok(())
    }
    fn lines(
        &self,
        name: &str,
        limit: u64,
        expected: u64,
        mut visitor: impl FnMut(&[u8]) -> DataResult<()>,
    ) -> DataResult<()> {
        let mut reader = BufReader::new(regular(&self.directory.join(name))?);
        let mut line = Vec::new();
        for _ in 0..expected {
            line.clear();
            (&mut reader).take(limit + 1).read_until(b'\n', &mut line)?;
            if line.is_empty() || line.len() as u64 > limit || line.last() != Some(&b'\n') {
                return Err(invalid("publication JSONL record limit/framing"));
            }
            visitor(&line)?;
        }
        if reader.read(&mut [0; 1])? != 0 {
            return Err(invalid("publication JSONL count"));
        }
        Ok(())
    }
}
impl BinarySource for Source {
    fn verify(&self) -> DataResult<()> {
        for name in [
            "",
            "graph",
            "graph/connection",
            "graph/edges",
            "graph/adjacency",
            "graph/restrictions",
        ] {
            if !fs::symlink_metadata(self.directory.join(name))?
                .file_type()
                .is_dir()
            {
                return Err(invalid("publication source directory"));
            }
        }
        let manifest = self.directory.join("manifest.json");
        let file = regular(&manifest)?;
        if file.metadata()?.len() > 1024 * 1024 || hash_file(&manifest)? != self.manifest_sha256 {
            return Err(invalid("publication source manifest changed"));
        }
        for name in FILES {
            let path = self.directory.join(name);
            let file = regular(&path)?;
            let (bytes, digest) = &self.report.files[name];
            if *bytes > 1024 * 1024 * 1024
                || file.metadata()?.len() != *bytes
                || hash_file(&path)? != *digest
            {
                return Err(invalid("publication source payload changed"));
            }
        }
        self.bindings()
    }
    fn nodes(&self, visitor: &mut dyn FnMut(Point) -> DataResult<()>) -> DataResult<()> {
        let mut reader =
            BufReader::new(regular(&self.directory.join("graph/connection/nodes.run"))?);
        let mut header = [0; 16];
        reader.read_exact(&mut header)?;
        if &header[..8] != super::MAGIC
            || u64::from_le_bytes(header[8..].try_into().unwrap()) != self.report.nodes
        {
            return Err(invalid("publication node header"));
        }
        let mut previous = None;
        for index in 0..self.report.nodes {
            let mut bytes = [0; 48];
            reader.read_exact(&mut bytes)?;
            let row: [u64; 6] = std::array::from_fn(|i| {
                u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap())
            });
            if row[1] != index
                || row[4] > 1
                || row[5] != 0
                || previous.is_some_and(|id| id >= row[0])
            {
                return Err(invalid("publication node row"));
            }
            previous = Some(row[0]);
            visitor(Point {
                lon: f64::from_bits(row[2]),
                lat: f64::from_bits(row[3]),
            })?;
        }
        if reader.read(&mut [0; 1])? != 0 {
            return Err(invalid("publication node count"));
        }
        Ok(())
    }
    fn edges(&self, visitor: &mut dyn FnMut(Edge) -> DataResult<()>) -> DataResult<()> {
        self.lines(
            "graph/edges/edges.jsonl",
            65536,
            self.report.edges,
            |line| {
                let edge = serde_json::from_slice(line).map_err(|e| invalid(e.to_string()))?;
                visitor(edge)
            },
        )
    }
    fn restrictions(&self, visitor: &mut dyn FnMut(&[usize]) -> DataResult<()>) -> DataResult<()> {
        self.lines(
            "graph/restrictions/forbidden.jsonl",
            4096,
            self.report.restrictions,
            |line| {
                let sequence: Sequence =
                    serde_json::from_slice(line).map_err(|e| invalid(e.to_string()))?;
                visitor(&sequence.0)
            },
        )
    }
}
struct Sequence(Vec<usize>);
impl<'de> Deserialize<'de> for Sequence {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Sequence;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("at most 256 edge IDs")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some(value) = seq.next_element()? {
                    if out.len() == 256 {
                        return Err(serde::de::Error::custom("restriction length"));
                    }
                    out.push(value);
                }
                Ok(Sequence(out))
            }
        }
        deserializer.deserialize_seq(Visitor)
    }
}
/// Source files are immutable inputs, excluded from the final writer's owned-byte
/// allowance. Input metadata is capped at 1 MiB, edge records at 64 KiB and
/// restriction records at 4 KiB / 256 IDs. No complete Graph is constructed.
pub fn publish_block_ways(
    input: &Path,
    output: &Path,
    options: BinaryWriteOptions,
) -> Result<PublicationReport> {
    let source = Source::open(input)?;
    let report = &source.report;
    let result = write_binary_streaming(
        output,
        &source,
        BinaryMetadata {
            nodes: report.nodes as usize,
            edges: report.edges as usize,
            restrictions: report.restrictions as usize,
            source_sha256: report.source_sha256.clone(),
            source_snapshot: report.source_snapshot,
            diagnostics: report.diagnostics.clone(),
        },
        options,
    );
    let input_report = InputReport {
        valid: result.is_ok(),
        failure_code: result
            .as_ref()
            .err()
            .map(|_| "DatasetPublicationError".into()),
        policy: report.policy,
        source_sha256: report.source_sha256.clone(),
        source_snapshot: report.source_snapshot,
        nodes: report.nodes as usize,
        edges: report.edges as usize,
        restrictions: report.restrictions as usize,
        diagnostics: report.diagnostics.clone(),
        restriction_diagnostics: report.restriction_diagnostics.clone(),
        blocked_way_ids: report.blocked_way_ids.clone(),
    };
    match result {
        Ok(manifest) => Ok(PublicationReport {
            manifest,
            source_manifest_sha256: source.manifest_sha256,
            input: input_report,
        }),
        Err(source) => Err(crate::Error {
            source,
            report: Box::new(input_report),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{fixture, source, temporary};
    use super::*;
    fn ready(parent: &Path) -> PathBuf {
        let staged = source(parent, &fixture());
        let output = parent.join("ready");
        super::super::prepare_block_ways(&staged, &output, Default::default()).unwrap();
        output
    }
    #[test]
    fn rejects_payload_corruption_unknown_paths_and_child_identity_mismatch() {
        let parent = temporary();
        let ready = ready(&parent);
        let output = parent.join("dataset");
        let payload = ready.join("graph/edges/edges.jsonl");
        let original = fs::read(&payload).unwrap();
        fs::write(&payload, b"{}\n").unwrap();
        assert!(publish_block_ways(&ready, &output, Default::default()).is_err());
        assert!(!output.exists());
        fs::write(&payload, original).unwrap();
        let top = ready.join("manifest.json");
        let original = fs::read(&top).unwrap();
        let mut m: serde_json::Value = serde_json::from_slice(&original).unwrap();
        m["files"]["../escape"] = serde_json::json!([0, "x"]);
        fs::write(&top, serde_json::to_vec(&m).unwrap()).unwrap();
        assert!(publish_block_ways(&ready, &output, Default::default()).is_err());
        fs::write(&top, &original).unwrap();
        let child = ready.join("graph/edges/manifest.json");
        let mut child_report: serde_json::Value =
            serde_json::from_slice(&fs::read(&child).unwrap()).unwrap();
        child_report["source_sha256"] = "f".repeat(64).into();
        let bytes = serde_json::to_vec(&child_report).unwrap();
        fs::write(&child, &bytes).unwrap();
        let mut m: serde_json::Value = serde_json::from_slice(&original).unwrap();
        m["files"]["graph/edges/manifest.json"] = serde_json::json!([bytes.len(), hash(&bytes)]);
        fs::write(&top, serde_json::to_vec(&m).unwrap()).unwrap();
        assert!(publish_block_ways(&ready, &output, Default::default()).is_err());
        assert!(!output.exists());
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn bounded_record_readers_reject_oversize_and_invalid_framing() {
        let parent = temporary();
        let ready = ready(&parent);
        let source = Source::open(&ready).unwrap();
        for bytes in [b"{}".to_vec(), vec![b'x'; 65537], b"{}\n{}\n{}\n".to_vec()] {
            fs::write(ready.join("graph/edges/edges.jsonl"), bytes).unwrap();
            assert!(source.edges(&mut |_| Ok(())).is_err());
        }
        assert!(serde_json::from_slice::<Sequence>(
            serde_json::to_vec(&vec![0; 257]).unwrap().as_slice()
        )
        .is_err());
        fs::remove_dir_all(parent).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn symlinked_source_components_are_rejected() {
        let parent = temporary();
        let ready = ready(&parent);
        let output = parent.join("dataset");
        let original = ready.join("graph/edges");
        let renamed = ready.join("graph/renamed");
        fs::rename(&original, &renamed).unwrap();
        std::os::unix::fs::symlink(&renamed, &original).unwrap();
        assert!(publish_block_ways(&ready, &output, Default::default()).is_err());
        assert!(!output.exists());
        fs::remove_dir_all(parent).unwrap();
    }
}
