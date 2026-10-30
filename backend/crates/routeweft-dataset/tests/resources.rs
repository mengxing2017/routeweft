use routeweft_dataset::*;
use std::collections::BTreeMap;

fn manifest() -> Manifest {
    Manifest {
        format: 2,
        dataset_id: String::new(),
        builder: String::new(),
        profile: "driving-v1".into(),
        source_sha256: hash(b"fixture"),
        source_snapshot: None,
        attribution: "synthetic".into(),
        graph_file: "graph.bin".into(),
        graph_sha256: String::new(),
        graph_bytes: 0,
        nodes: 6_500_000,
        edges: 12_000_000,
        restrictions: 8000,
        diagnostics: BTreeMap::new(),
    }
}

#[test]
fn planning_and_execution_share_estimates_across_capacity_and_budget_boundaries() {
    for format in [1, 2] {
        let mut m = manifest();
        m.format = format;
        for refs in [0, 128, 1_000_000] {
            let mut limits = GraphCapacity::Regional.limits();
            limits.max_restriction_edges = refs;
            for indexes in [false, true] {
                let estimate = estimate_resident(&m, limits, indexes).unwrap();
                assert_eq!(
                    estimate.estimated_bytes,
                    estimate.nodes_bytes
                        + estimate.edges_bytes
                        + estimate.restriction_bytes
                        + estimate.strings_bytes
                        + estimate.dictionary_bytes
                );
                limits.max_resident_bytes = estimate.estimated_bytes;
                assert_eq!(
                    check_resident_budget(&m, limits, indexes).unwrap(),
                    estimate.estimated_bytes
                );
                limits.max_resident_bytes -= 1;
                let Err(Error::ResourceLimit(error)) = check_resident_budget(&m, limits, indexes)
                else {
                    panic!("must reject")
                };
                assert_eq!(error.observed, estimate.estimated_bytes);
                assert_eq!(error.limit, limits.max_resident_bytes);
                assert_eq!(error.estimator_version, Some(ESTIMATOR_VERSION));
                assert_eq!(
                    error.stage,
                    if indexes {
                        "engine_load"
                    } else {
                        "dataset_load"
                    }
                );
            }
        }
    }
    assert!(
        estimate_resident(&manifest(), GraphCapacity::Regional.limits(), true)
            .unwrap()
            .estimated_bytes
            > 8 * 1024 * 1024 * 1024
    );
}

#[test]
fn v1_reserves_visitor_ceilings_and_checked_arithmetic_rejects_extreme_counts() {
    let mut m = manifest();
    m.format = 1;
    let e = estimate_resident(&m, LoadOptions::default(), true).unwrap();
    m.nodes = 0;
    m.edges = 0;
    m.restrictions = 0;
    assert_eq!(
        estimate_resident(&m, LoadOptions::default(), true).unwrap(),
        e
    );
    m.format = 2;
    m.edges = usize::MAX;
    assert!(estimate_resident(&m, LoadOptions::default(), true).is_err());
    m.format = 9;
    assert!(matches!(
        estimate_resident(&m, LoadOptions::default(), true),
        Err(Error::IncompatibleFormat)
    ));
}

#[test]
fn authenticated_planning_does_not_load_graph_and_rejects_tampering() {
    for format in [DatasetFormat::JsonV1, DatasetFormat::BinaryV2] {
        let path = std::env::temp_dir().join(format!(
            "resource-plan-{}-{}",
            std::process::id(),
            format.version()
        ));
        let graph = Graph {
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
                name: String::new(),
                class: "residential".into(),
                roundabout: false,
            }],
            forbidden: vec![],
        };
        let m = write_with_format(
            &path,
            &graph,
            hash(b"fixture"),
            None,
            BTreeMap::new(),
            format,
        )
        .unwrap();
        let limits = LoadOptions {
            max_resident_bytes: 1,
            ..Default::default()
        };
        assert!(matches!(open(&path, limits), Err(Error::ResourceLimit(_))));
        let plan = resource_plan(&path, limits, 64 * 1024 * 1024).unwrap();
        assert_eq!(plan.dataset_id, m.dataset_id);
        assert_eq!(plan.graph_sha256, m.graph_sha256);
        assert_eq!(
            plan.engine_load,
            estimate_resident(&m, limits, true).unwrap()
        );
        assert!(!plan.excluded_unknown.is_empty());
        assert_eq!(
            plan.verification_scope,
            if format == DatasetFormat::BinaryV2 {
                "full_streaming_v2"
            } else {
                "identity_size_hash_v1_not_semantic"
            }
        );
        // Unknown optional acceleration is explicitly excluded; no index reader is invoked.
        std::fs::write(path.join("routing.mld"), b"not an index").unwrap();
        assert!(resource_plan(&path, limits, 64 * 1024 * 1024).is_ok());
        if format == DatasetFormat::BinaryV2 {
            assert!(resource_plan(&path, limits, 0).is_err());
        }
        let file = path.join(&m.graph_file);
        let original = std::fs::read(&file).unwrap();
        let mut corrupt = original.clone();
        let last = corrupt.len() - 1;
        corrupt[last] ^= 1;
        std::fs::write(&file, &corrupt).unwrap();
        assert!(resource_plan(&path, limits, 64 * 1024 * 1024).is_err());
        std::fs::write(&file, &original).unwrap();
        let mut forged = m.clone();
        forged.nodes = usize::MAX;
        std::fs::write(
            path.join("manifest.json"),
            serde_json::to_vec(&forged).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            resource_plan(&path, limits, 64 * 1024 * 1024),
            Err(Error::ResourceLimit(_))
        ));
        if format == DatasetFormat::BinaryV2 {
            forged = m.clone();
            forged.nodes = 1;
            std::fs::write(
                path.join("manifest.json"),
                serde_json::to_vec(&forged).unwrap(),
            )
            .unwrap();
            assert!(resource_plan(&path, limits, 64 * 1024 * 1024).is_err());
        }
        forged = m.clone();
        forged.graph_sha256 = hash(b"other");
        std::fs::write(
            path.join("manifest.json"),
            serde_json::to_vec(&forged).unwrap(),
        )
        .unwrap();
        assert!(resource_plan(&path, limits, 64 * 1024 * 1024).is_err());
        std::fs::remove_dir_all(path).unwrap();
    }
}
