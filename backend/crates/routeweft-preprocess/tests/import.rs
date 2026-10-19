use routeweft_dataset::Point;
use routeweft_preprocess::{
    graph,
    pbf::{Member, Node, Osm, Relation, Tags, Way},
};
fn tags(values: &[(&str, &str)]) -> Tags {
    values
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}
fn network() -> Osm {
    let mut osm = Osm::default();
    for id in 1..=6 {
        osm.nodes.insert(
            id,
            Node {
                id,
                point: Point {
                    lon: id as f64 * 0.001,
                    lat: if id == 5 { 0.001 } else { 0. },
                },
                tags: Tags::new(),
            },
        );
    }
    for (id, refs) in [
        (10, vec![1, 2]),
        (20, vec![2, 3, 4]),
        (30, vec![4, 6]),
        (40, vec![2, 5]),
        (50, vec![3, 5]),
        (60, vec![4, 5]),
    ] {
        osm.ways.insert(
            id,
            Way {
                id,
                refs,
                tags: tags(&[("highway", "residential"), ("maxspeed", "30")]),
            },
        );
    }
    osm
}
fn relation(rule: &str, via_node: bool) -> Relation {
    Relation {
        tags: tags(&[("type", "restriction"), ("restriction", rule)]),
        members: vec![
            Member {
                id: 10,
                kind: 1,
                role: "from".into(),
            },
            Member {
                id: if via_node { 2 } else { 20 },
                kind: if via_node { 0 } else { 1 },
                role: "via".into(),
            },
            Member {
                id: if via_node { 40 } else { 30 },
                kind: 1,
                role: "to".into(),
            },
        ],
    }
}
#[test]
fn only_via_way_blocks_early_exits_and_keeps_allowed_sequence() {
    let mut osm = network();
    osm.relations.push(relation("only_straight_on", false));
    let (g, _) = graph(&osm).unwrap();
    let id = |way: i64, from: usize, to: usize| {
        g.edges
            .iter()
            .position(|e| e.way == way && e.from == from - 1 && e.to == to - 1)
            .unwrap()
    };
    let a = id(10, 1, 2);
    let b = id(20, 2, 3);
    let c = id(20, 3, 4);
    let d = id(30, 4, 6);
    assert!(g.forbidden.contains(&vec![a, id(40, 2, 5)]));
    assert!(g.forbidden.contains(&vec![a, b, id(50, 3, 5)]));
    assert!(g.forbidden.contains(&vec![a, b, c, id(60, 4, 5)]));
    assert!(!g.forbidden.contains(&vec![a, b, c, d]));
}
#[test]
fn only_rules_with_same_prefix_union_allowed_exits() {
    let mut osm = network();
    osm.relations.push(relation("only_right_turn", true));
    let mut second = relation("only_straight_on", true);
    second.members[2].id = 20;
    osm.relations.push(second);
    let (g, _) = graph(&osm).unwrap();
    let a = g
        .edges
        .iter()
        .position(|e| e.way == 10 && e.from == 0)
        .unwrap();
    for (b, e) in g
        .edges
        .iter()
        .enumerate()
        .filter(|(_, e)| e.from == 1 && matches!(e.way, 20 | 40))
    {
        assert!(!g.forbidden.contains(&vec![a, b]), "allowed way {}", e.way);
    }
}
#[test]
fn no_via_way_resolves_complete_sequence() {
    let mut osm = network();
    osm.relations.push(relation("no_straight_on", false));
    let (g, stats) = graph(&osm).unwrap();
    assert!(g.forbidden.iter().any(|s| s.len() == 4));
    assert_eq!(stats["resolved_restrictions"], 1);
}
#[test]
fn rules_direction_access_speed_and_conservative_conditionals() {
    for direction in ["yes", "-1", "no"] {
        let mut osm = network();
        osm.ways
            .get_mut(&10)
            .unwrap()
            .tags
            .insert("oneway".into(), direction.into());
        let (g, _) = graph(&osm).unwrap();
        let edges: Vec<_> = g.edges.iter().filter(|e| e.way == 10).collect();
        assert_eq!(edges.len(), if direction == "no" { 2 } else { 1 });
        if direction == "-1" {
            assert_eq!(edges[0].from, 1);
        }
    }
    let mut osm = network();
    let way = osm.ways.get_mut(&10).unwrap();
    way.tags.insert("access".into(), "no".into());
    way.tags.insert("motorcar".into(), "yes".into());
    let (g, _) = graph(&osm).unwrap();
    assert!(g.edges.iter().any(|e| e.way == 10));
    osm.ways
        .get_mut(&10)
        .unwrap()
        .tags
        .insert("motorcar:conditional".into(), "yes @ (Mo-Fr)".into());
    let (g, stats) = graph(&osm).unwrap();
    assert!(g.edges.iter().all(|e| e.way != 10));
    assert!(stats["access_or_conditional_ways_blocked"] > 0);
    let mut osm = network();
    osm.ways
        .get_mut(&10)
        .unwrap()
        .tags
        .insert("maxspeed".into(), "10 mph".into());
    let (g, _) = graph(&osm).unwrap();
    let edge = g.edges.iter().find(|e| e.way == 10).unwrap();
    let expected = edge.distance_mm as f64 * 3.6 / 16.09344;
    assert!((edge.duration_ms as f64 - expected).abs() <= 0.5);
}
#[test]
fn malformed_static_restriction_fails_instead_of_disappearing() {
    let mut osm = network();
    osm.relations.push(relation("no_flying", true));
    assert!(graph(&osm).is_err());
    let mut osm = network();
    osm.ways.get_mut(&10).unwrap().refs.push(99);
    assert!(graph(&osm).is_err());
}

#[test]
fn ordered_multiple_via_ways_keep_every_only_prefix() {
    let mut osm = network();
    osm.ways.get_mut(&20).unwrap().refs = vec![2, 3];
    osm.ways.insert(
        70,
        Way {
            id: 70,
            refs: vec![3, 4],
            tags: tags(&[("highway", "residential")]),
        },
    );
    let mut rel = relation("only_straight_on", false);
    rel.members.insert(
        2,
        Member {
            id: 70,
            kind: 1,
            role: "via".into(),
        },
    );
    osm.relations.push(rel);
    let (g, _) = graph(&osm).unwrap();
    assert!(g.forbidden.iter().any(|sequence| sequence
        .iter()
        .map(|&id| g.edges[id].way)
        .eq([10, 20, 70, 60])));
    assert!(!g.forbidden.iter().any(|sequence| sequence
        .iter()
        .map(|&id| g.edges[id].way)
        .eq([10, 20, 70, 30])));
}
