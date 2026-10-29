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

#[test]
fn multilevel_metrics_match_dijkstra_with_restrictions_splits_and_waypoints() {
    use routeweft_routing::{acceleration::CustomizationOptions, RoutingAlgorithm};
    for seed in 0..6 {
        let width = 16;
        let nodes: Vec<_> = (0..width * width)
            .map(|i| p((i % width) as f64 * 0.001, (i / width) as f64 * 0.001))
            .collect();
        let mut edges = Vec::new();
        for i in 0..nodes.len() {
            for j in [
                i.checked_sub(1).filter(|_| i % width > 0),
                (i % width + 1 < width).then_some(i + 1),
                i.checked_sub(width),
                (i + width < nodes.len()).then_some(i + width),
            ]
            .into_iter()
            .flatten()
            {
                edges.push(e(i, j, ((i * 17 + j * 13 + seed) % 19 + 1) as u64 * 100));
            }
        }
        let mut forbidden = Vec::new();
        for i in (0..edges.len()).step_by(23) {
            if let Some((j, second)) = edges
                .iter()
                .enumerate()
                .find(|(_, edge)| edge.from == edges[i].to && edge.to != edges[i].from)
            {
                forbidden.push(vec![i, j]);
                if let Some((k, _)) = edges
                    .iter()
                    .enumerate()
                    .find(|(_, edge)| edge.from == second.to && edge.to != second.from)
                {
                    forbidden.push(vec![i, j, k]);
                }
            }
        }
        let mut router = engine(nodes.clone(), edges, forbidden);
        let info = router
            .customize(CustomizationOptions {
                leaf_nodes: 8,
                fanout: 4,
                levels: 3 + seed % 2,
                max_boundary: 64,
                ..Default::default()
            })
            .unwrap();
        assert!(info.shortcuts.iter().sum::<usize>() > 0);
        let mut rng = seed as u64 + 42;
        for case in 0..100 {
            let mut choose = || {
                rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
                nodes[(rng >> 32) as usize % nodes.len()]
            };
            let mut points = vec![choose(), choose()];
            if case % 3 == 0 {
                points.insert(1, choose());
            }
            if case % 7 == 0 {
                points.insert(1, points[0]);
            }
            if case % 2 == 0 {
                points[0].lon = (points[0].lon + 0.0005).min((width - 1) as f64 * 0.001);
            }
            let baseline = router.route_detailed(
                &points,
                opts(),
                &SearchBudget::default(),
                RoutingAlgorithm::Dijkstra,
            );
            let accelerated = router.route_detailed(
                &points,
                opts(),
                &SearchBudget::default(),
                RoutingAlgorithm::Auto,
            );
            assert_eq!(accelerated.diagnostics.algorithm, "mld");
            match (baseline.result, accelerated.result) {
                (Ok(a), Ok(b)) => {
                    assert_eq!(
                        a.duration_s, b.duration_s,
                        "seed={seed} case={case} points={points:?}"
                    );
                    assert_eq!(a.legs.len(), b.legs.len());
                    assert!(
                        (b.steps.iter().map(|s| s.duration_s).sum::<f64>() - b.duration_s).abs()
                            < 1e-9
                    );
                }
                (Err(a), Err(b)) => assert_eq!(a, b),
                (a, b) => panic!("mismatch seed={seed} case={case}: {a:?} vs {b:?}"),
            }
        }
    }
}

#[test]
fn cell_shortcuts_skip_shape_nodes_but_recover_all_geometry_under_default_limits() {
    use routeweft_routing::{acceleration::CustomizationOptions, RoutingAlgorithm};
    let count = 10_000;
    let nodes: Vec<_> = (0..count).map(|i| p(i as f64 * 0.00001, 0.)).collect();
    let mut edges = Vec::new();
    for i in 0..count - 1 {
        edges.push(e(i, i + 1, 1000));
        edges.push(e(i + 1, i, 1000));
    }
    let points = [nodes[0], nodes[count - 1]];
    let mut router = engine(nodes, edges, vec![]);
    router
        .customize(CustomizationOptions {
            leaf_nodes: 32,
            fanout: 8,
            levels: 3,
            ..Default::default()
        })
        .unwrap();
    let budget = SearchBudget {
        max_states: 500,
        ..Default::default()
    };
    let old = router.route_detailed(&points, opts(), &budget, RoutingAlgorithm::Dijkstra);
    assert!(matches!(old.result, Err(Error::BudgetExceeded)));
    assert_eq!(old.diagnostics.failure.unwrap().reason, "states");
    let new = router.route_detailed(&points, opts(), &budget, RoutingAlgorithm::Auto);
    let route = new.result.unwrap();
    assert_eq!(route.duration_s, (count - 1) as f64);
    assert_eq!(route.geometry.len(), count);
    assert!(new.diagnostics.relaxed_shortcuts > 0);
    assert!(new.diagnostics.settled_states < 500);
    let limited = router.route_detailed(
        &points,
        opts(),
        &SearchBudget {
            max_geometry: 100,
            ..Default::default()
        },
        RoutingAlgorithm::Auto,
    );
    assert!(matches!(limited.result, Err(Error::BudgetExceeded)));
    assert_eq!(limited.diagnostics.failure.unwrap().reason, "geometry");
}

#[test]
fn uncustomized_cells_fall_back_and_weak_components_never_reject_one_way_reachability() {
    use routeweft_routing::acceleration::CustomizationOptions;
    let nodes: Vec<_> = (0..100).map(|i| p(i as f64 * 0.00001, 0.)).collect();
    let edges: Vec<_> = (0..98).map(|i| e(i, i + 1, 1000)).collect();
    let mut router = engine(nodes.clone(), edges, vec![]);
    router
        .customize(CustomizationOptions {
            leaf_nodes: 4,
            max_boundary: 1,
            ..Default::default()
        })
        .unwrap();
    let route = router
        .route(&[nodes[0], nodes[98]], opts(), &SearchBudget::default())
        .unwrap();
    assert_eq!(route.duration_s, 98.);
    assert!(matches!(
        router.route(&[nodes[98], nodes[0]], opts(), &SearchBudget::default()),
        Err(Error::NoRoute)
    ));
    let outcome = router.route_detailed(
        &[nodes[0], nodes[99]],
        QueryOptions {
            radius_m: 0.01,
            ..opts()
        },
        &SearchBudget::default(),
        Default::default(),
    );
    // Isolated nodes are not snapped; choose two separated edge components below.
    assert!(matches!(outcome.result, Err(Error::NoSegment)));
    let mut router = engine(
        vec![p(0., 0.), p(0.001, 0.), p(0.01, 0.), p(0.011, 0.)],
        vec![e(0, 1, 1), e(2, 3, 1)],
        vec![],
    );
    router.customize(Default::default()).unwrap();
    let outcome = router.route_detailed(
        &[p(0., 0.), p(0.011, 0.)],
        opts(),
        &SearchBudget::default(),
        Default::default(),
    );
    assert!(matches!(outcome.result, Err(Error::NoRoute)));
    assert_eq!(outcome.diagnostics.settled_states, 0);
}

#[test]
fn detailed_budget_reasons_are_distinct_and_keep_the_error_enum_compatible() {
    use routeweft_routing::RoutingAlgorithm;
    let router = engine(vec![p(0., 0.), p(0.001, 0.)], vec![e(0, 1, 1000)], vec![]);
    for (budget, reason) in [
        (
            SearchBudget {
                max_work: 0,
                ..Default::default()
            },
            "work",
        ),
        (
            SearchBudget {
                max_states: 0,
                ..Default::default()
            },
            "states",
        ),
        (
            SearchBudget {
                max_queue: 0,
                ..Default::default()
            },
            "queue",
        ),
        (
            SearchBudget {
                max_geometry: 0,
                ..Default::default()
            },
            "geometry",
        ),
        (
            SearchBudget {
                deadline: Some(Instant::now()),
                ..Default::default()
            },
            "deadline",
        ),
    ] {
        let outcome = router.route_detailed(
            &[p(0., 0.), p(0.001, 0.)],
            opts(),
            &budget,
            RoutingAlgorithm::Auto,
        );
        assert!(matches!(outcome.result, Err(Error::BudgetExceeded)));
        assert_eq!(outcome.diagnostics.failure.unwrap().reason, reason);
    }
}

#[test]
fn full_edge_integer_weights_above_float_precision_match_customized_costs() {
    use routeweft_routing::{acceleration::CustomizationOptions, RoutingAlgorithm};
    let nodes: Vec<_> = (0..96).map(|i| p(i as f64 * 0.00001, 0.)).collect();
    let mut edges: Vec<_> = (0..95).map(|i| e(i, i + 1, (1_u64 << 53) + 123)).collect();
    for edge in &mut edges {
        edge.distance_mm = 1;
    }
    let mut router = engine(nodes.clone(), edges, vec![]);
    router
        .customize(CustomizationOptions {
            leaf_nodes: 4,
            ..Default::default()
        })
        .unwrap();
    let points = [nodes[0], nodes[95]];
    let a = router
        .route_detailed(
            &points,
            opts(),
            &SearchBudget::default(),
            RoutingAlgorithm::Dijkstra,
        )
        .result
        .unwrap();
    let b = router
        .route_detailed(
            &points,
            opts(),
            &SearchBudget::default(),
            RoutingAlgorithm::Auto,
        )
        .result
        .unwrap();
    assert_eq!(a.duration_s, b.duration_s);
    assert_eq!(b.geometry.len(), 96);
}

#[test]
fn maximum_integer_shortcut_cost_does_not_become_unreachable() {
    use routeweft_routing::{acceleration::CustomizationOptions, RoutingAlgorithm};
    let nodes: Vec<_> = (0..6).map(|i| p(i as f64 * 0.00001, 0.)).collect();
    let mut edges: Vec<_> = (0..5).map(|i| e(i, i + 1, 1)).collect();
    edges[2].duration_ms = u64::MAX;
    let mut router = engine(nodes.clone(), edges, vec![]);
    router
        .customize(CustomizationOptions {
            leaf_nodes: 2,
            levels: 1,
            ..Default::default()
        })
        .unwrap();
    for algorithm in [RoutingAlgorithm::Dijkstra, RoutingAlgorithm::Auto] {
        assert!(matches!(
            router
                .route_detailed(
                    &[nodes[0], nodes[5]],
                    opts(),
                    &SearchBudget::default(),
                    algorithm
                )
                .result,
            Err(Error::CostOverflow)
        ));
    }
}

#[test]
fn compressed_bidirectional_chains_match_oracle_at_splits_vias_rings_and_restriction_resets() {
    use routeweft_routing::{acceleration::CustomizationOptions, RoutingAlgorithm};
    for ring in [false, true] {
        let n = 80;
        let nodes: Vec<_> = (0..n)
            .map(|i| {
                if ring {
                    let t = i as f64 / n as f64 * std::f64::consts::TAU;
                    p(0.01 * t.cos(), 0.01 * t.sin())
                } else {
                    p(i as f64 * 0.0001, 0.)
                }
            })
            .collect();
        let mut edges = Vec::new();
        for i in 0..if ring { n } else { n - 1 } {
            let j = (i + 1) % n;
            edges.push(e(i, j, 10 + (i % 7) as u64));
            edges.push(e(j, i, 13 + (i % 11) as u64));
        }
        // A no-turn whose optimum may legally reset the prefix by turning
        // around on the neighbouring shape point, plus overlapping via-way.
        let forbidden = vec![vec![40, 42], vec![20, 22, 24], vec![22, 24, 26]];
        let mut router = engine(nodes.clone(), edges, forbidden);
        router
            .customize(CustomizationOptions {
                leaf_nodes: 8,
                levels: 3,
                fanout: 4,
                ..Default::default()
            })
            .unwrap();
        let mut rng = 731_u64;
        let mut compressed = 0;
        let mut reverse = 0;
        for case in 0..400 {
            let count = if case % 19 == 0 {
                10
            } else if case % 3 == 0 {
                3
            } else {
                2
            };
            let mut points = Vec::new();
            for i in 0..count {
                rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
                let v = (rng >> 32) as usize % n;
                let point = if (case + i) % 2 == 0 && v + 1 < n {
                    p(
                        (nodes[v].lon + nodes[v + 1].lon) / 2.,
                        (nodes[v].lat + nodes[v + 1].lat) / 2.,
                    )
                } else {
                    nodes[v]
                };
                points.push(point);
            }
            if case % 23 == 0 {
                points[1] = points[0];
            }
            let a = router.route_detailed(
                &points,
                opts(),
                &SearchBudget::default(),
                RoutingAlgorithm::Dijkstra,
            );
            let b = router.route_detailed(
                &points,
                opts(),
                &SearchBudget::default(),
                RoutingAlgorithm::Auto,
            );
            compressed += b.diagnostics.compressed_edges;
            reverse += b.diagnostics.reverse_settled;
            match (a.result, b.result) {
                (Ok(a), Ok(b)) => {
                    assert_eq!(
                        a.duration_s, b.duration_s,
                        "ring={ring} case={case} points={points:?}"
                    );
                    assert_eq!(a.legs.len(), b.legs.len());
                    assert!(
                        (b.steps.iter().map(|s| s.duration_s).sum::<f64>() - b.duration_s).abs()
                            < 1e-9
                    );
                }
                (Err(a), Err(b)) => assert_eq!(a, b, "ring={ring} case={case}"),
                (a, b) => panic!("ring={ring} case={case}: {a:?} vs {b:?}"),
            }
        }
        assert!(compressed > 0);
        assert!(reverse > 0);
    }
}

#[test]
fn tiny_component_alternative_survives_candidate_cap_and_closed_component_is_retained() {
    use routeweft_routing::RoutingAlgorithm;
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    // Twelve closer isolated one-way service segments exhaust the old top-k.
    for i in 0..12 {
        let v = nodes.len();
        nodes.push(p(0., i as f64 * 0.000002));
        nodes.push(p(0.001, i as f64 * 0.000002));
        edges.push(e(v, v + 1, 100));
    }
    let main = nodes.len();
    for i in 0..20 {
        nodes.push(p(i as f64 * 0.0001, 0.0001));
        if i > 0 {
            edges.push(e(main + i - 1, main + i, 10));
            edges.push(e(main + i, main + i - 1, 10));
        }
    }
    let mut router = engine(nodes.clone(), edges, vec![]);
    router.customize(Default::default()).unwrap();
    let options = QueryOptions {
        radius_m: 30.,
        candidates: 1,
        ..Default::default()
    };
    for algorithm in [RoutingAlgorithm::Auto, RoutingAlgorithm::Dijkstra] {
        let result = router.route_detailed(
            &[p(0.0002, 0.), nodes[main + 18]],
            options,
            &SearchBudget::default(),
            algorithm,
        );
        assert_eq!(result.diagnostics.snap_fallbacks, 1);
        let route = result.result.unwrap();
        assert_eq!(route.snapped[0].lat, 0.0001);
        assert_eq!(route.duration_s, 0.16);
        let result = router.route_detailed(
            &[p(0.0002, 0.), p(0.0008, 0.)],
            options,
            &SearchBudget::default(),
            algorithm,
        );
        assert_eq!(result.diagnostics.snap_fallbacks, 0);
        assert_eq!(result.result.unwrap().snapped[0].lat, 0.);
    }
}

#[test]
fn bidirectional_restriction_graph_preserves_ten_waypoint_history_and_clique_pruning() {
    use routeweft_routing::{acceleration::CustomizationOptions, RoutingAlgorithm};
    let width = 20;
    let nodes: Vec<_> = (0..width * width)
        .map(|v| p((v % width) as f64 * 0.001, (v / width) as f64 * 0.001))
        .collect();
    let mut edges = Vec::new();
    for v in 0..nodes.len() {
        for to in [
            (v % width + 1 < width).then_some(v + 1),
            (v + width < nodes.len()).then_some(v + width),
        ]
        .into_iter()
        .flatten()
        {
            edges.push(e(v, to, 100));
            edges.push(e(to, v, 103));
        }
    }
    let forbidden: Vec<_> = (0..edges.len())
        .step_by(9)
        .filter_map(|a| {
            edges
                .iter()
                .position(|b| b.from == edges[a].to && b.to != edges[a].from)
                .map(|b| vec![a, b])
        })
        .collect();
    let mut router = engine(nodes.clone(), edges, forbidden);
    let info = router
        .customize(CustomizationOptions {
            leaf_nodes: 16,
            fanout: 4,
            levels: 3,
            max_boundary: 128,
            ..Default::default()
        })
        .unwrap();
    assert!(info.restriction_states > nodes.len());
    assert!(info.restriction_transitions > 0);
    let mut skips = 0;
    for count in [2, 3, 10] {
        let points: Vec<_> = (0..count)
            .map(|i| nodes[i * (nodes.len() - 1) / (count - 1)])
            .collect();
        let a = router
            .route_detailed(
                &points,
                opts(),
                &SearchBudget::default(),
                RoutingAlgorithm::Dijkstra,
            )
            .result
            .unwrap();
        let b = router.route_detailed(
            &points,
            opts(),
            &SearchBudget::default(),
            RoutingAlgorithm::Auto,
        );
        assert!(b.diagnostics.forward_settled > 0);
        assert!(b.diagnostics.reverse_settled > 0);
        skips += b.diagnostics.clique_skips;
        let b = b.result.unwrap();
        assert_eq!(a.duration_s, b.duration_s);
        assert_eq!(b.legs.len(), count - 1);
    }
    assert!(skips > 0);
}

#[test]
fn coverage_reports_directed_components_and_exclusion_residue() {
    let engine = engine(
        (0..6).map(|i| p(i as f64 * 0.001, 0.)).collect(),
        vec![e(0, 1, 1), e(1, 0, 1), e(1, 2, 1), e(3, 4, 1), e(4, 3, 1)],
        vec![],
    );
    let coverage = engine.coverage(&[1]).unwrap();
    assert_eq!(
        (coverage.weak_components, coverage.largest_weak_nodes),
        (3, 3)
    );
    assert_eq!(
        (coverage.strong_components, coverage.largest_strong_nodes),
        (4, 2)
    );
    assert_eq!(coverage.excluded_way_edges, 5);
    assert_eq!(engine.coverage(&[2]).unwrap().excluded_way_edges, 0);
    assert!(matches!(engine.coverage(&[2, 1]), Err(Error::InvalidInput)));
}
