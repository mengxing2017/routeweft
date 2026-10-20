//! Seek-based validation without constructing a Graph. Files must remain immutable.
use super::*;
use std::io::{Seek, SeekFrom};

fn charge(used: &mut u64, bytes: u64, limit: u64) -> Result<()> {
    let observed = used.checked_add(bytes).ok_or(Error::LimitExceeded)?;
    if observed > limit {
        return Err(resource_limit(
            "validation_workspace_bytes",
            limit,
            observed,
            "v2_validation",
        ));
    }
    *used = observed;
    Ok(())
}

pub(crate) fn validate(
    reader: &mut (impl Read + Seek),
    m: &Manifest,
    limits: LoadOptions,
    workspace: u64,
) -> Result<()> {
    if &array::<8>(reader)? != MAGIC || u64_read(reader)? != 0 {
        return Err(invalid("v2 magic/flags"));
    }
    let nodes = u64_read(reader)?;
    let edges = u64_read(reader)?;
    let sequences = u64_read(reader)?;
    let count = u64_read(reader)?;
    let refs = u64_read(reader)?;
    let dictionary_bytes = u64_read(reader)?;
    let defaults = crate::GraphCapacity::Regional.limits();
    for (name, value, limit) in [
        (
            "graph_nodes",
            nodes,
            limits.max_nodes.min(defaults.max_nodes) as u64,
        ),
        (
            "graph_edges",
            edges,
            limits.max_edges.min(defaults.max_edges) as u64,
        ),
        (
            "restriction_edges",
            refs,
            limits
                .max_restriction_edges
                .min(defaults.max_restriction_edges) as u64,
        ),
        (
            "graph_bytes",
            m.graph_bytes,
            limits.max_bytes.min(defaults.max_bytes),
        ),
    ] {
        bound(name, value, limit)?;
    }
    if nodes == 0
        || nodes != m.nodes as u64
        || edges != m.edges as u64
        || sequences != m.restrictions as u64
        || sequences > refs / 2
        || count > edges.saturating_mul(2)
        || dictionary_bytes < count.saturating_mul(4)
        || dictionary_bytes > count.saturating_mul(4100)
        || layout(nodes, edges, sequences, refs, dictionary_bytes)? != m.graph_bytes
    {
        return Err(invalid("v2 header counts/layout"));
    }
    // Fixed arrays include both adjacent dictionary strings, restriction scratch,
    // and I/O scratch. The logical budget deliberately overcharges set nodes.
    let mut used = 0;
    charge(&mut used, 98304, workspace)?;
    let mut lengths = Vec::new();
    let mut previous = [0_u8; 4096];
    let mut previous_len = None;
    let mut dictionary_used = 0_u64;
    for _ in 0..count {
        let length = u32_read(reader)? as usize;
        dictionary_used += 4 + length as u64;
        if length > 4096 || dictionary_used > dictionary_bytes {
            return Err(invalid("v2 string length"));
        }
        let mut bytes = [0_u8; 4096];
        reader.read_exact(&mut bytes[..length])?;
        std::str::from_utf8(&bytes[..length]).map_err(|_| invalid("v2 string UTF-8"))?;
        if previous_len.is_some_and(|n| previous[..n] >= bytes[..length]) {
            return Err(invalid("v2 dictionary order/duplicate"));
        }
        previous[..length].copy_from_slice(&bytes[..length]);
        previous_len = Some(length);
        // Charge before reserve, including spare capacity (no unchecked growth).
        if lengths.len() == lengths.capacity() {
            let growth = (count as usize - lengths.len()).min(1024);
            charge(&mut used, growth as u64 * 4, workspace)?;
            lengths.reserve_exact(growth);
        }
        lengths.push(length as u32);
    }
    if dictionary_used != dictionary_bytes {
        return Err(invalid("v2 dictionary bytes"));
    }
    for _ in 0..nodes {
        let p = Point {
            lon: f64::from_le_bytes(array(reader)?),
            lat: f64::from_le_bytes(array(reader)?),
        };
        p.coordinate()?;
        if p.lat.abs() > 85. {
            return Err(invalid("supported latitude is [-85,85]"));
        }
    }
    let edge_start = reader.stream_position()?;
    let mut expanded = 0_u64;
    for _ in 0..edges {
        let from = u64_read(reader)?;
        let to = u64_read(reader)?;
        let _way = u64_read(reader)?;
        let distance = u64_read(reader)?;
        let duration = u64_read(reader)?;
        let name = u32_read(reader)? as usize;
        let class = u32_read(reader)? as usize;
        let flags = u64_read(reader)?;
        let name_len = lengths
            .get(name)
            .ok_or_else(|| invalid("v2 name reference"))?;
        let class_len = lengths
            .get(class)
            .ok_or_else(|| invalid("v2 class reference"))?;
        if from >= nodes
            || to >= nodes
            || distance == 0
            || duration == 0
            || flags > 1
            || *class_len > 64
        {
            return Err(invalid("v2 edge"));
        }
        expanded += u64::from(*name_len) + u64::from(*class_len);
        bound(
            "graph_string_bytes",
            expanded,
            limits.max_bytes.min(MAX_STRING_BYTES),
        )?;
    }
    let mut unique = BTreeSet::new();
    let mut used_refs = 0_u64;
    for _ in 0..sequences {
        let length = u32_read(reader)? as usize;
        used_refs += length as u64;
        if !(2..=256).contains(&length) || used_refs > refs {
            return Err(invalid("v2 restriction length"));
        }
        let mut sequence = Vec::new();
        charge(&mut used, 128 + length as u64 * 8, workspace)?;
        sequence.reserve_exact(length);
        for _ in 0..length {
            let id = u64_read(reader)?;
            if id >= edges {
                return Err(invalid("v2 restriction reference"));
            }
            sequence.push(id);
        }
        let resume = reader.stream_position()?;
        let mut end = None;
        for &id in &sequence {
            reader.seek(SeekFrom::Start(edge_start + id * EDGE_BYTES))?;
            let from = u64_read(reader)?;
            let to = u64_read(reader)?;
            if end.is_some_and(|previous| previous != from) {
                return Err(invalid("restriction reference"));
            }
            end = Some(to);
        }
        reader.seek(SeekFrom::Start(resume))?;
        if !unique.insert(sequence) {
            return Err(invalid("restriction size/duplicate"));
        }
    }
    if used_refs != refs
        || reader.stream_position()? != m.graph_bytes
        || reader.read(&mut [0; 1])? != 0
    {
        return Err(invalid("v2 final counts/bytes"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    fn fixture() -> Graph {
        let edge = Edge {
            from: 0,
            to: 0,
            way: -1,
            distance_mm: 1,
            duration_ms: 1,
            name: "道路🚗".into(),
            class: "residential".into(),
            roundabout: true,
        };
        Graph {
            nodes: vec![Point { lon: -0., lat: 0. }],
            edges: vec![edge.clone(), edge],
            // Valid sequences need not be sorted in the existing v2 contract.
            forbidden: vec![vec![1, 0], vec![0, 1]],
        }
    }
    fn manifest(g: &Graph, bytes: u64) -> Manifest {
        Manifest {
            format: 2,
            dataset_id: String::new(),
            builder: String::new(),
            profile: String::new(),
            source_sha256: String::new(),
            source_snapshot: None,
            attribution: String::new(),
            graph_file: "graph.bin".into(),
            graph_sha256: String::new(),
            graph_bytes: bytes,
            nodes: g.nodes.len(),
            edges: g.edges.len(),
            restrictions: g.forbidden.len(),
            diagnostics: Default::default(),
        }
    }
    fn check(g: &Graph) -> Result<()> {
        let mut bytes = Vec::new();
        write(&mut bytes, g, Default::default())?;
        validate(
            &mut Cursor::new(&bytes),
            &manifest(g, bytes.len() as u64),
            Default::default(),
            1024 * 1024,
        )
    }
    #[test]
    fn streaming_semantics_match_resident_validator_for_corrupted_records() {
        let g = fixture();
        check(&g).unwrap();
        let mut bytes = Vec::new();
        write(&mut bytes, &g, Default::default()).unwrap();
        let m = manifest(&g, bytes.len() as u64);
        for offset in 0..bytes.len() {
            for mask in [1, 128, 255] {
                let mut changed = bytes.clone();
                changed[offset] ^= mask;
                let resident = read(&mut Cursor::new(&changed), &m, Default::default())
                    .and_then(|graph| graph.validate(Default::default()));
                let streamed = validate(
                    &mut Cursor::new(&changed),
                    &m,
                    Default::default(),
                    1024 * 1024,
                );
                assert_eq!(
                    resident.is_ok(),
                    streamed.is_ok(),
                    "offset={offset},mask={mask}"
                );
            }
        }
        for length in 0..bytes.len() {
            assert!(validate(
                &mut Cursor::new(&bytes[..length]),
                &m,
                Default::default(),
                1024 * 1024
            )
            .is_err());
        }
        bytes.push(0);
        assert!(validate(
            &mut Cursor::new(&bytes),
            &m,
            Default::default(),
            1024 * 1024
        )
        .is_err());
    }
    #[test]
    fn graph_semantics_and_hard_limits_are_preserved() {
        let base = fixture();
        let mut cases = Vec::new();
        let mut g = base.clone();
        g.nodes.clear();
        cases.push(g);
        let mut g = base.clone();
        g.nodes[0].lat = 86.;
        cases.push(g);
        let mut g = base.clone();
        g.nodes[0].lon = f64::NAN;
        cases.push(g);
        let mut g = base.clone();
        g.edges[0].duration_ms = 0;
        cases.push(g);
        let mut g = base.clone();
        g.edges[0].distance_mm = 0;
        cases.push(g);
        let mut g = base.clone();
        g.edges[0].from = 1;
        cases.push(g);
        let mut g = base.clone();
        g.forbidden[0] = vec![2, 0];
        cases.push(g);
        let mut g = base.clone();
        g.forbidden[0] = vec![0];
        cases.push(g);
        let mut g = base.clone();
        g.edges[0].class = "x".repeat(65);
        cases.push(g);
        for g in cases {
            assert!(g.validate(Default::default()).is_err());
            assert!(check(&g).is_err());
        }
        let mut bytes = Vec::new();
        write(&mut bytes, &base, Default::default()).unwrap();
        let m = manifest(&base, bytes.len() as u64);
        for limits in [
            LoadOptions {
                max_bytes: 1,
                ..Default::default()
            },
            LoadOptions {
                max_nodes: 0,
                ..Default::default()
            },
            LoadOptions {
                max_edges: 1,
                ..Default::default()
            },
            LoadOptions {
                max_restriction_edges: 3,
                ..Default::default()
            },
        ] {
            assert!(matches!(
                validate(&mut Cursor::new(&bytes), &m, limits, 1024 * 1024),
                Err(Error::ResourceLimit(_))
            ));
        }
    }
    #[test]
    fn rejects_duplicate_disconnected_and_over_budget_restrictions() {
        let mut g = fixture();
        g.forbidden.push(g.forbidden[0].clone());
        assert!(check(&g).is_err());
        g.forbidden.pop();
        g.nodes.push(Point {
            lon: 0.001,
            lat: 0.,
        });
        g.edges[1].to = 1;
        assert!(check(&g).is_err());
        let g = fixture();
        let mut bytes = Vec::new();
        write(&mut bytes, &g, Default::default()).unwrap();
        let m = manifest(&g, bytes.len() as u64);
        for budget in [0, 98303, 98304, 98304 + 8 + 128 + 16] {
            assert!(matches!(
                validate(&mut Cursor::new(&bytes), &m, Default::default(), budget),
                Err(Error::ResourceLimit(_))
            ));
        }
    }
}
