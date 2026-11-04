//! Optional seekable query indexes over the authoritative compact v2 graph.
//! Graph strings remain dictionary IDs until an edge is materialized for a query.
use crate::*;
#[path = "paged_prepare.rs"]
mod prepare;
#[path = "paged_verify.rs"]
mod verify;
use std::{
    collections::HashMap,
    io::{Seek, SeekFrom},
    sync::Mutex,
};
pub use verify::QueryIndexVerification;

pub const INDEX_FILE: &str = "query.idx";
const MAGIC: &[u8; 8] = b"RWFTQI02";
const HEADER_BYTES: u64 = 4096;
const PAGE: usize = 4096;
const PAGE_CHARGE: u64 = PAGE as u64 + 256;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryIndexMetadata {
    pub schema: u32,
    pub dataset_id: String,
    pub graph_sha256: String,
    pub profile: String,
    pub tool_build: serde_json::Value,
    pub leaf_edges: usize,
    pub nodes: u64,
    pub edges: u64,
    pub dictionary_count: u64,
    pub graph_nodes: u64,
    pub graph_edges: u64,
    pub graph_restrictions: u64,
    pub dictionary: u64,
    pub offsets: u64,
    pub edge_ids: u64,
    pub components: u64,
    pub spatial: u64,
    pub spatial_count: u64,
    pub spatial_ids: u64,
    #[serde(default)]
    pub compact: u64,
    #[serde(default)]
    pub potentials: u64,
    #[serde(default)]
    pub landmark_count: u32,
    pub bytes: u64,
    pub largest_component: u32,
    pub latitude_midpoint: f64,
    pub span_degrees: f64,
}
#[derive(Clone, Copy)]
struct Spatial {
    bounds: [f64; 4],
    left: u64,
    right: u64,
    start: u64,
    count: u64,
}
impl Spatial {
    fn bytes(self) -> [u8; 64] {
        let mut bytes = [0; 64];
        for (i, v) in self.bounds.iter().enumerate() {
            bytes[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
        }
        for (i, v) in [self.left, self.right, self.start, self.count]
            .iter()
            .enumerate()
        {
            bytes[32 + i * 8..40 + i * 8].copy_from_slice(&v.to_le_bytes());
        }
        bytes
    }
}
fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}
fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}
fn find(parent: &mut [u32], mut a: usize) -> usize {
    while parent[a] as usize != a {
        parent[a] = parent[parent[a] as usize];
        a = parent[a] as usize;
    }
    a
}
fn spatial_build(graph: &Graph, ids: &mut [usize], start: usize, tree: &mut Vec<Spatial>) -> usize {
    let index = tree.len();
    let mut bounds = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    for &id in ids.iter() {
        let e = &graph.edges[id];
        for p in [graph.nodes[e.from], graph.nodes[e.to]] {
            bounds[0] = bounds[0].min(p.lon);
            bounds[1] = bounds[1].min(p.lat);
            bounds[2] = bounds[2].max(p.lon);
            bounds[3] = bounds[3].max(p.lat);
        }
    }
    tree.push(Spatial {
        bounds,
        left: 0,
        right: 0,
        start: start as u64,
        count: ids.len() as u64,
    });
    if ids.len() > 32 {
        let axis = usize::from(bounds[3] - bounds[1] > bounds[2] - bounds[0]);
        let middle = ids.len() / 2;
        ids.select_nth_unstable_by(middle, |&a, &b| {
            let center = |id: usize| {
                let e = &graph.edges[id];
                let a = graph.nodes[e.from];
                let b = graph.nodes[e.to];
                if axis == 0 {
                    a.lon + b.lon
                } else {
                    a.lat + b.lat
                }
            };
            center(a).total_cmp(&center(b)).then(a.cmp(&b))
        });
        let (left, right) = ids.split_at_mut(middle);
        let a = spatial_build(graph, left, start, tree);
        let b = spatial_build(graph, right, start + middle, tree);
        tree[index].left = a as u64;
        tree[index].right = b as u64;
        tree[index].count = 0;
    }
    index
}
/// Offline preparation may decode the complete editing graph; serving never does.
/// Atomic publication leaves a previous index intact on failure.
pub fn prepare_query_index(
    path: &Path,
    limits: LoadOptions,
    tool_build: serde_json::Value,
) -> Result<QueryIndexMetadata> {
    let m = inspect_manifest(path, limits)?;
    check_resident_budget(&m, limits, true)?;
    // Include undirected CSR, interleaved potentials, min-distance selection,
    // duplicate queue entries and allocation-growth overlap before constructing.
    let preparation_peak = resource_plan_from_manifest(&m, limits)?
        .engine_load
        .estimated_bytes
        .checked_add(
            (m.nodes as u64)
                .checked_mul(64)
                .ok_or(Error::LimitExceeded)?,
        )
        .and_then(|v| {
            (m.edges as u64)
                .checked_mul(192)
                .and_then(|e| v.checked_add(e))
        })
        .and_then(|v| v.checked_add(131072))
        .ok_or(Error::LimitExceeded)?;
    if preparation_peak > limits.max_resident_bytes {
        return Err(resource_limit(
            "resident_bytes",
            limits.max_resident_bytes,
            preparation_peak,
            "prepare_query_landmarks",
        ));
    }
    let (manifest, graph) = open(path, limits)?;
    if manifest.format != 2 {
        return Err(Error::IncompatibleFormat);
    }
    let mut source = controlled_file(path, &manifest.graph_file, limits.max_bytes)?;
    let mut header = [0; 64];
    source.read_exact(&mut header)?;
    let dictionary_count = u64_at(&header, 40);
    let dictionary_bytes = u64_at(&header, 56);
    let mut dictionary = Vec::new();
    for _ in 0..dictionary_count {
        let mut length = [0; 4];
        source.read_exact(&mut length)?;
        let len = u32::from_le_bytes(length);
        let offset = source.stream_position()?;
        dictionary.push((offset, len));
        source.seek(SeekFrom::Current(len as i64))?;
    }
    let adjacency = Adjacency::with_limits(graph.nodes.len(), &graph.edges, limits)?;
    let mut parent: Vec<u32> = (0..graph.nodes.len() as u32).collect();
    for edge in &graph.edges {
        let a = find(&mut parent, edge.from);
        let b = find(&mut parent, edge.to);
        parent[a.max(b)] = a.min(b) as u32;
    }
    let mut sizes = vec![0u32; parent.len()];
    for i in 0..parent.len() {
        parent[i] = find(&mut parent, i) as u32;
        sizes[parent[i] as usize] += 1;
    }
    let largest = sizes
        .iter()
        .enumerate()
        .max_by_key(|&(i, s)| (*s, std::cmp::Reverse(i)))
        .map(|(i, _)| i as u32)
        .unwrap_or(0);
    let potentials = prepare::landmarks(&graph, largest as usize);
    let mut ids: Vec<_> = (0..graph.edges.len()).collect();
    let mut tree = Vec::new();
    if !ids.is_empty() {
        spatial_build(&graph, &mut ids, 0, &mut tree);
    }
    let min_lat = graph
        .nodes
        .iter()
        .map(|p| p.lat)
        .fold(f64::INFINITY, f64::min);
    let max_lat = graph
        .nodes
        .iter()
        .map(|p| p.lat)
        .fold(f64::NEG_INFINITY, f64::max);
    let min_lon = graph
        .nodes
        .iter()
        .map(|p| p.lon)
        .fold(f64::INFINITY, f64::min);
    let max_lon = graph
        .nodes
        .iter()
        .map(|p| p.lon)
        .fold(f64::NEG_INFINITY, f64::max);
    let mut m = QueryIndexMetadata {
        schema: 2,
        dataset_id: manifest.dataset_id.clone(),
        graph_sha256: manifest.graph_sha256.clone(),
        profile: manifest.profile.clone(),
        tool_build,
        leaf_edges: 32,
        nodes: manifest.nodes as u64,
        edges: manifest.edges as u64,
        dictionary_count,
        graph_nodes: 64 + dictionary_bytes,
        graph_edges: 64 + dictionary_bytes + manifest.nodes as u64 * 16,
        graph_restrictions: 64
            + dictionary_bytes
            + manifest.nodes as u64 * 16
            + manifest.edges as u64 * 56,
        dictionary: HEADER_BYTES,
        offsets: 0,
        edge_ids: 0,
        components: 0,
        spatial: 0,
        spatial_count: tree.len() as u64,
        spatial_ids: 0,
        compact: 0,
        potentials: 0,
        landmark_count: 4,
        bytes: 0,
        largest_component: largest,
        latitude_midpoint: (min_lat + max_lat) / 2.,
        span_degrees: (max_lat - min_lat).max(max_lon - min_lon),
    };
    m.offsets = m.dictionary + dictionary_count * 12;
    m.edge_ids = m.offsets + (m.nodes + 1) * 8;
    m.components = m.edge_ids + m.edges * 8;
    m.spatial = m.components + m.nodes * 8;
    m.spatial_ids = m.spatial + m.spatial_count * 64;
    m.compact = m.spatial_ids + m.edges * 8;
    m.potentials = m.compact + m.edges * 24;
    m.bytes = m.potentials + m.nodes * 32 + 32;
    let encoded = serde_json::to_vec(&m).map_err(|_| invalid("query metadata"))?;
    if encoded.len() > HEADER_BYTES as usize - 12 {
        return Err(invalid("query metadata size"));
    }
    let temp = path.join(format!(
        ".query-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| invalid("clock"))?
            .as_nanos()
    ));
    let result = (|| {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        let mut writer = BufWriter::new(file);
        let mut header = [0; HEADER_BYTES as usize];
        header[..8].copy_from_slice(MAGIC);
        header[8..12].copy_from_slice(&(encoded.len() as u32).to_le_bytes());
        header[12..12 + encoded.len()].copy_from_slice(&encoded);
        writer.write_all(&header)?;
        for (offset, len) in dictionary {
            writer.write_all(&offset.to_le_bytes())?;
            writer.write_all(&len.to_le_bytes())?;
        }
        let mut offset = 0u64;
        for i in 0..graph.nodes.len() {
            writer.write_all(&offset.to_le_bytes())?;
            offset += adjacency[i].len() as u64;
        }
        writer.write_all(&offset.to_le_bytes())?;
        for i in 0..graph.nodes.len() {
            for &id in &adjacency[i] {
                writer.write_all(&(id as u64).to_le_bytes())?;
            }
        }
        for &c in &parent {
            writer.write_all(&c.to_le_bytes())?;
            writer.write_all(&sizes[c as usize].to_le_bytes())?;
        }
        for node in tree {
            writer.write_all(&node.bytes())?;
        }
        for id in ids {
            writer.write_all(&(id as u64).to_le_bytes())?;
        }
        for node in 0..graph.nodes.len() {
            for &id in &adjacency[node] {
                let edge = &graph.edges[id];
                writer.write_all(&(id as u64).to_le_bytes())?;
                writer.write_all(&(edge.to as u64).to_le_bytes())?;
                writer.write_all(&edge.duration_ms.to_le_bytes())?;
            }
        }
        for row in potentials {
            for value in row {
                writer.write_all(&value.to_le_bytes())?;
            }
        }
        writer.flush()?;
        drop(writer);
        let digest = hash_file(&temp)?;
        let bytes = (0..32)
            .map(|i| u8::from_str_radix(&digest[i * 2..i * 2 + 2], 16).unwrap())
            .collect::<Vec<_>>();
        let mut file = OpenOptions::new().append(true).open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        if hash_file(&path.join(&manifest.graph_file))? != manifest.graph_sha256 {
            return Err(invalid("graph changed during preparation"));
        }
        let current = inspect_manifest(path, limits)?;
        if current != manifest {
            return Err(invalid("manifest changed during preparation"));
        }
        fs::rename(&temp, path.join(INDEX_FILE))?;
        File::open(path)?.sync_all()?;
        Ok(m)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct PageStats {
    pub hits: u64,
    pub misses: u64,
    pub bytes_read: u64,
    pub cached_pages: usize,
    pub cache_budget_bytes: u64,
}
struct PageEntry {
    bytes: Vec<u8>,
    used: u64,
}
struct Cache {
    files: [File; 2],
    lengths: [u64; 2],
    pages: HashMap<(usize, u64), PageEntry>,
    order: BTreeMap<u64, (usize, u64)>,
    clock: u64,
    stats: PageStats,
    max_pages: usize,
}
impl Cache {
    fn read(&mut self, kind: usize, mut offset: u64, mut output: &mut [u8]) -> Result<()> {
        if offset
            .checked_add(output.len() as u64)
            .is_none_or(|end| end > self.lengths[kind])
        {
            return Err(invalid("paged range"));
        }
        while !output.is_empty() {
            let id = offset / PAGE as u64;
            let key = (kind, id);
            if self.clock == u64::MAX {
                self.pages.clear();
                self.order.clear();
                self.clock = 0;
            }
            self.clock += 1;
            if !self.pages.contains_key(&key) {
                if self.pages.len() == self.max_pages {
                    let (_, victim) = self.order.pop_first().unwrap();
                    self.pages.remove(&victim);
                }
                let start = id * PAGE as u64;
                let length = (self.lengths[kind] - start).min(PAGE as u64) as usize;
                let mut bytes = vec![0; length];
                self.files[kind].seek(SeekFrom::Start(start))?;
                self.files[kind].read_exact(&mut bytes)?;
                self.stats.misses += 1;
                self.stats.bytes_read += length as u64;
                self.pages.insert(
                    key,
                    PageEntry {
                        bytes,
                        used: self.clock,
                    },
                );
            } else {
                self.stats.hits += 1;
            }
            let entry = self.pages.get_mut(&key).unwrap();
            self.order.remove(&entry.used);
            entry.used = self.clock;
            self.order.insert(self.clock, key);
            let begin = (offset % PAGE as u64) as usize;
            let count = output.len().min(entry.bytes.len() - begin);
            output[..count].copy_from_slice(&entry.bytes[begin..begin + count]);
            output = &mut output[count..];
            offset += count as u64;
        }
        self.stats.cached_pages = self.pages.len();
        Ok(())
    }
}
/// Thread-safe bounded page cache. No mapped unsafe layouts or per-edge strings.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct PagedOpenDiagnostics {
    pub base_validation_and_hash_ms: f64,
    pub index_header_ms: f64,
    pub index_hash_ms: f64,
    pub verified_graph_bytes: u64,
    pub verified_index_bytes: u64,
    pub authentication: &'static str,
}
pub struct PagedGraph {
    pub open_diagnostics: PagedOpenDiagnostics,
    pub manifest: Manifest,
    pub metadata: QueryIndexMetadata,
    pub index_sha256: String,
    cache: Mutex<Cache>,
}
impl PagedGraph {
    pub fn open(
        path: &Path,
        limits: LoadOptions,
        cache_bytes: u64,
        workspace: u64,
    ) -> Result<Self> {
        if cache_bytes < PAGE_CHARGE {
            return Err(resource_limit(
                "page_cache_bytes",
                cache_bytes,
                PAGE_CHARGE,
                "paged_open",
            ));
        }
        // Authentication scans are streamed separately from random query reads.
        let started = std::time::Instant::now();
        let plan = resource_plan(path, limits, workspace)?;
        if plan.format != 2 {
            return Err(Error::IncompatibleFormat);
        }
        let base_validation_and_hash_ms = started.elapsed().as_secs_f64() * 1000.;
        let header_started = std::time::Instant::now();
        let manifest = inspect_manifest(path, limits)?;
        let mut file = controlled_file(path, INDEX_FILE, limits.max_bytes.saturating_mul(4))?;
        let length = file.metadata()?.len();
        if length < HEADER_BYTES + 32 {
            return Err(invalid("query index size"));
        }
        let mut header = [0; HEADER_BYTES as usize];
        file.read_exact(&mut header)?;
        if &header[..8] != MAGIC && &header[..8] != b"RWFTQI01" {
            return Err(Error::IncompatibleFormat);
        }
        let len = u32_at(&header, 8) as usize;
        if len > header.len() - 12 {
            return Err(invalid("query header"));
        }
        let m: QueryIndexMetadata =
            serde_json::from_slice(&header[12..12 + len]).map_err(|_| invalid("query metadata"))?;
        if !matches!(m.schema, 1 | 2)
            || (m.schema == 1 && &header[..8] != b"RWFTQI01")
            || (m.schema == 2 && &header[..8] != MAGIC)
            || m.dataset_id != manifest.dataset_id
            || m.graph_sha256 != manifest.graph_sha256
            || m.profile != manifest.profile
            || m.nodes != manifest.nodes as u64
            || m.edges != manifest.edges as u64
            || m.graph_nodes < 64
            || m.graph_nodes > manifest.graph_bytes
            || m.dictionary_count > m.edges * 2
            || m.spatial_count > m.edges * 2
            || m.bytes != length
            || !m.latitude_midpoint.is_finite()
            || !m.span_degrees.is_finite()
            || m.span_degrees < 0.
            || m.leaf_edges != 32
        {
            return Err(invalid("query identity/layout"));
        }
        if m.dictionary != HEADER_BYTES
            || m.offsets != m.dictionary + m.dictionary_count * 12
            || m.edge_ids != m.offsets + (m.nodes + 1) * 8
            || m.components != m.edge_ids + m.edges * 8
            || m.spatial != m.components + m.nodes * 8
            || m.spatial_ids != m.spatial + m.spatial_count * 64
            || m.graph_edges != m.graph_nodes + m.nodes * 16
            || m.graph_restrictions != m.graph_edges + m.edges * 56
            || m.graph_restrictions > manifest.graph_bytes
        {
            return Err(invalid("query layout"));
        }
        let end = m.spatial_ids + m.edges * 8;
        if (m.schema == 1
            && (m.bytes != end + 32
                || m.landmark_count != 0
                || m.compact != 0
                || m.potentials != 0))
            || (m.schema == 2
                && (m.landmark_count != 4
                    || m.compact != end
                    || m.potentials != end + m.edges * 24
                    || m.bytes != end + m.edges * 24 + m.nodes * 32 + 32))
        {
            return Err(invalid("compact query layout"));
        }
        let index_header_ms = header_started.elapsed().as_secs_f64() * 1000.;
        let hash_started = std::time::Instant::now();
        file.seek(SeekFrom::Start(0))?;
        let mut hash = Sha256::new();
        let mut remaining = length - 32;
        let mut buffer = [0; 65536];
        while remaining > 0 {
            let count = remaining.min(buffer.len() as u64) as usize;
            file.read_exact(&mut buffer[..count])?;
            hash.update(&buffer[..count]);
            remaining -= count as u64;
        }
        let mut trailer = [0; 32];
        file.read_exact(&mut trailer)?;
        if hash.clone().finalize().as_slice() != trailer {
            return Err(invalid("query checksum"));
        }
        hash.update(trailer);
        let source = controlled_file(path, &manifest.graph_file, limits.max_bytes)?;
        Ok(Self {
            open_diagnostics: PagedOpenDiagnostics {
                base_validation_and_hash_ms,
                index_header_ms,
                index_hash_ms: hash_started.elapsed().as_secs_f64() * 1000.,
                verified_graph_bytes: manifest.graph_bytes,
                verified_index_bytes: length,
                authentication: "full_each_open_retain_verified_handle_for_resume",
            },
            manifest,
            metadata: m,
            index_sha256: format!("{:x}", hash.finalize()),
            cache: Mutex::new(Cache {
                files: [source, file],
                lengths: [plan.counts.graph_bytes, length],
                pages: HashMap::new(),
                order: BTreeMap::new(),
                clock: 0,
                stats: PageStats {
                    cache_budget_bytes: cache_bytes,
                    ..Default::default()
                },
                max_pages: (cache_bytes / PAGE_CHARGE).min(usize::MAX as u64) as usize,
            }),
        })
    }
    pub fn stats(&self) -> PageStats {
        self.cache.lock().unwrap().stats
    }
    fn read<const N: usize>(&self, kind: usize, offset: u64) -> Result<[u8; N]> {
        let mut bytes = [0; N];
        self.cache.lock().unwrap().read(kind, offset, &mut bytes)?;
        Ok(bytes)
    }
    pub fn node(&self, id: usize) -> Result<Point> {
        if id >= self.manifest.nodes {
            return Err(invalid("node id"));
        }
        let bytes = self.read::<16>(0, self.metadata.graph_nodes + id as u64 * 16)?;
        Ok(Point {
            lon: f64::from_le_bytes(bytes[..8].try_into().unwrap()),
            lat: f64::from_le_bytes(bytes[8..].try_into().unwrap()),
        })
    }
    pub fn component(&self, node: usize) -> Result<(u32, u32)> {
        if node >= self.manifest.nodes {
            return Err(invalid("node id"));
        }
        let bytes = self.read::<8>(1, self.metadata.components + node as u64 * 8)?;
        Ok((u32_at(&bytes, 0), u32_at(&bytes, 4)))
    }
    pub fn edge_record(&self, id: usize) -> Result<[u8; 56]> {
        if id >= self.manifest.edges {
            return Err(invalid("edge id"));
        }
        self.read(0, self.metadata.graph_edges + id as u64 * 56)
    }
    pub fn edge(&self, id: usize) -> Result<Edge> {
        let b = self.edge_record(id)?;
        Ok(Edge {
            from: u64_at(&b, 0) as usize,
            to: u64_at(&b, 8) as usize,
            way: i64::from_le_bytes(b[16..24].try_into().unwrap()),
            distance_mm: u64_at(&b, 24),
            duration_ms: u64_at(&b, 32),
            name: self.string(u32_at(&b, 40))?,
            class: self.string(u32_at(&b, 44))?,
            roundabout: u64_at(&b, 48) != 0,
        })
    }
    fn string(&self, id: u32) -> Result<String> {
        if id as u64 >= self.metadata.dictionary_count {
            return Err(invalid("dictionary id"));
        }
        let b = self.read::<12>(1, self.metadata.dictionary + id as u64 * 12)?;
        let length = u32_at(&b, 8) as usize;
        if length > 4096 {
            return Err(invalid("dictionary length"));
        }
        let mut bytes = vec![0; length];
        self.cache
            .lock()
            .unwrap()
            .read(0, u64_at(&b, 0), &mut bytes)?;
        String::from_utf8(bytes).map_err(|_| invalid("dictionary utf8"))
    }
    pub fn outgoing_range(&self, node: usize) -> Result<std::ops::Range<u64>> {
        if node >= self.manifest.nodes {
            return Err(invalid("node id"));
        }
        let b = self.read::<16>(1, self.metadata.offsets + node as u64 * 8)?;
        let a = u64_at(&b, 0);
        let z = u64_at(&b, 8);
        if a > z || z > self.metadata.edges {
            return Err(invalid("adjacency offsets"));
        }
        Ok(a..z)
    }
    pub fn outgoing_edge(&self, offset: u64) -> Result<usize> {
        if offset >= self.metadata.edges {
            return Err(invalid("adjacency range"));
        }
        let id = u64_at(&self.read::<8>(1, self.metadata.edge_ids + offset * 8)?, 0);
        if id >= self.metadata.edges {
            return Err(invalid("adjacency edge"));
        }
        Ok(id as usize)
    }
    /// Outgoing CSR records colocate original edge ID, target and integer cost.
    pub fn outgoing_record(&self, node: usize, offset: u64) -> Result<(usize, usize, u64)> {
        if self.metadata.schema == 1 {
            let id = self.outgoing_edge(offset)?;
            let raw = self.edge_record(id)?;
            if u64_at(&raw, 0) != node as u64 {
                return Err(invalid("outgoing source"));
            }
            return Ok((id, u64_at(&raw, 8) as usize, u64_at(&raw, 32)));
        }
        if offset >= self.metadata.edges {
            return Err(invalid("outgoing offset"));
        }
        let b = self.read::<24>(1, self.metadata.compact + offset * 24)?;
        let (id, to, cost) = (u64_at(&b, 0), u64_at(&b, 8), u64_at(&b, 16));
        if id >= self.metadata.edges || to >= self.metadata.nodes || cost == 0 {
            return Err(invalid("outgoing record"));
        }
        Ok((id as usize, to as usize, cost))
    }
    pub fn landmark_potentials(&self, node: usize) -> Result<[u64; 4]> {
        if node >= self.manifest.nodes {
            return Err(invalid("landmark node"));
        }
        if self.metadata.schema == 1 {
            return Ok([u64::MAX; 4]);
        }
        let raw = self.read::<32>(1, self.metadata.potentials + node as u64 * 32)?;
        Ok(std::array::from_fn(|i| u64_at(&raw, i * 8)))
    }
    /// Callback for each bounded spatial node/leaf operation, including empty scans.
    pub fn spatial_candidates(
        &self,
        bounds: [f64; 4],
        mut visit: impl FnMut(Option<usize>) -> Result<()>,
    ) -> Result<()> {
        if self.metadata.spatial_count == 0 {
            return Ok(());
        }
        let mut pending = vec![0u64];
        let mut work = 0;
        while let Some(id) = pending.pop() {
            visit(None)?;
            work += 1;
            if id >= self.metadata.spatial_count || work > self.metadata.spatial_count {
                return Err(invalid("spatial tree"));
            }
            let b = self.read::<64>(1, self.metadata.spatial + id * 64)?;
            let mut bb = [0.; 4];
            for i in 0..4 {
                bb[i] = f64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
            }
            if bb.iter().any(|v| !v.is_finite()) || bb[0] > bb[2] || bb[1] > bb[3] {
                return Err(invalid("spatial bounds"));
            }
            if bb[2] < bounds[0] || bb[0] > bounds[2] || bb[3] < bounds[1] || bb[1] > bounds[3] {
                continue;
            }
            let count = u64_at(&b, 56);
            if count == 0 {
                let left = u64_at(&b, 32);
                let right = u64_at(&b, 40);
                if left <= id || right <= id || pending.len() > 64 {
                    return Err(invalid("spatial children"));
                }
                pending.push(right);
                pending.push(left);
            } else {
                let start = u64_at(&b, 48);
                if count > 32
                    || start
                        .checked_add(count)
                        .is_none_or(|v| v > self.metadata.edges)
                {
                    return Err(invalid("spatial leaf"));
                }
                for i in start..start + count {
                    let edge = u64_at(&self.read::<8>(1, self.metadata.spatial_ids + i * 8)?, 0);
                    if edge >= self.metadata.edges {
                        return Err(invalid("spatial edge"));
                    }
                    visit(Some(edge as usize))?;
                }
            }
        }
        Ok(())
    }
    pub fn restrictions(&self, max_bytes: u64) -> Result<Vec<Vec<usize>>> {
        let mut offset = self.metadata.graph_restrictions;
        let mut result = Vec::new();
        let mut used = 65536;
        for _ in 0..self.manifest.restrictions {
            let n = u32_at(&self.read::<4>(0, offset)?, 0) as usize;
            offset += 4;
            if !(2..=256).contains(&n) {
                return Err(invalid("restriction length"));
            }
            used += 128 + n as u64 * 16;
            if used > max_bytes {
                return Err(resource_limit(
                    "query_restrictions_bytes",
                    max_bytes,
                    used,
                    "paged_query",
                ));
            }
            let mut sequence = Vec::with_capacity(n);
            for _ in 0..n {
                let id = u64_at(&self.read::<8>(0, offset)?, 0);
                offset += 8;
                if id >= self.metadata.edges {
                    return Err(invalid("restriction edge"));
                }
                sequence.push(id as usize);
            }
            result.push(sequence);
        }
        Ok(result)
    }
}
