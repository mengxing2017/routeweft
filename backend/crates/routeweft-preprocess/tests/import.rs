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
        id: 100,
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
    second.id = 101;
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

#[test]
fn malformed_relation_diagnostics_and_explicit_conservative_blocking() {
    use routeweft_preprocess::{prepare, BuildOptions, InvalidRestrictionPolicy};
    let mut osm = network();
    let mut rel = relation("no_right_turn", true);
    rel.id = 6843783; // Same malformed member shape as the real input, synthetic geometry.
    rel.members.retain(|m| m.role != "via");
    osm.relations.push(rel);
    let error = graph(&osm).unwrap_err();
    let d = &error.report.restriction_diagnostics[0];
    assert_eq!(
        (d.relation_id, d.code.as_str(), d.action.as_str()),
        (6843783, "MissingVia", "rejected")
    );
    assert_eq!(d.tags["restriction"], "no_right_turn");
    assert_eq!(
        d.members
            .iter()
            .map(|m| (m.kind.as_str(), m.id))
            .collect::<Vec<_>>(),
        [("way", 10), ("way", 40)]
    );
    let options = BuildOptions {
        invalid_restrictions: InvalidRestrictionPolicy::BlockWays,
    };
    let (g, report) = prepare(&osm, options).unwrap();
    assert_eq!(report.blocked_way_ids, [10, 40]);
    assert!(g.edges.iter().all(|e| !matches!(e.way, 10 | 40)));
    assert_eq!(report.restriction_diagnostics[0].action, "blocked_ways");
    assert_eq!(report.diagnostics["invalid_restrictions_blocked"], 1);
    assert!(report.valid);
}

#[test]
fn missing_references_invalid_shapes_and_multiplicity_are_distinct() {
    for (mutation, code) in [
        (0, "MissingReference"),
        (1, "UnsupportedViaShape"),
        (2, "InvalidMemberType"),
        (3, "UnsupportedMultiplicity"),
        (4, "MissingFromTo"),
        (5, "UnsupportedRestriction"),
    ] {
        let mut osm = network();
        let mut r = relation("no_right_turn", true);
        match mutation {
            0 => r.members[1].id = 999,
            1 => {
                r.members.push(Member {
                    id: 20,
                    kind: 1,
                    role: "via".into(),
                });
            }
            2 => r.members[0].kind = 0,
            3 => r.members.push(Member {
                id: 20,
                kind: 1,
                role: "from".into(),
            }),
            4 => r.members.retain(|m| m.role != "to"),
            _ => {
                r.tags.insert("restriction".into(), "no_flying".into());
            }
        }
        osm.relations.push(r);
        let err = graph(&osm).unwrap_err();
        assert_eq!(err.report.restriction_diagnostics[0].code, code);
    }
    // A from/to member omitted from an extract must not be mistaken for a
    // profile-excluded road and silently bypass the restriction.
    let mut osm = network();
    let mut r = relation("no_right_turn", true);
    r.members[0].id = 999;
    osm.relations.push(r);
    assert_eq!(
        graph(&osm).unwrap_err().report.restriction_diagnostics[0].code,
        "MissingReference"
    );
}

fn node_route(
    g: &routeweft_dataset::Graph,
    from: usize,
    to: usize,
) -> Result<routeweft_routing::Path, routeweft_routing::Error> {
    use routeweft_domain::model::TravelProfile;
    let engine = routeweft_routing::NodeEngine::new(
        g.nodes.iter().map(|p| p.coordinate().unwrap()).collect(),
        g.edges
            .iter()
            .map(|e| routeweft_routing::Edge {
                from: e.from,
                to: e.to,
                duration_ms: e.duration_ms,
                distance_mm: e.distance_mm,
            })
            .collect(),
        g.forbidden.clone(),
    )
    .unwrap();
    engine.route(
        from - 1,
        to - 1,
        TravelProfile::Driving,
        &Default::default(),
    )
}

#[test]
fn multi_from_no_entry_and_multi_to_no_exit_enforce_each_turn() {
    for multi_from in [true, false] {
        let mut osm = network();
        // Star-shaped directed network isolates each prohibited turn, so a
        // routing result cannot hide a missing restriction behind a detour.
        osm.ways.retain(|id, _| matches!(id, 10 | 20 | 40));
        osm.ways.get_mut(&20).unwrap().refs = vec![2, 3];
        for w in osm.ways.values_mut() {
            w.tags.insert("oneway".into(), "yes".into());
        }
        let mut r = relation(if multi_from { "no_entry" } else { "no_exit" }, true);
        if multi_from {
            osm.ways.get_mut(&40).unwrap().refs = vec![5, 2];
            r.members[2].id = 20;
            r.members.push(Member {
                id: 40,
                kind: 1,
                role: "from".into(),
            });
        } else {
            r.members.push(Member {
                id: 20,
                kind: 1,
                role: "to".into(),
            });
        }
        osm.relations.push(r);
        let (g, stats) = graph(&osm).unwrap();
        assert_eq!(stats["resolved_restrictions"], 1);
        assert_eq!(g.forbidden.len(), 2);
        // Graph node IDs follow retained sorted OSM node IDs [1,2,3,5].
        assert_eq!(node_route(&g, 1, 3), Err(routeweft_routing::Error::NoRoute));
        if multi_from {
            assert_eq!(node_route(&g, 4, 3), Err(routeweft_routing::Error::NoRoute));
            assert!(node_route(&g, 1, 2).is_ok());
        } else {
            assert_eq!(node_route(&g, 1, 4), Err(routeweft_routing::Error::NoRoute));
            assert!(node_route(&g, 2, 4).is_ok());
        }
    }
}

#[test]
fn irrelevant_mode_conditionals_do_not_block_driving_roads() {
    for except in [true, false] {
        let mut osm = network();
        let mut r = relation("no_right_turn", true);
        r.tags.remove("restriction");
        r.tags.insert(
            if except {
                "restriction:conditional"
            } else {
                "restriction:bicycle:conditional"
            }
            .into(),
            "no_right_turn @ (Mo-Fr)".into(),
        );
        if except {
            r.tags.insert("except".into(), "motorcar".into());
        }
        osm.relations.push(r);
        let (g, stats) = graph(&osm).unwrap();
        assert!(g.edges.iter().any(|e| e.way == 10));
        assert_eq!(stats.get("conditional_restrictions_blocked"), None);
    }
}

#[test]
fn conservative_blocking_rechecks_dependent_restrictions() {
    use routeweft_preprocess::{prepare, BuildOptions, InvalidRestrictionPolicy};
    let mut osm = network();
    let mut malformed = relation("no_right_turn", true);
    malformed.id = 1;
    malformed.members.retain(|m| m.role != "via");
    // Blocking the malformed relation's via-way makes an initially valid
    // restriction unresolved. Its from/to roads must then also be excluded.
    malformed.members[0].id = 20;
    malformed.members[1].id = 50;
    let mut dependent = relation("no_straight_on", false);
    dependent.id = 2;
    osm.relations.extend([malformed, dependent]);
    let (g, report) = prepare(
        &osm,
        BuildOptions {
            invalid_restrictions: InvalidRestrictionPolicy::BlockWays,
        },
    )
    .unwrap();
    assert!(g.edges.iter().all(|e| !matches!(e.way, 10 | 20 | 30 | 50)));
    assert_eq!(report.blocked_way_ids, [10, 20, 30, 50]);
    assert_eq!(report.restriction_diagnostics.len(), 2);
}

#[test]
fn preprocessor_rejects_extent_unusable_by_the_city_query_engine() {
    let mut osm = network();
    osm.nodes.get_mut(&1).unwrap().point.lon = 10.;
    let err = graph(&osm).unwrap_err();
    assert!(err.to_string().contains("unsupported geographic extent"));
}

#[test]
fn multi_entry_via_way_and_expansion_limits_do_not_drop_restrictions() {
    let mut osm = network();
    let mut r = relation("no_entry", false);
    r.members.push(Member {
        id: 40,
        kind: 1,
        role: "from".into(),
    });
    osm.relations.push(r);
    let (g, _) = graph(&osm).unwrap();
    for from_way in [10, 40] {
        assert!(g
            .forbidden
            .iter()
            .any(|s| g.edges[s[0]].way == from_way && g.edges[*s.last().unwrap()].way == 30));
    }
    let mut osm = network();
    let mut r = relation("no_entry", true);
    for _ in 0..256 {
        r.members.push(Member {
            id: 10,
            kind: 1,
            role: "from".into(),
        });
    }
    osm.relations.push(r);
    let error = graph(&osm).unwrap_err();
    assert_eq!(
        error.report.restriction_diagnostics[0].code,
        "RestrictionLimitExceeded"
    );
    assert!(error.report.restriction_diagnostics[0].members_truncated);
}

#[test]
fn excluding_the_only_allowed_exit_never_opens_other_turns() {
    use routeweft_preprocess::{prepare, BuildOptions, InvalidRestrictionPolicy};
    let mut osm = network();
    osm.ways
        .get_mut(&40)
        .unwrap()
        .tags
        .insert("access".into(), "no".into());
    osm.relations.push(relation("only_right_turn", true));
    let error = graph(&osm).unwrap_err();
    assert_eq!(
        error.report.restriction_diagnostics[0].code,
        "UnresolvedRestriction"
    );
    let (g, report) = prepare(
        &osm,
        BuildOptions {
            invalid_restrictions: InvalidRestrictionPolicy::BlockWays,
        },
    )
    .unwrap();
    assert_eq!(report.blocked_way_ids, [10, 40]);
    assert!(g.edges.iter().all(|e| e.way != 10));
}

#[test]
fn compact_node_lookup_preserves_signed_sparse_ids_and_via_node_rules() {
    let mut osm = network();
    osm.relations.push(relation("only_straight_on", true));
    let expected = graph(&osm).unwrap();
    let ids = [i64::MIN, -9000, -1, 0, 9000, i64::MAX];
    let remap = |id: i64| ids[(id - 1) as usize];
    for (old_id, mut node) in std::mem::take(&mut osm.nodes) {
        node.id = remap(old_id);
        osm.nodes.insert(node.id, node);
    }
    for way in osm.ways.values_mut() {
        for id in &mut way.refs {
            *id = remap(*id);
        }
    }
    for rel in &mut osm.relations {
        for member in &mut rel.members {
            if member.kind == 0 {
                member.id = remap(member.id);
            }
        }
    }
    assert_eq!(graph(&osm).unwrap(), expected);
}
