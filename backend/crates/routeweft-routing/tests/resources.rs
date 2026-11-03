use routeweft_dataset::{DatasetFormat, Edge, Graph, LoadOptions, Point};
use routeweft_routing::{
    acceleration::{self, CustomizationOptions, FILE_NAME},
    resources, Engine, Error, QueryOptions, SamplingOptions, SearchBudget,
};
use std::{path::PathBuf, time::Instant};

struct Fixture(PathBuf);
impl Fixture {
    fn new(restrictions: bool, format: DatasetFormat) -> Self {
        let root = std::env::temp_dir().join(format!(
            "mld-plan-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let nodes: Vec<_> = (0..144)
            .map(|i| Point {
                lon: (i % 12) as f64 * 0.001,
                lat: (i / 12) as f64 * 0.001,
            })
            .collect();
        let mut edges = Vec::new();
        for i in 0..nodes.len() {
            for to in [
                (i % 12 < 11).then_some(i + 1),
                (i + 12 < 144).then_some(i + 12),
            ]
            .into_iter()
            .flatten()
            {
                for (from, to) in [(i, to), (to, i)] {
                    edges.push(Edge {
                        from,
                        to,
                        way: 1,
                        distance_mm: 111000,
                        duration_ms: 1000,
                        name: "road".into(),
                        class: "residential".into(),
                        roundabout: false,
                    });
                }
            }
        }
        let forbidden = if restrictions {
            (0..edges.len())
                .step_by(13)
                .filter_map(|a| {
                    edges
                        .iter()
                        .position(|e| e.from == edges[a].to && e.to != edges[a].from)
                        .map(|b| vec![a, b])
                })
                .collect()
        } else {
            Vec::new()
        };
        routeweft_dataset::write_with_format(
            &root,
            &Graph {
                nodes,
                edges,
                forbidden,
            },
            "c".repeat(64),
            None,
            Default::default(),
            format,
        )
        .unwrap();
        Self(root)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}
fn options() -> CustomizationOptions {
    CustomizationOptions {
        leaf_nodes: 8,
        fanout: 4,
        levels: 2,
        max_shortcuts: 100000,
        ..Default::default()
    }
}
fn plan(path: &std::path::Path, options: CustomizationOptions) -> resources::ResourcePlan {
    resources::resource_plan(
        path,
        LoadOptions {
            max_resident_bytes: 1,
            ..Default::default()
        },
        64 * 1024 * 1024,
        options,
    )
    .unwrap()
}

#[test]
fn mld_planning_bounds_execution_across_restrictions_parameters_and_formats() {
    for format in [DatasetFormat::JsonV1, DatasetFormat::BinaryV2] {
        for restrictions in [false, true] {
            let fixture = Fixture::new(restrictions, format);
            let manifest_before = std::fs::read(fixture.0.join("manifest.json")).unwrap();
            let mut options = options();
            let before = plan(&fixture.0, options);
            assert_eq!(before.base.schema, 2);
            assert_eq!(before.base.estimator_version, 2);
            assert!(!before.acceleration_present);
            assert!(before.accelerated_load.is_none());
            assert!(!before
                .base
                .excluded_unknown
                .contains(&"optional_routing_mld"));
            assert!(before.customize.accelerator.output_bytes > 0);
            options.max_bytes = before.customize.accelerator.customization_peak_bytes;
            let limits = LoadOptions {
                max_resident_bytes: before.customize.peak_bytes,
                ..Default::default()
            };
            let info = acceleration::customize(&fixture.0, limits, options).unwrap();
            assert!(info.restriction_states as u64 <= before.topology_upper_bound.states);
            assert!(info.restriction_transitions as u64 <= before.topology_upper_bound.transitions);
            let after = plan(&fixture.0, options);
            let artifact = after.acceleration.as_ref().unwrap();
            assert_eq!(artifact.format, "RWFTML02");
            assert!(artifact.bytes <= before.customize.accelerator.output_bytes);
            let load = after.accelerated_load.unwrap();
            let engine = Engine::open(
                &fixture.0,
                LoadOptions {
                    max_resident_bytes: load.peak_bytes,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(engine.acceleration_sha256(), Some(artifact.sha256.as_str()));
            assert!(
                engine.acceleration_info().unwrap().estimated_bytes
                    <= load.accelerator.resident_bytes
            );
            assert_eq!(
                std::fs::read(fixture.0.join("manifest.json")).unwrap(),
                manifest_before
            );
            let changed = plan(
                &fixture.0,
                CustomizationOptions {
                    levels: 3,
                    fanout: 8,
                    ..options
                },
            );
            assert_eq!(changed.acceleration.as_ref().unwrap().fanout, 4);
            assert_eq!(changed.customization.fanout, 8);
            assert_ne!(changed.customize.peak_bytes, after.customize.peak_bytes);
        }
    }
}

#[test]
fn resource_budgets_reject_without_publication_and_load_boundary_matches_shared_model() {
    let fixture = Fixture::new(false, DatasetFormat::BinaryV2);
    for workspace in [0, 1000] {
        assert!(matches!(
            resources::resource_plan(&fixture.0, Default::default(), workspace, options()),
            Err(Error::BudgetExceeded)
        ));
    }
    assert!(matches!(
        acceleration::customize(
            &fixture.0,
            Default::default(),
            CustomizationOptions {
                max_bytes: 1,
                ..options()
            }
        ),
        Err(Error::BudgetExceeded)
    ));
    assert!(!fixture.0.join(FILE_NAME).exists());
    acceleration::customize(&fixture.0, Default::default(), options()).unwrap();
    let p = plan(&fixture.0, options());
    let boundary = p.accelerated_load.unwrap().peak_bytes;
    assert!(Engine::open(
        &fixture.0,
        LoadOptions {
            max_resident_bytes: boundary,
            ..Default::default()
        }
    )
    .is_ok());
    assert!(matches!(
        Engine::open(
            &fixture.0,
            LoadOptions {
                max_resident_bytes: boundary - 1,
                ..Default::default()
            }
        ),
        Err(Error::BudgetExceeded)
    ));
    let before = std::fs::read(fixture.0.join(FILE_NAME)).unwrap();
    assert!(acceleration::customize(
        &fixture.0,
        Default::default(),
        CustomizationOptions {
            max_bytes: 1,
            ..options()
        }
    )
    .is_err());
    assert_eq!(std::fs::read(fixture.0.join(FILE_NAME)).unwrap(), before);
    assert!(!std::fs::read_dir(&fixture.0).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .ends_with(".tmp")));
}

#[test]
fn planning_rejects_corrupt_incompatible_truncated_and_wrong_graph_artifacts() {
    let fixture = Fixture::new(true, DatasetFormat::BinaryV2);
    acceleration::customize(&fixture.0, Default::default(), options()).unwrap();
    let path = fixture.0.join(FILE_NAME);
    let valid = std::fs::read(&path).unwrap();
    for mode in 0..5 {
        let mut bytes = valid.clone();
        match mode {
            0 => {
                let last = bytes.len() - 1;
                bytes[last] ^= 1;
            }
            1 => bytes[..8].copy_from_slice(b"RWFTML01"),
            2 => bytes[8..72].fill(b'a'),
            3 => bytes.truncate(bytes.len() / 2),
            _ => bytes[72..76].copy_from_slice(&u32::MAX.to_le_bytes()),
        }
        std::fs::write(&path, bytes).unwrap();
        assert!(
            resources::resource_plan(&fixture.0, Default::default(), 64 * 1024 * 1024, options())
                .is_err(),
            "mode {mode}"
        );
    }
    std::fs::write(&path, valid).unwrap();
    assert!(resources::resource_plan(&fixture.0, Default::default(), 100000, options()).is_err());
    assert!(resources::resource_plan(
        &fixture.0,
        Default::default(),
        64 * 1024 * 1024,
        CustomizationOptions {
            levels: 5,
            ..options()
        }
    )
    .is_err());
}

#[test]
fn samples_are_deterministic_cross_edge_directed_legal_and_preserve_via_history() {
    let fixture = Fixture::new(true, DatasetFormat::BinaryV2);
    let engine = Engine::open(&fixture.0, Default::default()).unwrap();
    let options = SamplingOptions {
        count: 9,
        waypoints: 3,
        max_path_edges: 128,
        distance_bands_m: vec![200., 600.],
        ..Default::default()
    };
    let first = engine
        .sample_routes(options.clone(), &SearchBudget::default())
        .unwrap();
    assert!(first.coverage_complete, "{:?}", first.shortfall_reasons);
    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        serde_json::to_value(
            engine
                .sample_routes(options, &SearchBudget::default())
                .unwrap()
        )
        .unwrap()
    );
    let (_, graph) = routeweft_dataset::open(&fixture.0, Default::default()).unwrap();
    for sample in &first.samples {
        assert!(sample
            .witness_edges
            .iter()
            .any(|&e| e != sample.witness_edges[0]));
        for pair in sample.witness_edges.windows(2) {
            assert_eq!(graph.edges[pair[0]].to, graph.edges[pair[1]].from);
        }
        for forbidden in &graph.forbidden {
            assert!(!sample
                .witness_edges
                .windows(forbidden.len())
                .any(|w| w == forbidden));
        }
        let actual = engine
            .route(
                &sample.coordinates,
                QueryOptions {
                    radius_m: 0.01,
                    ..Default::default()
                },
                &SearchBudget::default(),
            )
            .unwrap();
        assert!(actual.duration_s * 1000. <= sample.witness_duration_ms as f64);
        assert_eq!(actual.legs.len(), 2);
    }
    assert!(first.generated_per_band.iter().all(|&n| n == 3));
}

#[test]
fn sampler_reports_partial_coverage_and_enforces_workspace_work_and_deadline() {
    let fixture = Fixture::new(false, DatasetFormat::BinaryV2);
    let engine = Engine::open(&fixture.0, Default::default()).unwrap();
    let zero = engine
        .sample_routes(
            Default::default(),
            &SearchBudget {
                max_work: 0,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!zero.coverage_complete);
    assert!(zero.shortfall_reasons.contains(&"work"));
    assert!(zero.pairs.is_empty());
    let timed = engine
        .sample_routes(
            Default::default(),
            &SearchBudget {
                deadline: Some(Instant::now()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(timed.shortfall_reasons.contains(&"deadline"));
    assert!(matches!(
        engine.sample_routes(
            SamplingOptions {
                max_workspace_bytes: 1,
                ..Default::default()
            },
            &SearchBudget::default()
        ),
        Err(Error::BudgetExceeded)
    ));
    let insufficient = engine
        .sample_routes(
            SamplingOptions {
                count: 1,
                distance_bands_m: vec![],
                max_path_edges: 2,
                max_attempts: 1,
                ..Default::default()
            },
            &SearchBudget::default(),
        )
        .unwrap();
    assert!(insufficient
        .samples
        .iter()
        .all(|s| s.witness_edges.len() >= 2));
}
