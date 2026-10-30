//! Versioned, bounded, immutable routing dataset. No process-global resources.
mod identity;
pub use identity::{is_digest, DatasetIdentity};
mod adjacency;
mod binary;
mod publication;
pub use adjacency::Adjacency;
pub use publication::{write_binary_streaming, BinaryMetadata, BinarySource, BinaryWriteOptions};
use routeweft_domain::model::Coordinate;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, Write};
use std::path::Path;

pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    InvalidDataset(String),
    IncompatibleFormat,
    LimitExceeded,
    ResourceLimit(ResourceLimit),
}
/// A deterministic implementation budget, not an OS RSS guarantee.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceLimit {
    pub resource: String,
    pub limit: u64,
    pub observed: u64,
    pub stage: String,
}
pub fn resource_limit(resource: &str, limit: u64, observed: u64, stage: &str) -> Error {
    Error::ResourceLimit(ResourceLimit {
        resource: resource.into(),
        limit,
        observed,
        stage: stage.into(),
    })
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
pub fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidDataset(message.into())
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Point {
    pub lon: f64,
    pub lat: f64,
}
impl Point {
    pub fn coordinate(self) -> Result<Coordinate> {
        Coordinate::new(self.lon, self.lat).map_err(|_| invalid("coordinate"))
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Edge {
    pub from: usize,
    pub to: usize,
    pub way: i64,
    pub distance_mm: u64,
    pub duration_ms: u64,
    pub name: String,
    pub class: String,
    pub roundabout: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Graph {
    #[serde(deserialize_with = "nodes_decode")]
    pub nodes: Vec<Point>,
    #[serde(deserialize_with = "edges_decode")]
    pub edges: Vec<Edge>,
    #[serde(deserialize_with = "restrictions_decode")]
    pub forbidden: Vec<Vec<usize>>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: u32,
    pub dataset_id: String,
    pub builder: String,
    pub profile: String,
    pub source_sha256: String,
    pub source_snapshot: Option<i64>,
    pub attribution: String,
    pub graph_file: String,
    pub graph_sha256: String,
    pub graph_bytes: u64,
    pub nodes: usize,
    pub edges: usize,
    pub restrictions: usize,
    pub diagnostics: BTreeMap<String, u64>,
}
/// Explicit on-disk encoding; existing write callers continue producing v1.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DatasetFormat {
    #[default]
    JsonV1,
    BinaryV2,
}
impl DatasetFormat {
    pub fn version(self) -> u32 {
        match self {
            Self::JsonV1 => 1,
            Self::BinaryV2 => 2,
        }
    }
    fn graph_file(self) -> &'static str {
        match self {
            Self::JsonV1 => "graph.json",
            Self::BinaryV2 => "graph.bin",
        }
    }
}
/// Explicit graph capacity, independent of PBF decode and resident memory budgets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphCapacity {
    #[default]
    City,
    Regional,
}
impl GraphCapacity {
    pub fn limits(self) -> LoadOptions {
        match self {
            Self::City => LoadOptions::default(),
            Self::Regional => LoadOptions {
                max_bytes: 2 * 1024 * 1024 * 1024,
                max_nodes: 8_000_000,
                max_edges: 16_000_000,
                ..Default::default()
            },
        }
    }
    pub fn max_span_degrees(self) -> f64 {
        match self {
            Self::City => 5.,
            Self::Regional => 10.,
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub struct LoadOptions {
    pub max_bytes: u64,
    pub max_nodes: usize,
    pub max_edges: usize,
    pub max_restriction_edges: usize,
    /// Conservative logical allocation estimate; not a process RSS guarantee.
    pub max_resident_bytes: u64,
}
impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            max_bytes: 256 * 1024 * 1024,
            max_nodes: 1_000_000,
            max_edges: 2_000_000,
            max_restriction_edges: 1_000_000,
            max_resident_bytes: 2 * 1024 * 1024 * 1024,
        }
    }
}
/// Reject excessive resident allocations before decoding the payload. Count
/// arithmetic is checked; include spare vector capacity, strings/dictionary,
/// allocator overhead and (optionally) CSR, R-tree construction and turn trie.
pub fn check_resident_budget(m: &Manifest, limits: LoadOptions, indexes: bool) -> Result<u64> {
    // The v1 serde visitors enforce legacy ceilings before manifest counts can
    // be checked. Reserve that worst case against forged count declarations.
    let legacy = LoadOptions::default();
    let nodes = if m.format == 1 {
        legacy.max_nodes
    } else {
        m.nodes
    };
    let edges = if m.format == 1 {
        legacy.max_edges
    } else {
        m.edges
    };
    let refs = if m.format == 1 {
        legacy.max_restriction_edges as u64
    } else {
        (m.restrictions as u64)
            .saturating_mul(256)
            .min(limits.max_restriction_edges as u64)
    };
    let strings = (edges as u64).saturating_mul(4160).min(256 * 1024 * 1024);
    // Up to two dictionary entries per edge, including unused 4096-byte
    // entries; class length only constrains strings actually used as classes.
    let dictionary = (edges as u64)
        .saturating_mul(2 * (4096 + 4 + 64))
        .min(256 * 1024 * 1024);
    let estimated = (nodes as u64)
        .checked_mul(if indexes { 128 } else { 32 })
        .and_then(|n| {
            (edges as u64)
                .checked_mul(if indexes { 576 } else { 256 })
                .and_then(|e| n.checked_add(e))
        })
        .and_then(|n| {
            refs.checked_mul(if indexes { 320 } else { 64 })
                .and_then(|r| n.checked_add(r))
        })
        .and_then(|n| {
            n.checked_add(strings)
                .and_then(|n| n.checked_add(dictionary))
        })
        .ok_or(Error::LimitExceeded)?;
    if estimated > limits.max_resident_bytes {
        return Err(resource_limit(
            "estimated_resident_bytes",
            limits.max_resident_bytes,
            estimated,
            if indexes {
                "engine_load"
            } else {
                "dataset_load"
            },
        ));
    }
    Ok(estimated)
}
pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn hash_file(path: &Path) -> Result<String> {
    let mut reader = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0; 65536];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}
impl Graph {
    pub fn validate(&self, limits: LoadOptions) -> Result<()> {
        if self.nodes.is_empty() {
            return Err(invalid("empty graph"));
        }
        for (resource, observed, limit) in [
            ("graph_nodes", self.nodes.len(), limits.max_nodes),
            ("graph_edges", self.edges.len(), limits.max_edges),
        ] {
            if observed > limit {
                return Err(resource_limit(
                    resource,
                    limit as u64,
                    observed as u64,
                    "graph_validation",
                ));
            }
        }
        for p in &self.nodes {
            p.coordinate()?;
            if p.lat.abs() > 85.0 {
                return Err(invalid("supported latitude is [-85,85]"));
            }
        }
        for e in &self.edges {
            if e.from >= self.nodes.len()
                || e.to >= self.nodes.len()
                || e.duration_ms == 0
                || e.distance_mm == 0
                || e.name.len() > 4096
                || e.class.len() > 64
            {
                return Err(invalid("edge"));
            }
        }
        let mut total = 0_usize;
        let mut unique = BTreeSet::new();
        for sequence in &self.forbidden {
            total = total
                .checked_add(sequence.len())
                .ok_or(Error::LimitExceeded)?;
            if total > limits.max_restriction_edges {
                return Err(resource_limit(
                    "restriction_edges",
                    limits.max_restriction_edges as u64,
                    total as u64,
                    "graph_validation",
                ));
            }
            if sequence.len() < 2 || sequence.len() > 256 || !unique.insert(sequence) {
                return Err(invalid("restriction size/duplicate"));
            }
            if sequence.iter().any(|&id| id >= self.edges.len())
                || sequence
                    .windows(2)
                    .any(|w| self.edges[w[0]].to != self.edges[w[1]].from)
            {
                return Err(invalid("restriction reference"));
            }
        }
        Ok(())
    }
}
fn read_controlled(root: &Path, name: &str, limit: u64) -> Result<Vec<u8>> {
    let path = root.join(name);
    let meta = fs::symlink_metadata(&path)?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(invalid("non-regular dataset file"));
    }
    if meta.len() > limit {
        return Err(Error::LimitExceeded);
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(Error::LimitExceeded);
    }
    Ok(bytes)
}
fn json_stream_error(error: serde_json::Error) -> Error {
    if let Some(kind) = error.io_error_kind() {
        Error::Io(std::io::Error::new(kind, error))
    } else {
        invalid(error.to_string())
    }
}
fn controlled_file(root: &Path, name: &str, limit: u64) -> Result<File> {
    let path = root.join(name);
    let meta = fs::symlink_metadata(&path)?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(invalid("non-regular dataset file"));
    }
    let file = File::open(path)?;
    if file.metadata()?.len() > limit {
        return Err(resource_limit(
            "graph_bytes",
            limit,
            file.metadata()?.len(),
            "dataset_read",
        ));
    }
    Ok(file)
}
struct DigestReader<R> {
    inner: R,
    digest: Sha256,
    bytes: u64,
    limit: u64,
    exceeded: Option<u64>,
}
impl<R> DigestReader<R> {
    fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            digest: Sha256::new(),
            bytes: 0,
            limit,
            exceeded: None,
        }
    }
    fn finish(self) -> (u64, String) {
        (self.bytes, format!("{:x}", self.digest.finalize()))
    }
}
impl<R: Read> Read for DigestReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let allowed = self
            .limit
            .saturating_sub(self.bytes)
            .saturating_add(1)
            .min(buf.len() as u64) as usize;
        let n = self.inner.read(&mut buf[..allowed])?;
        self.bytes = self.bytes.saturating_add(n as u64);
        if self.bytes > self.limit {
            self.exceeded = Some(self.bytes);
            return Err(std::io::Error::other("graph byte budget exceeded"));
        }
        self.digest.update(&buf[..n]);
        Ok(n)
    }
}
struct DigestWriter<W> {
    inner: W,
    digest: Sha256,
    bytes: u64,
    limit: u64,
    exceeded: Option<u64>,
}
impl<W: Write> Write for DigestWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let observed = self.bytes.saturating_add(buf.len() as u64);
        if observed > self.limit {
            self.exceeded = Some(observed);
            return Err(std::io::Error::other("graph byte budget exceeded"));
        }
        let n = self.inner.write(buf)?;
        self.digest.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}
fn identity(m: &Manifest) -> String {
    hash(
        format!(
            "routeweft-v{}\n{}\n{}\n{}\n{}",
            m.format, m.source_sha256, m.profile, m.builder, m.graph_sha256
        )
        .as_bytes(),
    )
}
/// Validate manifest syntax, declared limits and content identity only.
/// Does not read or authenticate graph bytes or check topology.
pub fn inspect_manifest(path: &Path, limits: LoadOptions) -> Result<Manifest> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(invalid("dataset directory symlink"));
    }
    let manifest: Manifest =
        serde_json::from_slice(&read_controlled(path, "manifest.json", 1024 * 1024)?)
            .map_err(|e| invalid(e.to_string()))?;
    if !matches!(manifest.format, 1 | 2) {
        return Err(Error::IncompatibleFormat);
    }
    if manifest.source_sha256.len() != 64
        || !manifest
            .source_sha256
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
    {
        return Err(invalid("source hash"));
    }
    let graph_file = if manifest.format == 1 {
        "graph.json"
    } else {
        "graph.bin"
    };
    if manifest.graph_file != graph_file || manifest.profile != "driving-v1" {
        return Err(invalid("manifest contract"));
    }
    for (resource, observed, limit) in [
        ("graph_bytes", manifest.graph_bytes, limits.max_bytes),
        (
            "graph_nodes",
            manifest.nodes as u64,
            limits.max_nodes as u64,
        ),
        (
            "graph_edges",
            manifest.edges as u64,
            limits.max_edges as u64,
        ),
    ] {
        if observed > limit {
            return Err(resource_limit(
                resource,
                limit,
                observed,
                "dataset_manifest",
            ));
        }
    }
    if manifest.graph_sha256.len() != 64
        || !manifest.graph_sha256.bytes().all(|b| b.is_ascii_hexdigit())
        || identity(&manifest) != manifest.dataset_id
    {
        return Err(invalid("manifest identity"));
    }
    Ok(manifest)
}
/// Fully validate an immutable v2 dataset using seeks instead of a resident Graph.
/// `workspace_bytes` bounds logical dictionary capacity and restriction deduplication
/// storage, including conservative allocation overhead; it is not an RSS limit.
/// The directory and its files must remain immutable throughout the call.
pub fn validate_binary_streaming(
    path: &Path,
    limits: LoadOptions,
    workspace_bytes: u64,
) -> Result<Manifest> {
    let manifest = inspect_manifest(path, limits)?;
    if manifest.format != 2 {
        return Err(Error::IncompatibleFormat);
    }
    let file = controlled_file(path, &manifest.graph_file, limits.max_bytes)?;
    if file.metadata()?.len() != manifest.graph_bytes {
        return Err(invalid("integrity"));
    }
    let mut reader = BufReader::new(file);
    binary::streaming::validate(&mut reader, &manifest, limits, workspace_bytes)?;
    reader.rewind()?;
    let mut digest = Sha256::new();
    let mut bytes = [0_u8; 65536];
    let mut bounded = reader.take(manifest.graph_bytes.saturating_add(1));
    let mut total = 0_u64;
    loop {
        let n = bounded.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        digest.update(&bytes[..n]);
    }
    if total != manifest.graph_bytes || format!("{:x}", digest.finalize()) != manifest.graph_sha256
    {
        return Err(invalid("integrity"));
    }
    Ok(manifest)
}
pub fn open(path: &Path, limits: LoadOptions) -> Result<(Manifest, Graph)> {
    let manifest = inspect_manifest(path, limits)?;
    check_resident_budget(&manifest, limits, false)?;
    let file = controlled_file(path, &manifest.graph_file, limits.max_bytes)?;
    if file.metadata()?.len() != manifest.graph_bytes {
        return Err(invalid("integrity"));
    }
    let mut reader = BufReader::new(DigestReader::new(file, limits.max_bytes));
    let parsed = if manifest.format == 1 {
        serde_json::from_reader::<_, Graph>(&mut reader).map_err(json_stream_error)
    } else {
        binary::read(&mut reader, &manifest, limits)
    };
    if let Some(observed) = reader.get_ref().exceeded {
        return Err(resource_limit(
            "graph_bytes",
            limits.max_bytes,
            observed,
            "dataset_read",
        ));
    }
    let graph = parsed?;
    let (bytes, digest) = reader.into_inner().finish();
    if bytes != manifest.graph_bytes
        || digest != manifest.graph_sha256
        || identity(&manifest) != manifest.dataset_id
    {
        return Err(invalid("integrity"));
    }
    graph.validate(limits)?;
    if graph.nodes.len() != manifest.nodes
        || graph.edges.len() != manifest.edges
        || graph.forbidden.len() != manifest.restrictions
    {
        return Err(invalid("counts"));
    }
    Ok((manifest, graph))
}
/// No overwrite. A unique staging directory is removed on failure and renamed
/// only after verifying its complete contents. Destination must not exist.
pub fn write(
    path: &Path,
    graph: &Graph,
    source_sha256: String,
    snapshot: Option<i64>,
    diagnostics: BTreeMap<String, u64>,
) -> Result<Manifest> {
    write_with_format(
        path,
        graph,
        source_sha256,
        snapshot,
        diagnostics,
        DatasetFormat::JsonV1,
    )
}
/// Same immutable publication and resource contracts as write, with explicit format.
pub fn write_with_format(
    path: &Path,
    graph: &Graph,
    source_sha256: String,
    snapshot: Option<i64>,
    diagnostics: BTreeMap<String, u64>,
    format: DatasetFormat,
) -> Result<Manifest> {
    graph.validate(LoadOptions::default())?;
    if path.exists() {
        return Err(invalid("output already exists"));
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| invalid("clock"))?
        .as_nanos();
    let stage = parent.join(format!(".routeweft-{}-{nonce}", std::process::id()));
    fs::create_dir(&stage)?;
    let result = (|| {
        let graph_file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(stage.join(format.graph_file()))?;
        let mut writer = BufWriter::new(DigestWriter {
            inner: graph_file,
            digest: Sha256::new(),
            bytes: 0,
            limit: LoadOptions::default().max_bytes,
            exceeded: None,
        });
        let serialized = match format {
            DatasetFormat::JsonV1 => {
                serde_json::to_writer(&mut writer, graph).map_err(json_stream_error)
            }
            DatasetFormat::BinaryV2 => binary::write(&mut writer, graph, LoadOptions::default()),
        };
        let flushed = writer.flush();
        if let Some(observed) = writer.get_ref().exceeded {
            return Err(resource_limit(
                "graph_bytes",
                writer.get_ref().limit,
                observed,
                "dataset_write",
            ));
        }
        serialized?;
        flushed?;
        writer.get_ref().inner.sync_all()?;
        let writer = writer.into_inner().map_err(|e| Error::Io(e.into_error()))?;
        let graph_bytes = writer.bytes;
        let graph_sha256 = format!("{:x}", writer.digest.finalize());
        let mut m = Manifest {
            format: format.version(),
            dataset_id: String::new(),
            builder: env!("CARGO_PKG_VERSION").into(),
            profile: "driving-v1".into(),
            source_sha256,
            source_snapshot: snapshot,
            attribution:
                "© OpenStreetMap contributors; ODbL 1.0; https://www.openstreetmap.org/copyright"
                    .into(),
            graph_file: format.graph_file().into(),
            graph_sha256,
            graph_bytes,
            nodes: graph.nodes.len(),
            edges: graph.edges.len(),
            restrictions: graph.forbidden.len(),
            diagnostics,
        };
        m.dataset_id = identity(&m);
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(stage.join("manifest.json"))?;
        serde_json::to_writer_pretty(&mut file, &m).map_err(|e| invalid(e.to_string()))?;
        file.sync_all()?;
        // Graph was validated before serialization. Recheck persisted bytes without
        // allocating a second Graph while the caller's Graph is still alive.
        if hash_file(&stage.join(format.graph_file()))? != m.graph_sha256
            || fs::metadata(stage.join(format.graph_file()))?.len() != m.graph_bytes
        {
            return Err(invalid("persisted graph integrity"));
        }
        let persisted = inspect_manifest(&stage, LoadOptions::default())?;
        if persisted != m {
            return Err(invalid("persisted manifest integrity"));
        }
        // Refuse existing targets immediately before publication as well.
        if path.exists() {
            return Err(invalid("output already exists"));
        }
        fs::rename(&stage, path)?;
        File::open(parent)?.sync_all()?;
        Ok(m)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&stage);
    }
    result
}

struct Limited<T, const N: usize>(Vec<T>);
impl<'de, T: Deserialize<'de>, const N: usize> Deserialize<'de> for Limited<T, N> {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Visitor<T, const N: usize>(std::marker::PhantomData<T>);
        impl<'de, T: Deserialize<'de>, const N: usize> serde::de::Visitor<'de> for Visitor<T, N> {
            type Value = Limited<T, N>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "array with at most {N} elements")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some(value) = seq.next_element()? {
                    if out.len() >= N {
                        return Err(serde::de::Error::custom("array limit"));
                    }
                    out.push(value);
                }
                Ok(Limited(out))
            }
        }
        deserializer.deserialize_seq(Visitor::<T, N>(std::marker::PhantomData))
    }
}
fn nodes_decode<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Vec<Point>, D::Error> {
    Ok(Limited::<Point, 1_000_000>::deserialize(d)?.0)
}
fn edges_decode<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Vec<Edge>, D::Error> {
    Ok(Limited::<Edge, 2_000_000>::deserialize(d)?.0)
}
fn restrictions_decode<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Vec<Vec<usize>>, D::Error> {
    struct Visitor;
    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = Vec<Vec<usize>>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "bounded restriction sequences")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            let mut total = 0_usize;
            while let Some(Limited(sequence)) = seq.next_element::<Limited<usize, 256>>()? {
                total = total.saturating_add(sequence.len());
                if total > 1_000_000 || out.len() >= 500_000 {
                    return Err(serde::de::Error::custom("restriction limit"));
                }
                out.push(sequence);
            }
            Ok(out)
        }
    }
    d.deserialize_seq(Visitor)
}

#[cfg(test)]
mod streaming_tests {
    use super::*;
    #[test]
    fn growth_during_streaming_read_and_write_cannot_bypass_byte_budget() {
        let mut reader = DigestReader::new(&b"123456"[..], 5);
        let mut bytes = Vec::new();
        assert!(reader.read_to_end(&mut bytes).is_err());
        assert_eq!(reader.exceeded, Some(6));
        let mut writer = DigestWriter {
            inner: Vec::new(),
            digest: Sha256::new(),
            bytes: 0,
            limit: 5,
            exceeded: None,
        };
        writer.write_all(b"123").unwrap();
        assert!(writer.write_all(b"456").is_err());
        assert_eq!(writer.inner, b"123");
        assert_eq!(writer.exceeded, Some(6));
    }
}
