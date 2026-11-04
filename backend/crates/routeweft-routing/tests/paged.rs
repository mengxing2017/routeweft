use routeweft_dataset::{DatasetFormat, Edge, Graph, LoadOptions, Point};
use routeweft_routing::{
    Engine, Error, PagedEngine, PagedOptions, QueryOptions, RoutingAlgorithm, SearchBudget,
};
use std::path::PathBuf;
struct Fixture(PathBuf);
impl Fixture {
    fn new(count: usize, restriction: bool) -> Self {
        let root = std::env::temp_dir().join(format!(
            "paged-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let nodes = (0..count)
            .map(|i| Point {
                lon: i as f64 * 0.00001,
                lat: 0.,
            })
            .collect();
        let edges = (0..count - 1)
            .flat_map(|i| [(i, i + 1), (i + 1, i)])
            .map(|(from, to)| Edge {
                from,
                to,
                way: 1,
                distance_mm: 1112,
                duration_ms: 1000,
                name: "shared name".into(),
                class: "residential".into(),
                roundabout: false,
            })
            .collect();
        let graph = Graph {
            nodes,
            edges,
            forbidden: if restriction {
                vec![vec![0, 2, 4]]
            } else {
                vec![]
            },
        };
        routeweft_dataset::write_with_format(
            &root,
            &graph,
            "a".repeat(64),
            None,
            Default::default(),
            DatasetFormat::BinaryV2,
        )
        .unwrap();
        routeweft_dataset::paged::prepare_query_index(
            &root,
            Default::default(),
            serde_json::json!({"test":true}),
        )
        .unwrap();
        Self(root)
    }
    fn paged(&self) -> PagedEngine {
        PagedEngine::open(
            &self.0,
            Default::default(),
            Default::default(),
            PagedOptions {
                cache_bytes: 16 * (4096 + 256),
                ..Default::default()
            },
        )
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}
#[test]
fn local_decoding_and_page_cache_stay_bounded_when_coverage_grows() {
    let points = [
        Point {
            lon: 0.000005,
            lat: 0.,
        },
        Point {
            lon: 0.000075,
            lat: 0.,
        },
        Point {
            lon: 0.000095,
            lat: 0.,
        },
    ];
    let mut work = Vec::new();
    for count in [100, 100000] {
        let f = Fixture::new(count, false);
        f.paged().verify_indexes(64 * 1024 * 1024).unwrap();
        let paged = f.paged();
        let before = paged.page_stats();
        assert_eq!(before.cached_pages, 0);
        let outcome = paged.route_detailed(
            &points,
            QueryOptions {
                radius_m: 1.,
                ..Default::default()
            },
            &SearchBudget::default(),
            RoutingAlgorithm::Auto,
        );
        let routed = outcome.result.unwrap();
        assert_eq!(routed.legs.len(), 2);
        assert_eq!(routed.duration_s, 9.);
        assert_eq!(outcome.diagnostics.algorithm, "astar");
        assert_eq!(outcome.diagnostics.working_graph_edges, 0);
        assert!((1..100).contains(&outcome.diagnostics.decoded_path_edges));
        let stats = paged.page_stats();
        assert!(stats.cached_pages <= 16);
        assert!(stats.bytes_read < 4 * 1024 * 1024);
        work.push(outcome.diagnostics.decoded_path_edges);
        if count == 100 {
            let engine = Engine::open(&f.0, Default::default()).unwrap();
            let reference = engine
                .route(
                    &points,
                    QueryOptions {
                        radius_m: 1.,
                        ..Default::default()
                    },
                    &SearchBudget::default(),
                )
                .unwrap();
            assert_eq!(routed.duration_s, reference.duration_s);
            assert_eq!(routed.geometry, reference.geometry);
        }
    }
    assert_eq!(work[0], work[1]);
}
#[test]
fn restriction_history_vias_and_cross_page_paths_match_native_dijkstra() {
    let f = Fixture::new(2000, true);
    let paged = f.paged();
    let engine = Engine::open(&f.0, Default::default()).unwrap();
    for points in [
        vec![
            Point {
                lon: 0.000005,
                lat: 0.,
            },
            Point {
                lon: 0.000015,
                lat: 0.,
            },
            Point {
                lon: 0.000035,
                lat: 0.,
            },
        ],
        vec![
            Point {
                lon: 0.008005,
                lat: 0.,
            },
            Point {
                lon: 0.008095,
                lat: 0.,
            },
        ],
    ] {
        let options = QueryOptions {
            radius_m: 1.,
            ..Default::default()
        };
        let budget = SearchBudget::default();
        let a = engine.route_detailed(&points, options, &budget, RoutingAlgorithm::Dijkstra);
        let b = paged.route_detailed(&points, options, &budget, RoutingAlgorithm::Dijkstra);
        match (a.result, b.result) {
            (Ok(a), Ok(b)) => {
                assert_eq!(a.duration_s, b.duration_s);
                assert_eq!(a.geometry, b.geometry);
                assert_eq!(a.legs.len(), b.legs.len());
            }
            (Err(a), Err(b)) => assert_eq!(a, b),
            x => panic!("{x:?}"),
        }
    }
}
#[test]
fn missing_corrupt_and_budgeted_indexes_fail_without_resident_fallback() {
    let f = Fixture::new(100, false);
    let options = PagedOptions {
        query_workspace_bytes: 131072,
        ..Default::default()
    };
    let paged = PagedEngine::open(&f.0, Default::default(), Default::default(), options).unwrap();
    let points = [
        Point {
            lon: 0.000005,
            lat: 0.,
        },
        Point {
            lon: 0.000095,
            lat: 0.,
        },
    ];
    assert_eq!(
        paged
            .route_detailed(
                &points,
                Default::default(),
                &SearchBudget::default(),
                RoutingAlgorithm::Auto
            )
            .result
            .unwrap_err(),
        Error::BudgetExceeded
    );
    let path = f.0.join(routeweft_dataset::paged::INDEX_FILE);
    let bytes = std::fs::read(&path).unwrap();
    let mut corrupt = bytes.clone();
    *corrupt.last_mut().unwrap() ^= 1;
    std::fs::write(&path, corrupt).unwrap();
    assert!(PagedEngine::open(
        &f.0,
        Default::default(),
        Default::default(),
        Default::default()
    )
    .is_err());
    std::fs::remove_file(path).unwrap();
    assert!(PagedEngine::open(
        &f.0,
        LoadOptions::default(),
        Default::default(),
        Default::default()
    )
    .is_err());
}

#[test]
fn compact_astar_keeps_a_required_detour_far_outside_endpoint_boxes() {
    let f = Fixture::new(5, false);
    let graph = Graph {
        nodes: vec![
            Point { lon: 0., lat: 0. },
            Point {
                lon: 0.001,
                lat: 0.,
            },
            Point {
                lon: 0.002,
                lat: 0.,
            },
            Point {
                lon: 0.003,
                lat: 0.,
            },
            Point { lon: 0.5, lat: 0.5 },
        ],
        edges: [
            (0, 1, 1000),
            (1, 2, 1000),
            (2, 3, 1000),
            (1, 4, 20000),
            (4, 2, 20000),
        ]
        .into_iter()
        .map(|(from, to, duration_ms)| Edge {
            from,
            to,
            way: 1,
            distance_mm: 10000,
            duration_ms,
            name: "detour".into(),
            class: "road".into(),
            roundabout: false,
        })
        .collect(),
        forbidden: vec![vec![0, 1]],
    };
    // Use a fresh staging directory for a different immutable base identity.
    let root = f.0.join("detour");
    routeweft_dataset::write_with_format(
        &root,
        &graph,
        "b".repeat(64),
        None,
        Default::default(),
        DatasetFormat::BinaryV2,
    )
    .unwrap();
    routeweft_dataset::paged::prepare_query_index(&root, Default::default(), Default::default())
        .unwrap();
    let paged = PagedEngine::open(
        &root,
        Default::default(),
        Default::default(),
        Default::default(),
    )
    .unwrap();
    let engine = Engine::open(&root, Default::default()).unwrap();
    let points = [
        Point {
            lon: 0.0005,
            lat: 0.,
        },
        Point {
            lon: 0.0025,
            lat: 0.,
        },
    ];
    let options = QueryOptions {
        radius_m: 1.,
        ..Default::default()
    };
    let a = engine
        .route(&points, options, &SearchBudget::default())
        .unwrap();
    let b = paged
        .route_detailed(
            &points,
            options,
            &SearchBudget::default(),
            RoutingAlgorithm::Auto,
        )
        .result
        .unwrap();
    assert_eq!(a.duration_s, 41.);
    assert_eq!(a.duration_s, b.duration_s);
    assert_eq!(a.geometry, b.geometry);
    assert!(b.geometry.iter().any(|p| p.lat == 0.5));
}

#[test]
fn semantic_verification_rejects_rehashed_index_with_wrong_adjacency() {
    use sha2::{Digest, Sha256};
    let f = Fixture::new(100, false);
    let graph = routeweft_dataset::paged::PagedGraph::open(
        &f.0,
        Default::default(),
        1024 * 1024,
        64 * 1024 * 1024,
    )
    .unwrap();
    assert!(graph.verify_indexes(1).is_err());
    let offset = graph.metadata.edge_ids as usize;
    drop(graph);
    let file = f.0.join(routeweft_dataset::paged::INDEX_FILE);
    let mut bytes = std::fs::read(&file).unwrap();
    bytes[offset..offset + 8].copy_from_slice(&1u64.to_le_bytes());
    let length = bytes.len() - 32;
    let checksum = Sha256::digest(&bytes[..length]);
    bytes[length..].copy_from_slice(&checksum);
    std::fs::write(file, bytes).unwrap();
    let engine = f.paged();
    assert!(engine.verify_indexes(64 * 1024 * 1024).is_err());
}

#[test]
fn authenticated_header_with_overflowing_base_offset_is_rejected() {
    use sha2::{Digest, Sha256};
    let f = Fixture::new(100, false);
    let file = f.0.join(routeweft_dataset::paged::INDEX_FILE);
    let mut bytes = std::fs::read(&file).unwrap();
    let length = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let mut metadata: serde_json::Value = serde_json::from_slice(&bytes[12..12 + length]).unwrap();
    metadata["graph_nodes"] = serde_json::json!(u64::MAX);
    let encoded = serde_json::to_vec(&metadata).unwrap();
    bytes[8..12].copy_from_slice(&(encoded.len() as u32).to_le_bytes());
    bytes[12..4096].fill(0);
    bytes[12..12 + encoded.len()].copy_from_slice(&encoded);
    let length = bytes.len() - 32;
    let checksum = Sha256::digest(&bytes[..length]);
    bytes[length..].copy_from_slice(&checksum);
    std::fs::write(file, bytes).unwrap();
    assert!(PagedEngine::open(
        &f.0,
        Default::default(),
        Default::default(),
        Default::default()
    )
    .is_err());
}

#[test]
fn legacy_query_index_uses_direct_search_without_silent_resident_fallback() {
    use sha2::{Digest, Sha256};
    let f = Fixture::new(200, false);
    let file = f.0.join(routeweft_dataset::paged::INDEX_FILE);
    let mut bytes = std::fs::read(&file).unwrap();
    let n = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let mut m: serde_json::Value = serde_json::from_slice(&bytes[12..12 + n]).unwrap();
    let end = m["compact"].as_u64().unwrap() as usize;
    m["schema"] = 1.into();
    m["bytes"] = (end + 32).into();
    for key in ["compact", "potentials", "landmark_count"] {
        m.as_object_mut().unwrap().remove(key);
    }
    let encoded = serde_json::to_vec(&m).unwrap();
    bytes.truncate(end);
    bytes[..8].copy_from_slice(b"RWFTQI01");
    bytes[8..12].copy_from_slice(&(encoded.len() as u32).to_le_bytes());
    bytes[12..4096].fill(0);
    bytes[12..12 + encoded.len()].copy_from_slice(&encoded);
    let digest = Sha256::digest(&bytes);
    bytes.extend_from_slice(&digest);
    std::fs::write(file, bytes).unwrap();
    let engine = f.paged();
    engine.verify_indexes(64 * 1024 * 1024).unwrap();
    let q = engine.route_detailed(
        &[
            Point {
                lon: 0.000005,
                lat: 0.,
            },
            Point {
                lon: 0.001985,
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
    assert_eq!(q.result.unwrap().duration_s, 198.);
    assert_eq!(q.diagnostics.algorithm, "dijkstra");
    assert_eq!(q.diagnostics.working_graph_edges, 0);
    assert!(q.diagnostics.decoded_path_edges > 0);
}

#[test]
fn compact_cost_and_landmark_semantics_are_verified_after_rehashing() {
    use sha2::{Digest, Sha256};
    for corrupt_potential in [false, true] {
        let f = Fixture::new(100, false);
        let graph = routeweft_dataset::paged::PagedGraph::open(
            &f.0,
            Default::default(),
            1024 * 1024,
            64 * 1024 * 1024,
        )
        .unwrap();
        let offset = if corrupt_potential {
            graph.metadata.potentials
        } else {
            graph.metadata.compact + 16
        } as usize;
        drop(graph);
        let file = f.0.join(routeweft_dataset::paged::INDEX_FILE);
        let mut bytes = std::fs::read(&file).unwrap();
        bytes[offset..offset + 8].copy_from_slice(&999999999u64.to_le_bytes());
        let end = bytes.len() - 32;
        let digest = Sha256::digest(&bytes[..end]);
        bytes[end..].copy_from_slice(&digest);
        std::fs::write(file, bytes).unwrap();
        assert!(f.paged().verify_indexes(64 * 1024 * 1024).is_err());
    }
}

#[test]
fn astar_direct_search_matches_restricted_oracle_for_partial_edges_and_vias() {
    let f = Fixture::new(1200, true);
    let paged = f.paged();
    let native = Engine::open(&f.0, Default::default()).unwrap();
    let options = QueryOptions {
        radius_m: 1.,
        ..Default::default()
    };
    for i in 0..40 {
        let points = [
            Point {
                lon: (i * 7) as f64 * 0.00001 + 0.000003,
                lat: 0.,
            },
            Point {
                lon: (i * 7 + 25) as f64 * 0.00001 + 0.000007,
                lat: 0.,
            },
            Point {
                lon: (i * 7 + 37) as f64 * 0.00001 + 0.000002,
                lat: 0.,
            },
        ];
        let baseline = native.route_detailed(
            &points,
            options,
            &SearchBudget::default(),
            RoutingAlgorithm::Dijkstra,
        );
        let query = paged.route_detailed(
            &points,
            options,
            &SearchBudget::default(),
            RoutingAlgorithm::Auto,
        );
        match (baseline.result, query.result) {
            (Ok(a), Ok(b)) => {
                assert_eq!(a.duration_s, b.duration_s);
                assert_eq!(a.geometry, b.geometry);
                assert_eq!(
                    serde_json::to_value(a.steps).unwrap(),
                    serde_json::to_value(b.steps).unwrap()
                );
            }
            (Err(a), Err(b)) => assert_eq!(a, b),
            other => panic!("{other:?}"),
        }
        assert_eq!(query.diagnostics.working_graph_edges, 0);
        assert!(query.diagnostics.phases.iter().any(|p| p.phase == "search"));
    }
}

#[test]
fn long_names_are_decoded_only_on_final_path_under_small_workspace() {
    let f = Fixture::new(2, false);
    let path = f.0.join("grid");
    let n = 40usize;
    let nodes = (0..n * n)
        .map(|id| Point {
            lon: (id % n) as f64 * 0.001,
            lat: (id / n) as f64 * 0.001,
        })
        .collect();
    let mut edges = Vec::new();
    for id in 0..n * n {
        for to in [
            id.checked_sub(n),
            if id % n > 0 { Some(id - 1) } else { None },
            if id % n + 1 < n { Some(id + 1) } else { None },
            if id + n < n * n { Some(id + n) } else { None },
        ]
        .into_iter()
        .flatten()
        {
            edges.push(Edge {
                from: id,
                to,
                way: id as i64,
                duration_ms: 1000 + ((id * 13 + to * 7) % 97) as u64,
                distance_mm: 100000,
                name: "n".repeat(4096),
                class: "residential".into(),
                roundabout: false,
            });
        }
    }
    let graph = Graph {
        nodes,
        edges,
        forbidden: vec![],
    };
    routeweft_dataset::write_with_format(
        &path,
        &graph,
        "b".repeat(64),
        None,
        Default::default(),
        DatasetFormat::BinaryV2,
    )
    .unwrap();
    drop(graph);
    routeweft_dataset::paged::prepare_query_index(&path, Default::default(), Default::default())
        .unwrap();
    let engine = PagedEngine::open(
        &path,
        Default::default(),
        Default::default(),
        PagedOptions {
            cache_bytes: 1024 * 1024,
            query_workspace_bytes: 4 * 1024 * 1024,
            ..Default::default()
        },
    )
    .unwrap();
    let native = Engine::open(&path, Default::default()).unwrap();
    let points = [
        Point { lon: 0., lat: 0. },
        Point {
            lon: 0.039,
            lat: 0.039,
        },
    ];
    let options = QueryOptions {
        radius_m: 1.,
        ..Default::default()
    };
    let a = native
        .route(&points, options, &SearchBudget::default())
        .unwrap();
    let b = engine.route_detailed(
        &points,
        options,
        &SearchBudget::default(),
        RoutingAlgorithm::Auto,
    );
    assert_eq!(a.duration_s, b.result.as_ref().unwrap().duration_s);
    assert!(b.diagnostics.decoded_path_edges < 100);
    assert_eq!(b.diagnostics.working_graph_edges, 0);
    assert!(b.diagnostics.paged_workspace.unwrap().planned_peak_bytes <= 4 * 1024 * 1024);
    let before = engine.page_stats().bytes_read;
    let budget = SearchBudget {
        max_states: 1,
        ..Default::default()
    };
    let failed = engine.route_detailed(&points, options, &budget, RoutingAlgorithm::Auto);
    assert_eq!(failed.result.unwrap_err(), Error::BudgetExceeded);
    assert_eq!(failed.diagnostics.failure.unwrap().phase, "search");
    assert_eq!(
        failed
            .diagnostics
            .phases
            .iter()
            .map(|p| p.cache_bytes_read)
            .sum::<u64>(),
        engine.page_stats().bytes_read - before
    );
}
