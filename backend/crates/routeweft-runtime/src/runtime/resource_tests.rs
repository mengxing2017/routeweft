use super::*;
use crate::{EngineLoader, EngineResourceIdentity};
use routeweft_dataset::{DatasetFormat, DatasetIdentity, Edge, Graph, Point};
use routeweft_routing::{acceleration, Engine, QueryOptions, RoutingAlgorithm, SearchBudget};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, time::Instant};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "runtime-resources-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let graph = Graph {
            nodes: (0..8)
                .map(|i| Point {
                    lon: i as f64 * 0.001,
                    lat: 0.,
                })
                .collect(),
            edges: (0..7)
                .flat_map(|i| [(i, i + 1), (i + 1, i)])
                .map(|(from, to)| Edge {
                    from,
                    to,
                    way: 1,
                    distance_mm: 10000,
                    duration_ms: 1000,
                    name: "road".into(),
                    class: "residential".into(),
                    roundabout: false,
                })
                .collect(),
            forbidden: vec![],
        };
        for (name, levels) in [("raw", 0), ("mld", 1), ("updated", 2)] {
            let path = root.join(name);
            routeweft_dataset::write_with_format(
                &path,
                &graph,
                "b".repeat(64),
                None,
                Default::default(),
                DatasetFormat::BinaryV2,
            )
            .unwrap();
            if levels > 0 {
                acceleration::customize(
                    &path,
                    Default::default(),
                    acceleration::CustomizationOptions {
                        leaf_nodes: 2,
                        levels,
                        ..Default::default()
                    },
                )
                .unwrap();
            }
        }
        Self(root)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
    fn identity(&self, name: &str) -> EngineResourceIdentity {
        let path = self.path(name);
        let manifest = routeweft_dataset::inspect_manifest(&path, Default::default()).unwrap();
        EngineResourceIdentity {
            dataset: DatasetIdentity::from(&manifest),
            mld_sha256: std::fs::read(path.join(acceleration::FILE_NAME))
                .ok()
                .map(|bytes| format!("{:x}", Sha256::digest(bytes))),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(5)
}
fn runtime(capacity: usize) -> Runtime<EngineLoader> {
    Runtime::new(
        Arc::new(EngineLoader),
        RuntimeConfig {
            resident_datasets: capacity,
            ..Default::default()
        },
    )
}
fn route(engine: &Engine, algorithm: &str) -> f64 {
    let outcome = engine.route_detailed(
        &[
            Point {
                lon: 0.0005,
                lat: 0.,
            },
            Point {
                lon: 0.0065,
                lat: 0.,
            },
        ],
        QueryOptions {
            radius_m: 1.,
            ..Default::default()
        },
        &SearchBudget::default(),
        RoutingAlgorithm::Auto,
    );
    assert_eq!(outcome.diagnostics.algorithm, algorithm);
    outcome.result.unwrap().duration_s
}

#[tokio::test]
async fn same_graph_upgrade_recustomize_and_rollback_use_distinct_engines() {
    let fixture = Fixture::new();
    let runtime = runtime(3);
    let raw = fixture.identity("raw");
    let mld = fixture.identity("mld");
    let updated = fixture.identity("updated");
    assert_eq!(raw.dataset, mld.dataset);
    assert_eq!(mld.dataset, updated.dataset);
    assert_ne!(mld.mld_sha256, updated.mld_sha256);

    let old = runtime
        .load_resources(fixture.path("raw"), raw.clone(), deadline())
        .await
        .unwrap();
    let new = runtime
        .load_resources(fixture.path("mld"), mld.clone(), deadline())
        .await
        .unwrap();
    let current = runtime
        .load_resources(fixture.path("updated"), updated, deadline())
        .await
        .unwrap();
    assert!(!Arc::ptr_eq(&old, &new));
    assert!(!Arc::ptr_eq(&new, &current));
    assert_eq!(route(&old, "dijkstra"), route(&new, "mld"));
    assert_eq!(route(&new, "mld"), route(&current, "mld"));
    let rollback = runtime
        .load_resources(fixture.path("raw"), raw, deadline())
        .await
        .unwrap();
    assert!(Arc::ptr_eq(&old, &rollback));

    // Eviction removes every completed revision, while live requests retain both
    // usable engines and admission. It must not let a new load exceed capacity.
    runtime.evict(&mld.dataset.dataset_id).await;
    assert!(runtime.registry.lock().await.is_empty());
    assert!(matches!(
        runtime
            .load_resources(fixture.path("mld"), mld.clone(), deadline())
            .await,
        Err(RuntimeError::Overloaded)
    ));
    assert_eq!(route(&old, "dijkstra"), route(&new, "mld"));
    drop((old, rollback));
    let reloaded = runtime
        .load_resources(fixture.path("mld"), mld, deadline())
        .await
        .unwrap();
    assert!(!Arc::ptr_eq(&new, &reloaded));
}

#[tokio::test]
async fn resource_identity_authenticates_files_and_staging_bypasses_cache() {
    let fixture = Fixture::new();
    let runtime = runtime(2);
    let raw = fixture.identity("raw");
    let mld = fixture.identity("mld");
    for (path, identity) in [
        (fixture.path("raw"), mld.clone()),
        (fixture.path("mld"), raw),
        (fixture.path("updated"), mld.clone()),
    ] {
        assert!(matches!(
            runtime.load_resources(path, identity, deadline()).await,
            Err(RuntimeError::InvalidDataset)
        ));
    }
    let mut invalid = mld.clone();
    invalid.mld_sha256 = Some("invalid".into());
    assert!(matches!(
        runtime
            .load_resources(fixture.path("mld"), invalid, deadline())
            .await,
        Err(RuntimeError::InvalidDataset)
    ));
    let loaded = runtime
        .load_resources(fixture.path("mld"), mld.clone(), deadline())
        .await
        .unwrap();
    // Identical fingerprints still verify every base identity field on a hit.
    let mut wrong = mld.clone();
    wrong.dataset.attribution = "Other".into();
    assert!(matches!(
        runtime
            .load_resources(fixture.path("mld"), wrong, deadline())
            .await,
        Err(RuntimeError::InvalidDataset)
    ));
    assert!(matches!(
        runtime
            .validate_resources(fixture.path("raw"), mld.clone(), deadline(), Arc::new(()))
            .await,
        Err(RuntimeError::InvalidDataset)
    ));
    let staged = runtime
        .validate_resources(fixture.path("mld"), mld, deadline(), Arc::new(()))
        .await
        .unwrap();
    assert!(!Arc::ptr_eq(&loaded, &staged));
    assert_eq!(route(&staged, "mld"), route(&loaded, "mld"));
}

#[tokio::test]
async fn legacy_load_cannot_satisfy_an_exact_absence_request() {
    let fixture = Fixture::new();
    let runtime = runtime(2);
    let raw = fixture.identity("raw");
    let legacy = runtime
        .load(fixture.path("mld"), raw.dataset.clone(), deadline())
        .await
        .unwrap();
    let exact = runtime
        .load_resources(fixture.path("raw"), raw, deadline())
        .await
        .unwrap();
    assert!(!Arc::ptr_eq(&legacy, &exact));
    assert_eq!(route(&legacy, "mld"), route(&exact, "dijkstra"));
}
