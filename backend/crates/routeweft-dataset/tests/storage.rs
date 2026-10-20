use routeweft_dataset::{hash, open, write, Edge, Graph, LoadOptions, Point};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
static COUNTER: AtomicUsize = AtomicUsize::new(0);
fn temp() -> PathBuf {
    std::env::temp_dir().join(format!(
        "routeweft-storage-{}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}
fn graph() -> Graph {
    Graph {
        nodes: vec![
            Point { lon: 0., lat: 0. },
            Point {
                lon: 0.001,
                lat: 0.,
            },
        ],
        edges: vec![Edge {
            from: 0,
            to: 1,
            way: 1,
            distance_mm: 1000,
            duration_ms: 100,
            name: "synthetic".into(),
            class: "residential".into(),
            roundabout: false,
        }],
        forbidden: vec![],
    }
}
#[test]
fn roundtrip_deterministic_identity_and_no_overwrite() {
    let a = temp();
    let b = temp();
    let source = hash(b"synthetic");
    let m = write(&a, &graph(), source.clone(), None, BTreeMap::new()).unwrap();
    let n = write(&b, &graph(), source, None, BTreeMap::new()).unwrap();
    assert_eq!(m, n);
    assert_eq!(open(&a, Default::default()).unwrap().1, graph());
    assert_eq!(
        std::fs::read(a.join("graph.json")).unwrap(),
        std::fs::read(b.join("graph.json")).unwrap()
    );
    assert!(write(&a, &graph(), "".into(), None, BTreeMap::new()).is_err());
    std::fs::remove_dir_all(a).unwrap();
    std::fs::remove_dir_all(b).unwrap();
}
#[test]
fn corrupt_and_oversized_data_are_rejected() {
    let a = temp();
    write(&a, &graph(), hash(b"x"), None, BTreeMap::new()).unwrap();
    assert!(open(
        &a,
        LoadOptions {
            max_bytes: 1,
            ..Default::default()
        }
    )
    .is_err());
    let bytes = std::fs::read(a.join("graph.json")).unwrap();
    std::fs::write(a.join("graph.json"), b"{}").unwrap();
    assert!(open(&a, Default::default()).is_err());
    std::fs::write(a.join("graph.json"), bytes).unwrap();
    let mut m: serde_json::Value =
        serde_json::from_slice(&std::fs::read(a.join("manifest.json")).unwrap()).unwrap();
    m["graph_file"] = "../escape".into();
    std::fs::write(a.join("manifest.json"), serde_json::to_vec(&m).unwrap()).unwrap();
    assert!(open(&a, Default::default()).is_err());
    std::fs::remove_dir_all(a).unwrap();
}
#[cfg(unix)]
#[test]
fn symlinked_payload_is_rejected() {
    let a = temp();
    write(&a, &graph(), hash(b"x"), None, BTreeMap::new()).unwrap();
    std::fs::rename(a.join("graph.json"), a.join("other.json")).unwrap();
    std::os::unix::fs::symlink("other.json", a.join("graph.json")).unwrap();
    assert!(open(&a, Default::default()).is_err());
    std::fs::remove_dir_all(a).unwrap();
}

#[test]
fn streamed_graph_preserves_v1_bytes_and_rejects_tampering_and_trailing_data() {
    let path = temp();
    let expected = serde_json::to_vec(&graph()).unwrap();
    let manifest = write(&path, &graph(), hash(b"streaming"), None, BTreeMap::new()).unwrap();
    assert_eq!(std::fs::read(path.join("graph.json")).unwrap(), expected);
    assert_eq!(manifest.graph_sha256, hash(&expected));
    assert_eq!(manifest.graph_bytes, expected.len() as u64);
    for replacement in [
        expected
            .iter()
            .copied()
            .chain(b"{}".iter().copied())
            .collect::<Vec<_>>(),
        expected
            .iter()
            .copied()
            .chain(b" ".iter().copied())
            .collect(),
    ] {
        std::fs::write(path.join("graph.json"), replacement).unwrap();
        assert!(open(&path, Default::default()).is_err());
    }
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn metadata_inspection_never_claims_payload_integrity() {
    let path = temp();
    let manifest = write(&path, &graph(), hash(b"metadata"), None, BTreeMap::new()).unwrap();
    std::fs::write(path.join("graph.json"), b"corrupt").unwrap();
    assert_eq!(
        routeweft_dataset::inspect_manifest(&path, Default::default()).unwrap(),
        manifest
    );
    assert!(open(&path, Default::default()).is_err());
    let mut altered = manifest;
    altered.nodes += 1; // counts are not identity fields; metadata does not validate topology.
    altered.dataset_id = "forged".into();
    std::fs::write(
        path.join("manifest.json"),
        serde_json::to_vec(&altered).unwrap(),
    )
    .unwrap();
    assert!(routeweft_dataset::inspect_manifest(&path, Default::default()).is_err());
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn streaming_publication_still_rejects_invalid_source_identity() {
    let path = temp();
    assert!(write(
        &path,
        &graph(),
        "invalid-hash".into(),
        None,
        BTreeMap::new()
    )
    .is_err());
    assert!(!path.exists());
}

#[test]
fn binary_v2_publication_is_deterministic_bounded_and_integrity_checked() {
    use routeweft_dataset::{write_with_format, DatasetFormat};
    let root = temp();
    let a = root.join("a");
    let b = root.join("b");
    let g = graph();
    let source = hash(b"v2");
    let m = write_with_format(
        &a,
        &g,
        source.clone(),
        None,
        BTreeMap::new(),
        DatasetFormat::BinaryV2,
    )
    .unwrap();
    let n = write_with_format(
        &b,
        &g,
        source,
        None,
        BTreeMap::new(),
        DatasetFormat::BinaryV2,
    )
    .unwrap();
    assert_eq!(m, n);
    assert_eq!(m.format, 2);
    assert_eq!(m.graph_file, "graph.bin");
    assert!(!a.join("graph.json").exists());
    assert_eq!(open(&a, Default::default()).unwrap().1, g);
    let payload = std::fs::read(a.join("graph.bin")).unwrap();
    assert_eq!(payload, std::fs::read(b.join("graph.bin")).unwrap());
    assert!(open(
        &a,
        LoadOptions {
            max_nodes: 1,
            ..Default::default()
        }
    )
    .is_err());
    assert!(write_with_format(
        &a,
        &g,
        hash(b"x"),
        None,
        BTreeMap::new(),
        DatasetFormat::BinaryV2
    )
    .is_err());
    let failed = root.join("invalid");
    assert!(write_with_format(
        &failed,
        &g,
        "bad".into(),
        None,
        BTreeMap::new(),
        DatasetFormat::BinaryV2
    )
    .is_err());
    assert!(!failed.exists());
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2); // failed staging cleaned
    let mut altered = payload.clone();
    *altered.last_mut().unwrap() ^= 1;
    std::fs::write(a.join("graph.bin"), altered).unwrap();
    assert!(open(&a, Default::default()).is_err());
    assert_eq!(
        routeweft_dataset::inspect_manifest(&a, Default::default()).unwrap(),
        m
    );
    std::fs::write(a.join("graph.bin"), &payload).unwrap();
    // Re-authenticate an invalid topology: hashes alone must not bypass validation.
    let dictionary_bytes = u64::from_le_bytes(payload[56..64].try_into().unwrap()) as usize;
    let edge = 64 + dictionary_bytes + 16 * g.nodes.len();
    let mut invalid = payload;
    invalid[edge + 32..edge + 40].copy_from_slice(&0_u64.to_le_bytes());
    let mut forged = m;
    forged.graph_sha256 = hash(&invalid);
    forged.dataset_id = hash(
        format!(
            "routeweft-v2\n{}\n{}\n{}\n{}",
            forged.source_sha256, forged.profile, forged.builder, forged.graph_sha256
        )
        .as_bytes(),
    );
    std::fs::write(a.join("graph.bin"), invalid).unwrap();
    std::fs::write(
        a.join("manifest.json"),
        serde_json::to_vec(&forged).unwrap(),
    )
    .unwrap();
    assert!(open(&a, Default::default()).is_err());
    #[cfg(unix)]
    {
        std::fs::rename(b.join("graph.bin"), b.join("other.bin")).unwrap();
        std::os::unix::fs::symlink("other.bin", b.join("graph.bin")).unwrap();
        assert!(open(&b, Default::default()).is_err());
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn seek_validator_authenticates_payload_and_enforces_workspace_and_format() {
    use routeweft_dataset::{validate_binary_streaming, write_with_format, DatasetFormat};
    let path = temp();
    let expected = write_with_format(
        &path,
        &graph(),
        hash(b"streaming"),
        None,
        BTreeMap::new(),
        DatasetFormat::BinaryV2,
    )
    .unwrap();
    assert_eq!(
        validate_binary_streaming(&path, Default::default(), 1024 * 1024).unwrap(),
        expected
    );
    assert!(validate_binary_streaming(&path, Default::default(), 0).is_err());
    // Coordinate change remains semantically valid; only authentication detects it.
    let payload = path.join("graph.bin");
    let mut bytes = std::fs::read(&payload).unwrap();
    let dictionary_bytes = u64::from_le_bytes(bytes[56..64].try_into().unwrap());
    let coordinate = 64 + dictionary_bytes as usize;
    bytes[coordinate..coordinate + 8].copy_from_slice(&0.002_f64.to_le_bytes());
    std::fs::write(&payload, bytes).unwrap();
    assert!(validate_binary_streaming(&path, Default::default(), 1024 * 1024).is_err());
    std::fs::remove_dir_all(&path).unwrap();
    write(&path, &graph(), hash(b"v1"), None, BTreeMap::new()).unwrap();
    assert!(matches!(
        validate_binary_streaming(&path, Default::default(), 1024 * 1024),
        Err(routeweft_dataset::Error::IncompatibleFormat)
    ));
    std::fs::remove_dir_all(path).unwrap();
}
