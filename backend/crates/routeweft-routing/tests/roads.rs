use routeweft_dataset::{Edge, Graph, Manifest, Point};
use routeweft_domain::model::TravelProfile;
use routeweft_routing::{Engine, Error, QueryOptions, SearchBudget};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::time::Instant;
fn p(lon: f64, lat: f64) -> Point {
    Point { lon, lat }
}
fn e(from: usize, to: usize, cost: u64) -> Edge {
    Edge {
        from,
        to,
        way: 1,
        duration_ms: cost,
        distance_mm: cost * 10,
        name: "road".into(),
        class: "residential".into(),
        roundabout: false,
    }
}
fn engine(nodes: Vec<Point>, edges: Vec<Edge>, forbidden: Vec<Vec<usize>>) -> Engine {
    let m = Manifest {
        format: 1,
        dataset_id: "test".into(),
        builder: "test".into(),
        profile: "driving-v1".into(),
        source_sha256: "test".into(),
        source_snapshot: None,
        attribution: "synthetic".into(),
        graph_file: "graph.json".into(),
        graph_sha256: "test".into(),
        graph_bytes: 0,
        nodes: nodes.len(),
        edges: edges.len(),
        restrictions: forbidden.len(),
        diagnostics: BTreeMap::new(),
    };
    Engine::from_graph(
        m,
        Graph {
            nodes,
            edges,
            forbidden,
        },
    )
    .unwrap()
}
fn opts() -> QueryOptions {
    QueryOptions {
        radius_m: 10.,
        ..Default::default()
    }
}
#[test]
fn regional_projection_uses_query_latitude_and_load_budget_precedes_index_build() {
    use routeweft_dataset::{GraphCapacity, LoadOptions};
    let root = std::env::temp_dir().join(format!(
        "regional-routing-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let graph = Graph {
        nodes: vec![p(0., 50.), p(0.01, 50.01), p(7., 58.)],
        edges: vec![e(0, 1, 1000)],
        forbidden: vec![],
    };
    routeweft_dataset::write_with_format(
        &root,
        &graph,
        "b".repeat(64),
        None,
        BTreeMap::new(),
        routeweft_dataset::DatasetFormat::BinaryV2,
    )
    .unwrap();
    assert!(matches!(
        Engine::open(&root, LoadOptions::default()),
        Err(Error::InvalidDataset)
    ));
    let mut limits = GraphCapacity::Regional.limits();
    limits.max_resident_bytes = 1;
    assert!(matches!(
        Engine::open_with_capacity(&root, limits, GraphCapacity::Regional),
        Err(Error::BudgetExceeded)
    ));
    let engine = Engine::open_with_capacity(
        &root,
        GraphCapacity::Regional.limits(),
        GraphCapacity::Regional,
    )
    .unwrap();
    let query = p(0.004, 50.006);
    let nearest = engine
        .nearest(
            query,
            QueryOptions {
                radius_m: 1000.,
                ..Default::default()
            },
            &SearchBudget::default(),
        )
        .unwrap();
    let cos = query.lat.to_radians().cos();
    let expected = ((query.lon * cos) * (0.01 * cos) + (query.lat - 50.) * 0.01)
        / ((0.01 * cos).powi(2) + 0.01_f64.powi(2));
    assert!((nearest[0].fraction - expected).abs() < 1e-9);
    let result = engine
        .route(
            &[p(0.002, 50.002), p(0.008, 50.008)],
            QueryOptions {
                radius_m: 100.,
                ..Default::default()
            },
            &SearchBudget::default(),
        )
        .unwrap();
    assert!((result.duration_s - 0.6).abs() < 0.001);
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn partial_edge_150_distinct_cases() {
    let mut cases = 0;
    for cost in (1000..=10000).step_by(1000) {
        let router = engine(vec![p(0., 0.), p(0.001, 0.)], vec![e(0, 1, cost)], vec![]);
        for start in 0..5 {
            for end in start + 1..=5 {
                let fraction = (end - start) as f64 / 5.;
                let result = router
                    .route(
                        &[p(start as f64 * 0.0002, 0.), p(end as f64 * 0.0002, 0.)],
                        opts(),
                        &SearchBudget::default(),
                    )
                    .unwrap();
                assert!((result.duration_s - cost as f64 * fraction / 1000.).abs() < 0.001);
                assert!((result.distance_m - cost as f64 * 10. * fraction / 1000.).abs() < 0.001);
                assert_eq!(result.legs.len(), 1);
                assert_eq!(result.geometry.len(), 2);
                assert_eq!(result.steps[0].maneuver, "depart");
                assert_eq!(result.steps.last().unwrap().maneuver, "arrive");
                cases += 1;
            }
        }
    }
    assert_eq!(cases, 150);
}
#[test]
fn direction_same_edge_reverse_and_zero_legs() {
    let nodes = vec![p(0., 0.), p(0.001, 0.)];
    let points = [p(0.0008, 0.), p(0.0002, 0.)];
    let one = engine(nodes.clone(), vec![e(0, 1, 1000)], vec![]);
    assert!(matches!(
        one.route(&points, opts(), &SearchBudget::default()),
        Err(Error::NoRoute)
    ));
    let two = engine(nodes, vec![e(0, 1, 1000), e(1, 0, 1000)], vec![]);
    let result = two
        .route(&points, opts(), &SearchBudget::default())
        .unwrap();
    assert_eq!(result.duration_s, 0.6);
    let result = two
        .route(
            &[points[0], points[0], points[1]],
            opts(),
            &SearchBudget::default(),
        )
        .unwrap();
    assert_eq!(result.legs.len(), 2);
    assert_eq!(result.legs[0].distance_m, 0.);
}
#[test]
fn waypoint_does_not_erase_via_node_or_via_way_history() {
    let nodes = vec![
        p(0., 0.),
        p(0.001, 0.),
        p(0.002, 0.),
        p(0.003, 0.),
        p(0.002, 0.001),
    ];
    let edges = vec![
        e(0, 1, 1000),
        e(1, 2, 1000),
        e(2, 3, 1000),
        e(2, 4, 1000),
        e(4, 3, 1000),
    ];
    let router = engine(nodes.clone(), edges.clone(), vec![vec![0, 1, 2]]);
    let result = router
        .route(
            &[nodes[0], nodes[1], nodes[2], nodes[3]],
            opts(),
            &SearchBudget::default(),
        )
        .unwrap();
    assert_eq!(result.duration_s, 4.);
    assert_eq!(result.legs.len(), 3);
    assert!(result.geometry.contains(&nodes[4]));
    let router = engine(nodes.clone(), edges, vec![vec![0, 1]]);
    assert!(matches!(
        router.route(
            &[nodes[0], nodes[1], nodes[3]],
            opts(),
            &SearchBudget::default()
        ),
        Err(Error::NoRoute)
    ));
}
#[test]
fn mid_edge_waypoint_cannot_magic_uturn_or_reset_restrictions() {
    let router = engine(
        vec![p(0., 0.), p(0.001, 0.)],
        vec![e(0, 1, 1000), e(1, 0, 1000)],
        vec![vec![0, 1]],
    );
    assert!(matches!(
        router.route(
            &[p(0.0002, 0.), p(0.0008, 0.), p(0.0004, 0.)],
            opts(),
            &SearchBudget::default()
        ),
        Err(Error::NoRoute)
    ));
}
#[test]
fn budgets_invalid_profile_and_no_segment() {
    let router = engine(vec![p(0., 0.), p(0.001, 0.)], vec![e(0, 1, 1000)], vec![]);
    let points = [p(0., 0.), p(0.001, 0.)];
    for budget in [
        SearchBudget {
            max_work: 0,
            ..Default::default()
        },
        SearchBudget {
            max_states: 0,
            ..Default::default()
        },
        SearchBudget {
            max_queue: 0,
            ..Default::default()
        },
        SearchBudget {
            max_geometry: 0,
            ..Default::default()
        },
        SearchBudget {
            deadline: Some(Instant::now()),
            ..Default::default()
        },
    ] {
        assert!(matches!(
            router.route(&points, opts(), &budget),
            Err(Error::BudgetExceeded)
        ));
    }
    let flag = AtomicBool::new(true);
    assert!(matches!(
        router.route(
            &points,
            opts(),
            &SearchBudget {
                cancelled: Some(&flag),
                ..Default::default()
            }
        ),
        Err(Error::Cancelled)
    ));
    assert!(matches!(
        router.route(
            &points,
            QueryOptions {
                profile: TravelProfile::Walking,
                ..opts()
            },
            &SearchBudget::default()
        ),
        Err(Error::UnsupportedProfile)
    ));
    assert!(matches!(
        router.nearest(p(10., 10.), opts(), &SearchBudget::default()),
        Err(Error::NoSegment)
    ));
    for radius in [0., -1., f64::NAN, 10001.] {
        assert!(matches!(
            router.nearest(
                points[0],
                QueryOptions {
                    radius_m: radius,
                    ..opts()
                },
                &SearchBudget::default()
            ),
            Err(Error::InvalidInput)
        ));
    }
}
#[test]
fn spatial_index_matches_independent_horizontal_segment_oracle() {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    for i in 0..10 {
        nodes.push(p(0., i as f64 * 0.001));
        nodes.push(p(0.01, i as f64 * 0.001));
        edges.push(e(i * 2, i * 2 + 1, 1000));
    }
    let router = engine(nodes, edges, vec![]);
    for i in 0..30 {
        let lat = i as f64 * 0.0003;
        let near = router
            .nearest(
                p(0.005, lat),
                QueryOptions {
                    radius_m: 1000.,
                    ..Default::default()
                },
                &SearchBudget::default(),
            )
            .unwrap();
        let expected = (0..10)
            .min_by(|a, b| {
                ((lat - *a as f64 * 0.001).abs()).total_cmp(&(lat - *b as f64 * 0.001).abs())
            })
            .unwrap();
        assert_eq!(near[0].edge, expected);
        assert!((near[0].fraction - 0.5).abs() < 1e-10);
    }
}
#[test]
fn concurrent_read_queries_match() {
    let router = std::sync::Arc::new(engine(
        vec![p(0., 0.), p(0.001, 0.)],
        vec![e(0, 1, 1000)],
        vec![],
    ));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let router = router.clone();
            std::thread::spawn(move || {
                for _ in 0..20 {
                    let result = router
                        .route(&[p(0., 0.), p(0.001, 0.)], opts(), &SearchBudget::default())
                        .unwrap();
                    assert_eq!(result.duration_s, 1.);
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
}
#[test]
fn roundabout_exit_and_turn_steps() {
    let nodes = vec![
        p(0., 0.),
        p(0.001, 0.),
        p(0.001, 0.001),
        p(0., 0.001),
        p(-0.001, 0.001),
        p(0.002, 0.001),
    ];
    let mut edges = vec![
        e(0, 1, 1000),
        e(1, 2, 1000),
        e(2, 3, 1000),
        e(3, 4, 1000),
        e(2, 5, 1000),
    ];
    edges[1].roundabout = true;
    edges[2].roundabout = true;
    let router = engine(nodes.clone(), edges, vec![]);
    let result = router
        .route(&[nodes[0], nodes[4]], opts(), &SearchBudget::default())
        .unwrap();
    assert!(result
        .steps
        .iter()
        .any(|s| s.maneuver == "roundabout_enter"));
    let exit = result
        .steps
        .iter()
        .find(|s| s.maneuver == "roundabout_exit")
        .unwrap();
    assert_eq!(exit.exit, Some(2));
}

#[test]
fn overlapping_restrictions_follow_failure_links() {
    let nodes = vec![
        p(0., 0.),
        p(0.001, 0.),
        p(0.002, 0.),
        p(0.003, 0.),
        p(0.004, 0.),
        p(0.005, 0.),
    ];
    let edges = vec![
        e(0, 1, 1000),
        e(1, 2, 1000),
        e(2, 3, 1000),
        e(3, 4, 1000),
        e(4, 5, 1000),
    ];
    // Pattern 0,1,2,3,4 overlaps with suffix pattern 1,2. Terminal
    // status on the suffix must stop the longer-prefix traversal as well.
    let router = engine(nodes.clone(), edges, vec![vec![0, 1, 2, 3, 4], vec![1, 2]]);
    assert!(matches!(
        router.route(&[nodes[0], nodes[4]], opts(), &SearchBudget::default()),
        Err(Error::NoRoute)
    ));
}
#[test]
fn shape_point_is_not_a_turn_and_geometry_budget_is_enforced() {
    let nodes = vec![p(0., 0.), p(0.001, 0.), p(0.001, 0.001)];
    let router = engine(nodes.clone(), vec![e(0, 1, 1000), e(1, 2, 1000)], vec![]);
    let result = router
        .route(&[nodes[0], nodes[2]], opts(), &SearchBudget::default())
        .unwrap();
    assert_eq!(result.steps.len(), 2);
    assert!(matches!(
        router.route(
            &[nodes[0], nodes[2]],
            opts(),
            &SearchBudget {
                max_geometry: 1,
                ..Default::default()
            }
        ),
        Err(Error::BudgetExceeded)
    ));
}

#[test]
fn actual_coordinate_engine_matches_floyd_warshall() {
    for seed in 0..10 {
        let n = 6;
        let nodes: Vec<_> = (0..n)
            .map(|i| {
                let angle = i as f64 * std::f64::consts::TAU / n as f64;
                p(angle.cos() * 0.001, angle.sin() * 0.001)
            })
            .collect();
        let mut edges = Vec::new();
        let mut oracle = vec![vec![u64::MAX / 2; n]; n];
        for (i, row) in oracle.iter_mut().enumerate() {
            row[i] = 0;
            for (j, cost) in row.iter_mut().enumerate() {
                if i != j && (i * 17 + j * 13 + seed) % 4 != 0 {
                    let weight = ((i * 7 + j * 11 + seed) % 19 + 1) as u64 * 100;
                    *cost = weight;
                    edges.push(e(i, j, weight));
                }
            }
        }
        for k in 0..n {
            for i in 0..n {
                for j in 0..n {
                    oracle[i][j] = oracle[i][j].min(oracle[i][k] + oracle[k][j]);
                }
            }
        }
        let router = engine(nodes.clone(), edges, vec![]);
        for (i, row) in oracle.iter().enumerate() {
            for (j, &expected) in row.iter().enumerate() {
                let result = router
                    .route(
                        &[nodes[i], nodes[j]],
                        QueryOptions {
                            radius_m: 0.1,
                            ..opts()
                        },
                        &SearchBudget::default(),
                    )
                    .unwrap();
                assert!((result.duration_s - expected as f64 / 1000.).abs() < 1e-9);
                assert!(
                    (result.steps.iter().map(|s| s.duration_s).sum::<f64>() - result.duration_s)
                        .abs()
                        < 1e-9
                );
            }
        }
    }
}
#[test]
fn basic_turns_merge_exit_uturn_and_ten_waypoints() {
    for (lat, expected) in [(0.001, "left"), (-0.001, "right")] {
        let nodes = vec![p(0., 0.), p(0.001, 0.), p(0.001, lat)];
        let mut edges = vec![e(0, 1, 1000), e(1, 2, 1000)];
        edges[1].way = 2;
        let router = engine(nodes.clone(), edges, vec![]);
        let result = router
            .route(&[nodes[0], nodes[2]], opts(), &SearchBudget::default())
            .unwrap();
        assert!(result.steps.iter().any(|s| s.maneuver == expected));
    }
    for (first, second, expected) in [
        ("motorway_link", "motorway", "merge"),
        ("motorway", "motorway_link", "exit"),
    ] {
        let nodes = vec![p(0., 0.), p(0.001, 0.), p(0.002, 0.)];
        let mut edges = vec![e(0, 1, 1000), e(1, 2, 1000)];
        edges[1].way = 2;
        edges[0].class = first.into();
        edges[1].class = second.into();
        let router = engine(nodes.clone(), edges, vec![]);
        let result = router
            .route(&[nodes[0], nodes[2]], opts(), &SearchBudget::default())
            .unwrap();
        assert!(result.steps.iter().any(|s| s.maneuver == expected));
    }
    let nodes = vec![p(0., 0.), p(0.001, 0.)];
    let router = engine(nodes.clone(), vec![e(0, 1, 1000), e(1, 0, 1000)], vec![]);
    let points: Vec<_> = (0..10).map(|i| nodes[i % 2]).collect();
    let result = router
        .route(&points, opts(), &SearchBudget::default())
        .unwrap();
    assert_eq!(result.legs.len(), 9);
    assert_eq!(result.duration_s, 9.);
    assert!(result.steps.iter().any(|s| s.maneuver == "uturn"));
    assert!(matches!(
        router.route(&[nodes[0]; 11], opts(), &SearchBudget::default()),
        Err(Error::InvalidInput)
    ));
}

#[test]
fn cooperative_cancel_interrupts_a_running_query() {
    let count = 20000;
    let nodes: Vec<_> = (0..count).map(|i| p(i as f64 * 0.00001, 0.)).collect();
    let edges: Vec<_> = (0..count - 1).map(|i| e(i, i + 1, 1000)).collect();
    let end = nodes[count - 1];
    let router = std::sync::Arc::new(engine(nodes, edges, vec![]));
    let flag = std::sync::Arc::new(AtomicBool::new(false));
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let worker = {
        let flag = flag.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            router.route(
                &[p(0., 0.), end],
                QueryOptions {
                    radius_m: 0.0001,
                    ..opts()
                },
                &SearchBudget {
                    cancelled: Some(&flag),
                    ..Default::default()
                },
            )
        })
    };
    barrier.wait();
    std::thread::sleep(std::time::Duration::from_millis(1));
    flag.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(matches!(worker.join().unwrap(), Err(Error::Cancelled)));
}
