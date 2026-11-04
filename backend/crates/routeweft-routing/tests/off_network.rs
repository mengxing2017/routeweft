use routeweft_dataset::{DatasetFormat, Edge, Graph, Point};
use routeweft_routing::{
    Engine, Error, PagedEngine, QueryOptions, QueryOutcome, RoutingAlgorithm, SearchBudget,
};
use std::path::PathBuf;

fn p(lon: f64, lat: f64) -> Point {
    Point { lon, lat }
}
fn edge(from: usize, to: usize) -> Edge {
    Edge {
        from,
        to,
        way: 1,
        duration_ms: 100_000,
        distance_mm: 2_223_902,
        name: "synthetic".into(),
        class: "residential".into(),
        roundabout: false,
    }
}
struct Fixture {
    root: PathBuf,
    resident: Engine,
    paged: PagedEngine,
}
impl Fixture {
    fn new(branch: bool) -> Self {
        let mut graph = Graph {
            nodes: vec![p(0., 0.), p(0.02, 0.)],
            edges: vec![edge(0, 1), edge(1, 0)],
            forbidden: vec![],
        };
        if branch {
            graph.nodes.extend([p(0.014, 0.0001), p(0.02, 0.0001)]);
            // A bidirectional road with only an exit to the main road. Same
            // weak component; both closest directions are unreachable from A.
            graph.edges.extend([edge(2, 3), edge(3, 2), edge(2, 0)]);
        }
        Self::from_graph(graph)
    }
    fn from_graph(graph: Graph) -> Self {
        let root = std::env::temp_dir().join(format!(
            "off-network-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

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
            serde_json::json!({"test":"off_network"}),
        )
        .unwrap();
        let mut resident = Engine::open(&root, Default::default()).unwrap();
        resident.customize(Default::default()).unwrap();
        let paged = PagedEngine::open(
            &root,
            Default::default(),
            Default::default(),
            Default::default(),
        )
        .unwrap();
        Self {
            root,
            resident,
            paged,
        }
    }
    fn route(
        &self,
        mode: usize,
        points: &[Point],
        options: QueryOptions,
        budget: &SearchBudget<'_>,
    ) -> QueryOutcome {
        let algorithm = if mode % 2 == 0 {
            RoutingAlgorithm::Auto
        } else {
            RoutingAlgorithm::Dijkstra
        };
        if mode < 2 {
            self.resident
                .route_detailed(points, options, budget, algorithm)
        } else {
            self.paged
                .route_detailed(points, options, budget, algorithm)
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

#[test]
fn reachable_alternative_survives_reverse_edge_candidate_crowding() {
    let f = Fixture::new(true);
    let points = [p(0.002, 0.), p(0.018, 0.0001)];
    let options = QueryOptions {
        candidates: 2,
        ..Default::default()
    };
    for mode in 0..4 {
        let outcome = f.route(mode, &points, options, &SearchBudget::default());
        let route = outcome.result.unwrap();
        assert_eq!(route.snapped[1].lat, 0.);
        assert!((route.duration_s - 80.).abs() < 0.001);
        assert_eq!(route.distance_scope, "road_only");
        assert_eq!(route.snap_details.len(), 2);
        let detail = &route.snap_details[1];
        assert_eq!(detail.requested_coordinate, points[1]);
        assert_eq!(detail.snapped_coordinate, route.snapped[1]);
        assert!((detail.snap_distance_m - 11.119508).abs() < 0.001);
        assert!(detail.fallback_used);
        assert_eq!(detail.snap_method, "reachable_alternative");
        assert_eq!(detail.arrival_status, "nearby");
        assert_eq!(detail.access_status, "unknown");
        assert!(outcome.diagnostics.snap_fallbacks > 0);
        // Public nearest keeps the existing directed-edge count contract.
        assert_eq!(
            f.resident
                .nearest(points[1], options, &SearchBudget::default())
                .unwrap()
                .len(),
            2
        );
    }
}

#[test]
fn nearest_feasible_route_and_strict_vias_are_preserved() {
    let f = Fixture::new(true);
    for mode in 0..4 {
        // A route within the closed component remains there even though the
        // nearby main road would offer a cheaper road-only cost.
        let points = [p(0.015, 0.0001), p(0.019, 0.0001)];
        let route = f
            .route(mode, &points, Default::default(), &SearchBudget::default())
            .result
            .unwrap();
        assert!(route.snapped.iter().all(|p| p.lat == 0.0001));
        assert!(route.snap_details.iter().all(|d| !d.fallback_used));
        let via = [p(0.002, 0.), p(0.018, 0.0001), p(0.019, 0.)];
        // Endpoint alternatives cannot silently move the inaccessible via.
        assert!(matches!(
            f.route(
                mode,
                &via,
                QueryOptions {
                    radius_m: 20.,
                    ..Default::default()
                },
                &SearchBudget::default()
            )
            .result,
            Err(Error::NoRoute)
        ));
        // A caller's tighter radius is never widened automatically.
        assert!(matches!(
            f.route(
                mode,
                &[p(0.002, 0.), p(0.018, 0.0001)],
                QueryOptions {
                    radius_m: 11.,
                    ..Default::default()
                },
                &SearchBudget::default()
            )
            .result,
            Err(Error::NoRoute)
        ));
    }
}

#[test]
fn off_network_radius_boundaries_and_explicit_expansion_keep_road_costs() {
    let f = Fixture::new(false);
    for mode in 0..4 {
        for offset in [
            49., 50., 51., 199., 200., 201., 999., 1000., 1001., 2999., 3001.,
        ] {
            for radius in [1000., 3000.] {
                let points = [
                    p(0.002, 0.),
                    p(0.018, (offset / 6_371_008.8_f64).to_degrees()),
                ];
                let outcome = f.route(
                    mode,
                    &points,
                    QueryOptions {
                        radius_m: radius,
                        ..Default::default()
                    },
                    &SearchBudget::default(),
                );
                if offset > radius {
                    assert!(
                        matches!(outcome.result, Err(Error::NoSegment)),
                        "{mode}/{offset}/{radius}"
                    );
                } else {
                    let route = outcome.result.unwrap();
                    assert_eq!(route.snapped[1].lat, 0.);
                    assert!((route.snap_details[1].snap_distance_m - offset).abs() < 1e-6);
                    assert!((route.distance_m - 1779.122).abs() < 0.001);
                    assert!((route.duration_s - 80.).abs() < 0.001);
                    assert!(!route.snap_details[1].fallback_used);
                    assert_eq!(route.snap_details[1].access_status, "unknown");
                }
            }
        }
    }
}

#[test]
fn fallback_attempts_share_work_budget_and_do_not_hide_cancellation() {
    let f = Fixture::new(true);
    let points = [p(0.002, 0.), p(0.018, 0.0001)];
    for mode in 0..4 {
        let baseline = f.route(mode, &points, Default::default(), &SearchBudget::default());
        assert!(baseline.result.is_ok());
        let budget = SearchBudget {
            max_work: baseline.diagnostics.work - 1,
            ..Default::default()
        };
        assert!(matches!(
            f.route(mode, &points, Default::default(), &budget).result,
            Err(Error::BudgetExceeded)
        ));
        let cancelled = std::sync::atomic::AtomicBool::new(true);
        let budget = SearchBudget {
            cancelled: Some(&cancelled),
            ..Default::default()
        };
        assert!(matches!(
            f.route(mode, &points, Default::default(), &budget).result,
            Err(Error::Cancelled)
        ));
    }
}

#[test]
fn parallel_edges_keep_distinct_turn_histories() {
    let f = Fixture::from_graph(Graph {
        nodes: vec![p(0., 0.), p(0.01, 0.), p(0.02, 0.)],
        edges: vec![edge(0, 1), edge(0, 1), edge(1, 2)],
        forbidden: vec![vec![0, 2]],
    });
    for mode in 0..4 {
        let route = f
            .route(
                mode,
                &[p(0.002, 0.), p(0.018, 0.)],
                QueryOptions {
                    candidates: 1,
                    radius_m: 1.,
                    ..Default::default()
                },
                &SearchBudget::default(),
            )
            .result
            .unwrap();
        assert!((route.duration_s - 160.).abs() < 0.001);
        assert!(route.snap_details.iter().all(|d| !d.fallback_used));
    }
}

#[test]
fn inaccessible_start_can_use_nearby_road_without_reversing_a_one_way() {
    let f = Fixture::from_graph(Graph {
        nodes: vec![p(0., 0.), p(0.02, 0.), p(0.018, 0.0001)],
        edges: vec![edge(0, 1), edge(1, 0), edge(0, 2)],
        forbidden: vec![],
    });
    for mode in 0..4 {
        let route = f
            .route(
                mode,
                &[p(0.018, 0.0001), p(0.002, 0.)],
                Default::default(),
                &SearchBudget::default(),
            )
            .result
            .unwrap();
        assert_eq!(route.snapped[0].lat, 0.);
        assert!(route.snap_details[0].fallback_used);
        assert!(!route.snap_details[1].fallback_used);
        assert!((route.duration_s - 80.).abs() < 0.001);
    }
}

#[test]
fn component_fallback_cannot_collapse_distinct_endpoints_but_real_zero_legs_work() {
    let f = Fixture::from_graph(Graph {
        nodes: vec![
            p(0., 0.),
            p(0.001, 0.),
            p(0.002, 0.),
            p(-0.0001, 0.0001),
            p(-0.0002, 0.0002),
            p(-0.0001, -0.0001),
            p(-0.0002, -0.0002),
        ],
        edges: vec![
            edge(0, 1),
            edge(1, 0),
            edge(1, 2),
            edge(2, 1),
            edge(3, 4),
            edge(4, 3),
            edge(5, 6),
            edge(6, 5),
        ],
        forbidden: vec![],
    });
    let straight = Fixture::new(false);
    for mode in 0..4 {
        let outcome = f.route(
            mode,
            &[p(-0.0001, 0.0001), p(-0.0001, -0.0001)],
            QueryOptions {
                radius_m: 20.,
                ..Default::default()
            },
            &SearchBudget::default(),
        );
        assert!(matches!(outcome.result, Err(Error::NoRoute)));
        let route = straight
            .route(
                mode,
                &[p(0.005, 0.001), p(0.005, -0.001)],
                Default::default(),
                &SearchBudget::default(),
            )
            .result
            .unwrap();
        assert_eq!(route.distance_m, 0.);
        assert_eq!(route.geometry.len(), 1);
        assert!(route
            .snap_details
            .iter()
            .all(|d| d.access_status == "unknown"));
    }
}
