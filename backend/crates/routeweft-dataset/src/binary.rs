//! Strict little-endian v2 codec. Streamed I/O, with bounded in-memory Graph decode.
#[path = "stream_write.rs"]
pub(super) mod stream_write;
#[path = "streaming.rs"]
pub(super) mod streaming;
use super::{invalid, resource_limit, Edge, Error, Graph, LoadOptions, Manifest, Point, Result};
use std::collections::BTreeSet;
use std::io::{Read, Write};

const MAGIC: &[u8; 8] = b"RWFTG002";
const HEADER: u64 = 64;
const EDGE_BYTES: u64 = 56;
const MAX_STRING_BYTES: u64 = 256 * 1024 * 1024;

fn layout(nodes: u64, edges: u64, sequences: u64, refs: u64, dictionary_bytes: u64) -> Result<u64> {
    [(nodes, 16), (edges, EDGE_BYTES), (sequences, 4), (refs, 8)]
        .into_iter()
        .fold(HEADER.checked_add(dictionary_bytes), |sum, (n, size)| {
            sum.and_then(|sum| n.checked_mul(size).and_then(|n| sum.checked_add(n)))
        })
        .ok_or_else(|| invalid("v2 layout overflow"))
}
fn bound(resource: &str, observed: u64, limit: u64) -> Result<()> {
    if observed > limit {
        return Err(resource_limit(resource, limit, observed, "binary_decode"));
    }
    Ok(())
}
fn array<const N: usize>(reader: &mut impl Read) -> Result<[u8; N]> {
    let mut bytes = [0; N];
    reader.read_exact(&mut bytes).map_err(|error| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            invalid("truncated v2 graph")
        } else {
            Error::Io(error)
        }
    })?;
    Ok(bytes)
}
fn u64_read(reader: &mut impl Read) -> Result<u64> {
    Ok(u64::from_le_bytes(array(reader)?))
}
fn u32_read(reader: &mut impl Read) -> Result<u32> {
    Ok(u32::from_le_bytes(array(reader)?))
}
fn u64_write(writer: &mut impl Write, value: u64) -> Result<()> {
    writer.write_all(&value.to_le_bytes())?;
    Ok(())
}
fn u32_write(writer: &mut impl Write, value: usize) -> Result<()> {
    let value = u32::try_from(value).map_err(|_| Error::LimitExceeded)?;
    writer.write_all(&value.to_le_bytes())?;
    Ok(())
}

pub(super) fn write(writer: &mut impl Write, graph: &Graph, limits: LoadOptions) -> Result<()> {
    // Dictionary references can expand far beyond the payload size when loaded.
    // Bound restored string bytes independently of the encoded file length.
    let mut expanded_bytes = 0_u64;
    for edge in &graph.edges {
        expanded_bytes += (edge.name.len() + edge.class.len()) as u64;
        let limit = limits.max_bytes.min(MAX_STRING_BYTES);
        if expanded_bytes > limit {
            return Err(resource_limit(
                "graph_string_bytes",
                limit,
                expanded_bytes,
                "dataset_write",
            ));
        }
    }
    // Borrow strings from the caller's validated Graph; do not clone their bytes.
    let mut unique = BTreeSet::new();
    for edge in &graph.edges {
        unique.insert(edge.name.as_str());
        unique.insert(edge.class.as_str());
    }
    let strings: Vec<_> = unique.into_iter().collect();
    let dictionary_bytes = strings.iter().try_fold(0_u64, |sum, string| {
        sum.checked_add(4 + string.len() as u64)
            .ok_or_else(|| invalid("v2 dictionary overflow"))
    })?;
    let refs = graph.forbidden.iter().map(|s| s.len() as u64).sum();
    let size = layout(
        graph.nodes.len() as u64,
        graph.edges.len() as u64,
        graph.forbidden.len() as u64,
        refs,
        dictionary_bytes,
    )?;
    if size > limits.max_bytes {
        return Err(resource_limit(
            "graph_bytes",
            limits.max_bytes,
            size,
            "dataset_write",
        ));
    }
    writer.write_all(MAGIC)?;
    for n in [
        0,
        graph.nodes.len() as u64,
        graph.edges.len() as u64,
        graph.forbidden.len() as u64,
        strings.len() as u64,
        refs,
        dictionary_bytes,
    ] {
        u64_write(writer, n)?;
    }
    for string in &strings {
        u32_write(writer, string.len())?;
        writer.write_all(string.as_bytes())?;
    }
    for point in &graph.nodes {
        writer.write_all(&point.lon.to_le_bytes())?;
        writer.write_all(&point.lat.to_le_bytes())?;
    }
    for edge in &graph.edges {
        for value in [
            edge.from as u64,
            edge.to as u64,
            edge.way as u64,
            edge.distance_mm,
            edge.duration_ms,
        ] {
            u64_write(writer, value)?;
        }
        for string in [&edge.name, &edge.class] {
            let id = strings
                .binary_search(&string.as_str())
                .map_err(|_| invalid("v2 dictionary identity"))?;
            u32_write(writer, id)?;
        }
        u64_write(writer, u64::from(edge.roundabout))?;
    }
    for sequence in &graph.forbidden {
        u32_write(writer, sequence.len())?;
        for &edge in sequence {
            u64_write(writer, edge as u64)?;
        }
    }
    Ok(())
}

pub(super) fn read(
    reader: &mut impl Read,
    manifest: &Manifest,
    limits: LoadOptions,
) -> Result<Graph> {
    if &array::<8>(reader)? != MAGIC {
        return Err(invalid("v2 magic"));
    }
    if u64_read(reader)? != 0 {
        return Err(Error::IncompatibleFormat);
    }
    let nodes = u64_read(reader)?;
    let edges = u64_read(reader)?;
    let sequences = u64_read(reader)?;
    let count = u64_read(reader)?;
    let refs = u64_read(reader)?;
    let dictionary_bytes = u64_read(reader)?;
    bound(
        "graph_nodes",
        nodes,
        limits.max_nodes.min(LoadOptions::default().max_nodes) as u64,
    )?;
    bound(
        "graph_edges",
        edges,
        limits.max_edges.min(LoadOptions::default().max_edges) as u64,
    )?;
    bound(
        "restriction_edges",
        refs,
        limits
            .max_restriction_edges
            .min(LoadOptions::default().max_restriction_edges) as u64,
    )?;
    if nodes != manifest.nodes as u64
        || edges != manifest.edges as u64
        || sequences != manifest.restrictions as u64
        || sequences > refs / 2
        || count > edges.saturating_mul(2)
        || count
            .checked_mul(4)
            .is_none_or(|minimum| dictionary_bytes < minimum)
        || count
            .checked_mul(4100)
            .is_none_or(|maximum| dictionary_bytes > maximum)
        || layout(nodes, edges, sequences, refs, dictionary_bytes)? != manifest.graph_bytes
    {
        return Err(invalid("v2 header counts/layout"));
    }
    // Grow from verified records, never reserve using untrusted header counts.
    let mut strings: Vec<String> = Vec::new();
    let mut used = 0_u64;
    for _ in 0..count {
        let length = u32_read(reader)? as usize;
        used += 4 + length as u64;
        if length > 4096 || used > dictionary_bytes {
            return Err(invalid("v2 string length"));
        }
        let mut bytes = vec![0; length];
        reader.read_exact(&mut bytes).map_err(|error| {
            if error.kind() == std::io::ErrorKind::UnexpectedEof {
                invalid("truncated v2 string")
            } else {
                Error::Io(error)
            }
        })?;
        let value = String::from_utf8(bytes).map_err(|_| invalid("v2 string UTF-8"))?;
        if strings.last().is_some_and(|previous| previous >= &value) {
            return Err(invalid("v2 dictionary order/duplicate"));
        }
        strings.push(value);
    }
    if used != dictionary_bytes {
        return Err(invalid("v2 dictionary bytes"));
    }
    let mut graph = Graph {
        nodes: Vec::new(),
        edges: Vec::new(),
        forbidden: Vec::new(),
    };
    for _ in 0..nodes {
        graph.nodes.push(Point {
            lon: f64::from_le_bytes(array(reader)?),
            lat: f64::from_le_bytes(array(reader)?),
        });
    }
    let mut expanded_bytes = 0_u64;
    for _ in 0..edges {
        let from = u64_read(reader)?;
        let to = u64_read(reader)?;
        let way = u64_read(reader)? as i64;
        let distance_mm = u64_read(reader)?;
        let duration_ms = u64_read(reader)?;
        let name = u32_read(reader)? as usize;
        let class = u32_read(reader)? as usize;
        let flags = u64_read(reader)?;
        if from >= nodes || to >= nodes || flags > 1 {
            return Err(invalid("v2 edge reference/flags"));
        }
        let name = strings
            .get(name)
            .ok_or_else(|| invalid("v2 name reference"))?;
        let class = strings
            .get(class)
            .ok_or_else(|| invalid("v2 class reference"))?;
        if class.len() > 64 {
            return Err(invalid("v2 class length"));
        }
        expanded_bytes += (name.len() + class.len()) as u64;
        bound(
            "graph_string_bytes",
            expanded_bytes,
            limits.max_bytes.min(MAX_STRING_BYTES),
        )?;
        graph.edges.push(Edge {
            from: from as usize,
            to: to as usize,
            way,
            distance_mm,
            duration_ms,
            name: name.clone(),
            class: class.clone(),
            roundabout: flags != 0,
        });
    }
    let mut used_refs = 0_u64;
    for _ in 0..sequences {
        let length = u32_read(reader)? as usize;
        used_refs += length as u64;
        if !(2..=256).contains(&length) || used_refs > refs {
            return Err(invalid("v2 restriction length"));
        }
        let mut sequence = Vec::new();
        for _ in 0..length {
            let edge = u64_read(reader)?;
            if edge >= edges {
                return Err(invalid("v2 restriction reference"));
            }
            sequence.push(edge as usize);
        }
        graph.forbidden.push(sequence);
    }
    if used_refs != refs {
        return Err(invalid("v2 restriction count"));
    }
    if reader.read(&mut [0; 1])? != 0 {
        return Err(invalid("v2 trailing bytes"));
    }
    Ok(graph)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    fn sample() -> Graph {
        Graph {
            nodes: vec![
                Point { lon: -0., lat: 0. },
                Point {
                    lon: 0.001,
                    lat: 0.,
                },
                Point {
                    lon: 0.002,
                    lat: 0.,
                },
            ],
            edges: vec![
                Edge {
                    from: 0,
                    to: 1,
                    way: i64::MIN,
                    distance_mm: u64::MAX,
                    duration_ms: 1,
                    name: "道路🚗".into(),
                    class: "residential".into(),
                    roundabout: false,
                },
                Edge {
                    from: 1,
                    to: 2,
                    way: i64::MAX,
                    distance_mm: 1,
                    duration_ms: u64::MAX,
                    name: "道路🚗".into(),
                    class: "residential".into(),
                    roundabout: true,
                },
            ],
            forbidden: vec![vec![0, 1]],
        }
    }
    fn manifest(graph: &Graph, length: usize) -> Manifest {
        Manifest {
            format: 2,
            dataset_id: String::new(),
            builder: "test".into(),
            profile: "driving-v1".into(),
            source_sha256: String::new(),
            source_snapshot: None,
            attribution: String::new(),
            graph_file: "graph.bin".into(),
            graph_sha256: String::new(),
            graph_bytes: length as u64,
            nodes: graph.nodes.len(),
            edges: graph.edges.len(),
            restrictions: graph.forbidden.len(),
            diagnostics: BTreeMap::new(),
        }
    }
    #[test]
    fn fixed_little_endian_golden_record_and_lossless_numeric_roundtrip() {
        // Independently assembled one-node/zero-edge record: no dictionaries or
        // restrictions. Negative zero's IEEE-754 bit pattern must survive.
        let mut golden = b"RWFTG002".to_vec();
        golden.extend([0; 8]); // flags
        golden.extend([1, 0, 0, 0, 0, 0, 0, 0]); // nodes
        golden.extend([0; 40]); // remaining five header fields
        golden.extend([0, 0, 0, 0, 0, 0, 0, 128]); // longitude -0
        golden.extend([0; 8]); // latitude 0
        let graph = Graph {
            nodes: vec![Point { lon: -0., lat: 0. }],
            edges: vec![],
            forbidden: vec![],
        };
        let mut encoded = Vec::new();
        write(&mut encoded, &graph, Default::default()).unwrap();
        assert_eq!(encoded, golden);
        for graph in [graph, sample()] {
            let mut bytes = Vec::new();
            write(&mut bytes, &graph, Default::default()).unwrap();
            let decoded = read(
                &mut bytes.as_slice(),
                &manifest(&graph, bytes.len()),
                Default::default(),
            )
            .unwrap();
            decoded.validate(Default::default()).unwrap();
            assert_eq!(graph, decoded);
            assert_eq!(decoded.nodes[0].lon.to_bits(), (-0_f64).to_bits());
        }
    }
    #[test]
    fn dictionary_expansion_and_class_length_are_checked_before_cloning() {
        let mut g = sample();
        g.forbidden.clear();
        g.edges = vec![
            Edge {
                from: 0,
                to: 1,
                way: 1,
                distance_mm: 1,
                duration_ms: 1,
                name: "A".repeat(1024),
                class: "r".into(),
                roundabout: false
            };
            100
        ];
        let mut payload = Vec::new();
        write(&mut payload, &g, Default::default()).unwrap();
        let limits = LoadOptions {
            max_bytes: 10_000,
            ..Default::default()
        };
        assert!(payload.len() < limits.max_bytes as usize);
        let m = manifest(&g, payload.len());
        for result in [
            read(&mut payload.as_slice(), &m, limits).map(|_| ()),
            write(&mut Vec::new(), &g, limits),
        ] {
            let Error::ResourceLimit(resource) = result.unwrap_err() else {
                panic!("missing budget diagnostic")
            };
            assert_eq!(resource.resource, "graph_string_bytes");
            assert_eq!(resource.limit, 10_000);
        }
        let dictionary_bytes = u64::from_le_bytes(payload[56..64].try_into().unwrap()) as usize;
        let edge = 64 + dictionary_bytes + g.nodes.len() * 16;
        payload[edge + 44..edge + 48].copy_from_slice(&0_u32.to_le_bytes()); // name as class
        assert!(
            matches!(read(&mut payload.as_slice(), &m, Default::default()), Err(Error::InvalidDataset(message)) if message == "v2 class length")
        );
    }
    #[test]
    fn rejects_forged_layout_dictionary_references_flags_truncation_and_budgets() {
        let g = sample();
        let mut bytes = Vec::new();
        write(&mut bytes, &g, Default::default()).unwrap();
        let m = manifest(&g, bytes.len());
        let dictionary_bytes = u64::from_le_bytes(bytes[56..64].try_into().unwrap()) as usize;
        let first_edge = 64 + dictionary_bytes + g.nodes.len() * 16;
        // Header, dictionary string length, node reference, string reference,
        // unknown edge flag, restriction length and edge reference.
        for (offset, replacement) in [
            (0, b"INVALID!".to_vec()),
            (8, 1_u64.to_le_bytes().to_vec()),
            (16, u64::MAX.to_le_bytes().to_vec()),
            (40, u64::MAX.to_le_bytes().to_vec()),
            (48, u64::MAX.to_le_bytes().to_vec()),
            (56, u64::MAX.to_le_bytes().to_vec()),
            (64, 4097_u32.to_le_bytes().to_vec()),
            (first_edge, 999_u64.to_le_bytes().to_vec()),
            (first_edge + 40, 99_u32.to_le_bytes().to_vec()),
            (first_edge + 48, 2_u64.to_le_bytes().to_vec()),
            (first_edge + 112, 257_u32.to_le_bytes().to_vec()),
            (first_edge + 116, 99_u64.to_le_bytes().to_vec()),
        ] {
            let mut altered = bytes.clone();
            altered[offset..offset + replacement.len()].copy_from_slice(&replacement);
            assert!(
                read(&mut altered.as_slice(), &m, Default::default()).is_err(),
                "offset {offset}"
            );
        }
        let mut invalid_utf8 = bytes.clone();
        invalid_utf8[68] = 0xff;
        assert!(read(&mut invalid_utf8.as_slice(), &m, Default::default()).is_err());
        for length in [0, 7, 63, bytes.len() - 1] {
            assert!(read(&mut &bytes[..length], &m, Default::default()).is_err());
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(read(&mut trailing.as_slice(), &m, Default::default()).is_err());
        assert!(read(
            &mut bytes.as_slice(),
            &m,
            LoadOptions {
                max_edges: 1,
                ..Default::default()
            }
        )
        .is_err());
        assert!(read(
            &mut bytes.as_slice(),
            &m,
            LoadOptions {
                max_restriction_edges: 1,
                ..Default::default()
            }
        )
        .is_err());
        assert!(write(
            &mut Vec::new(),
            &g,
            LoadOptions {
                max_bytes: 1,
                ..Default::default()
            }
        )
        .is_err());
    }
}
