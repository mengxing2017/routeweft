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
