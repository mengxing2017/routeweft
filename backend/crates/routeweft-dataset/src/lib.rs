//! Versioned, bounded, immutable routing dataset. No process-global resources.
use routeweft_domain::model::Coordinate;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    InvalidDataset(String),
    IncompatibleFormat,
    LimitExceeded,
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
#[derive(Clone, Copy)]
pub struct LoadOptions {
    pub max_bytes: u64,
    pub max_nodes: usize,
    pub max_edges: usize,
    pub max_restriction_edges: usize,
}
impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            max_bytes: 256 * 1024 * 1024,
            max_nodes: 1_000_000,
            max_edges: 2_000_000,
            max_restriction_edges: 1_000_000,
        }
    }
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
        if self.nodes.is_empty()
            || self.nodes.len() > limits.max_nodes
            || self.edges.len() > limits.max_edges
        {
            return Err(Error::LimitExceeded);
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
            if total > limits.max_restriction_edges
                || sequence.len() < 2
                || sequence.len() > 256
                || !unique.insert(sequence)
            {
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
fn identity(m: &Manifest) -> String {
    hash(
        format!(
            "routeweft-v1\n{}\n{}\n{}\n{}",
            m.source_sha256, m.profile, m.builder, m.graph_sha256
        )
        .as_bytes(),
    )
}
pub fn open(path: &Path, limits: LoadOptions) -> Result<(Manifest, Graph)> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(invalid("dataset directory symlink"));
    }
    let manifest: Manifest =
        serde_json::from_slice(&read_controlled(path, "manifest.json", 1024 * 1024)?)
            .map_err(|e| invalid(e.to_string()))?;
    if manifest.format != 1 {
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
    if manifest.graph_file != "graph.json"
        || manifest.profile != "driving-v1"
        || manifest.graph_bytes > limits.max_bytes
        || manifest.nodes > limits.max_nodes
        || manifest.edges > limits.max_edges
    {
        return Err(invalid("manifest contract"));
    }
    let bytes = read_controlled(path, "graph.json", limits.max_bytes)?;
    if bytes.len() as u64 != manifest.graph_bytes
        || hash(&bytes) != manifest.graph_sha256
        || identity(&manifest) != manifest.dataset_id
    {
        return Err(invalid("integrity"));
    }
    let graph: Graph = serde_json::from_slice(&bytes).map_err(|e| invalid(e.to_string()))?;
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
        let bytes = serde_json::to_vec(graph).map_err(|e| invalid(e.to_string()))?;
        if bytes.len() as u64 > LoadOptions::default().max_bytes {
            return Err(Error::LimitExceeded);
        }
        let mut m = Manifest {
            format: 1,
            dataset_id: String::new(),
            builder: env!("CARGO_PKG_VERSION").into(),
            profile: "driving-v1".into(),
            source_sha256,
            source_snapshot: snapshot,
            attribution:
                "© OpenStreetMap contributors; ODbL 1.0; https://www.openstreetmap.org/copyright"
                    .into(),
            graph_file: "graph.json".into(),
            graph_sha256: hash(&bytes),
            graph_bytes: bytes.len() as u64,
            nodes: graph.nodes.len(),
            edges: graph.edges.len(),
            restrictions: graph.forbidden.len(),
            diagnostics,
        };
        m.dataset_id = identity(&m);
        for (name, content) in [
            ("graph.json", bytes),
            (
                "manifest.json",
                serde_json::to_vec_pretty(&m).map_err(|e| invalid(e.to_string()))?,
            ),
        ] {
            let mut f = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(stage.join(name))?;
            f.write_all(&content)?;
            f.sync_all()?;
        }
        open(&stage, LoadOptions::default())?;
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
