use routeweft_domain::model::{Coordinate, TravelProfile};
use routeweft_routing::{
    geometry::project_point_on_segment, Edge, Error, NodeEngine as Engine, SearchBudget,
};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

fn nodes(n: usize) -> Vec<Coordinate> {
    (0..n)
        .map(|i| Coordinate::new(i as f64, 0.0).unwrap())
        .collect()
}
fn edge(from: usize, to: usize, duration_ms: u64) -> Edge {
    Edge {
        from,
        to,
        duration_ms,
        distance_mm: 100,
    }
}
fn query(engine: &Engine, from: usize, to: usize) -> Result<routeweft_routing::Path, Error> {
    engine.route(from, to, TravelProfile::Driving, &SearchBudget::default())
}

#[test]
fn restriction_history_survives_merge_and_via_way() {
    // Cheapest arrival at node 3 cannot continue. A node-only search loses the valid path.
    let edges = vec![
        edge(0, 1, 1),
        edge(1, 3, 1),
        edge(0, 2, 2),
        edge(2, 3, 2),
        edge(3, 4, 1),
        edge(4, 5, 1),
    ];
    let engine = Engine::new(nodes(6), edges, vec![vec![0, 1, 4, 5]]).unwrap();
    let result = query(&engine, 0, 5).unwrap();
    assert_eq!(result.edges, vec![2, 3, 4, 5]);
    assert_eq!((result.duration_ms, result.distance_mm), (6, 400));
    assert_eq!(query(&engine, 5, 0), Err(Error::NoRoute));
}

#[test]
fn no_turn_and_invalid_restrictions() {
    let edges = vec![edge(0, 1, 1), edge(1, 2, 1)];
    let engine = Engine::new(nodes(3), edges.clone(), vec![vec![0, 1]]).unwrap();
    assert_eq!(query(&engine, 0, 2), Err(Error::NoRoute));
    assert!(Engine::new(nodes(3), edges.clone(), vec![vec![1, 0]]).is_err());
    assert!(Engine::new(nodes(3), edges, vec![vec![0, 9]]).is_err());
    assert!(Engine::new(nodes(1), vec![edge(0, 1, 1)], vec![]).is_err());
    assert!(Engine::new(nodes(1), vec![edge(0, 0, 0)], vec![]).is_err());
}

#[test]
fn budget_cancel_profile_and_overflow_are_distinct() {
    let engine = Engine::new(nodes(3), vec![edge(0, 1, u64::MAX), edge(1, 2, 1)], vec![]).unwrap();
    assert_eq!(query(&engine, 0, 2), Err(Error::CostOverflow));
    let cancelled = AtomicBool::new(true);
    let budget = SearchBudget {
        cancelled: Some(&cancelled),
        ..Default::default()
    };
    assert_eq!(
        engine.route(0, 0, TravelProfile::Driving, &budget),
        Err(Error::Cancelled)
    );
    let budget = SearchBudget {
        max_work: 0,
        ..Default::default()
    };
    assert_eq!(
        engine.route(0, 1, TravelProfile::Driving, &budget),
        Err(Error::BudgetExceeded)
    );
    let budget = SearchBudget {
        deadline: Some(Instant::now()),
        ..Default::default()
    };
    assert_eq!(
        engine.route(0, 1, TravelProfile::Driving, &budget),
        Err(Error::BudgetExceeded)
    );
    assert_eq!(
        engine.route(0, 1, TravelProfile::Cycling, &SearchBudget::default()),
        Err(Error::UnsupportedProfile)
    );
    assert_eq!(query(&engine, 9, 0), Err(Error::InvalidInput));
    assert!(query(&engine, 0, 0).unwrap().edges.is_empty());
}

#[test]
fn all_pairs_match_independent_floyd_warshall_oracle() {
    // Deterministic generated directed graphs; no dependency on the routing implementation.
    for seed in 0..20 {
        let n = 6;
        let mut oracle = vec![vec![u64::MAX / 2; n]; n];
        let mut edges = Vec::new();
        for (i, row) in oracle.iter_mut().enumerate() {
            row[i] = 0;
            for (j, cost) in row.iter_mut().enumerate() {
                if i != j && (i * 17 + j * 13 + seed) % 4 != 0 {
                    let weight = ((i * 7 + j * 11 + seed) % 19 + 1) as u64;
                    *cost = weight;
                    edges.push(edge(i, j, weight));
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
        let engine = Engine::new(nodes(n), edges, vec![]).unwrap();
        for (i, row) in oracle.iter().enumerate() {
            for (j, &expected) in row.iter().enumerate() {
                assert_eq!(query(&engine, i, j).unwrap().duration_ms, expected);
            }
        }
    }
}

#[test]
fn planar_projection_clamps_and_rejects_overflow() {
    assert_eq!(
        project_point_on_segment([0., 0.], [10., 0.], [4., 3.]),
        Some((0.4, [4., 0.]))
    );
    assert_eq!(
        project_point_on_segment([0., 0.], [10., 0.], [-2., 3.]),
        Some((0., [0., 0.]))
    );
    assert_eq!(
        project_point_on_segment([0., 0.], [10., 0.], [12., 3.]),
        Some((1., [10., 0.]))
    );
    assert_eq!(
        project_point_on_segment([1., 1.], [1., 1.], [2., 3.]),
        Some((0., [1., 1.]))
    );
    assert!(project_point_on_segment([f64::NAN, 0.], [1., 1.], [0., 0.]).is_none());
    assert!(project_point_on_segment([-f64::MAX, 0.], [f64::MAX, 0.], [0., 0.]).is_none());
}
