use routeweft_dataset::*;
use std::collections::BTreeMap;

struct MillionNodes;
impl BinarySource for MillionNodes {
    fn nodes(&self, visit: &mut dyn FnMut(Point) -> Result<()>) -> Result<()> {
        for i in 0..1_000_002 {
            visit(Point {
                lon: i as f64 * 0.000001,
                lat: 22.,
            })?;
        }
        Ok(())
    }
    fn edges(&self, visit: &mut dyn FnMut(Edge) -> Result<()>) -> Result<()> {
        visit(Edge {
            from: 0,
            to: 1_000_001,
            way: 1,
            distance_mm: 100_000_000,
            duration_ms: 10_000_000,
            name: String::new(),
            class: "residential".into(),
            roundabout: false,
        })
    }
    fn restrictions(&self, _: &mut dyn FnMut(&[usize]) -> Result<()>) -> Result<()> {
        Ok(())
    }
    fn verify(&self) -> Result<()> {
        Ok(())
    }
}
#[test]
fn regional_v2_exceeds_legacy_nodes_and_still_enforces_load_and_integrity_budgets() {
    let root = std::env::temp_dir().join(format!(
        "regional-dataset-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    let metadata = || BinaryMetadata {
        nodes: 1_000_002,
        edges: 1,
        restrictions: 0,
        source_sha256: "a".repeat(64),
        source_snapshot: None,
        diagnostics: BTreeMap::new(),
    };
    assert!(
        matches!(write_binary_streaming(&root.join("city"), &MillionNodes, metadata(), Default::default()), Err(Error::ResourceLimit(ref r)) if r.resource == "graph_nodes" && r.observed == 1_000_002)
    );
    let dataset = root.join("regional");
    let m = write_binary_streaming(
        &dataset,
        &MillionNodes,
        metadata(),
        BinaryWriteOptions {
            graph_capacity: GraphCapacity::Regional,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        matches!(open(&dataset, Default::default()), Err(Error::ResourceLimit(ref r)) if r.resource == "graph_nodes")
    );
    let mut limits = GraphCapacity::Regional.limits();
    assert_eq!(
        validate_binary_streaming(&dataset, limits, 64 * 1024 * 1024).unwrap(),
        m
    );
    assert_eq!(open(&dataset, limits).unwrap().1.nodes.len(), 1_000_002);
    // The budget must fail before the payload is opened, even if it is missing.
    std::fs::rename(dataset.join("graph.bin"), dataset.join("saved.bin")).unwrap();
    limits.max_resident_bytes = 1;
    assert!(
        matches!(open(&dataset, limits), Err(Error::ResourceLimit(ref r)) if r.resource == "estimated_resident_bytes")
    );
    std::fs::rename(dataset.join("saved.bin"), dataset.join("graph.bin")).unwrap();
    // Regional mode still validates every coordinate, not only file digests.
    use std::io::{Seek, Write};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(dataset.join("graph.bin"))
        .unwrap();
    // Header(64), empty string(4), class length+bytes(15), first longitude.
    file.seek(std::io::SeekFrom::Start(83)).unwrap();
    file.write_all(&f64::NAN.to_le_bytes()).unwrap();
    assert!(validate_binary_streaming(
        &dataset,
        GraphCapacity::Regional.limits(),
        64 * 1024 * 1024
    )
    .is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn resident_estimate_includes_unused_long_dictionary_entries() {
    let manifest = Manifest {
        format: 2,
        dataset_id: String::new(),
        builder: String::new(),
        profile: String::new(),
        source_sha256: String::new(),
        source_snapshot: None,
        attribution: String::new(),
        graph_file: String::new(),
        graph_sha256: String::new(),
        graph_bytes: 0,
        nodes: 1,
        edges: 1,
        restrictions: 0,
        diagnostics: BTreeMap::new(),
    };
    // Two 4096-byte dictionary entries are legal even though a class actually
    // referenced by an edge may only be 64 bytes. The header allows unused entries.
    let limits = LoadOptions {
        max_resident_bytes: 10_000,
        ..Default::default()
    };
    assert!(matches!(check_resident_budget(&manifest, limits, true),
        Err(Error::ResourceLimit(ref r)) if r.resource == "estimated_resident_bytes"));
}
