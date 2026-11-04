use routeweft_dataset::{DatasetFormat, Edge, Graph, Point};
use routeweft_routing::{
    acceleration::{self, CustomizationOptions, FILE_NAME},
    Engine, Error, QueryOptions, SearchBudget,
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
};

fn fixture() -> PathBuf {
    fixture_format(DatasetFormat::BinaryV2)
}
fn fixture_format(format: DatasetFormat) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "mld-cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let graph = Graph {
        nodes: (0..800)
            .map(|i| Point {
                lon: i as f64 * 0.00001,
                lat: 0.,
            })
            .collect(),
        edges: (0..799)
            .map(|i| Edge {
                from: i,
                to: i + 1,
                way: 1,
                distance_mm: 1000,
                duration_ms: 1000,
                name: "road".into(),
                class: "residential".into(),
                roundabout: false,
            })
            .collect(),
        forbidden: vec![],
    };
    routeweft_dataset::write_with_format(
        &path,
        &graph,
        "a".repeat(64),
        None,
        BTreeMap::new(),
        format,
    )
    .unwrap();
    path
}
fn run(args: &[&str]) -> (i32, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_routeweft"))
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    (
        output.status.code().unwrap(),
        serde_json::from_slice(&output.stdout).unwrap(),
    )
}
#[test]
fn persisted_metric_binds_to_dataset_rejects_corruption_and_preserves_previous_on_failure() {
    let path = fixture();
    let load = Default::default();
    assert!(Engine::open(&path, load)
        .unwrap()
        .acceleration_info()
        .is_none());
    let info = acceleration::customize(&path, load, Default::default()).unwrap();
    assert!(info.shortcuts.iter().sum::<usize>() > 0);
    let engine = Engine::open(&path, load).unwrap();
    assert!(engine.acceleration_info().is_some());
    let outcome = engine.route_detailed(
        &[
            Point { lon: 0., lat: 0. },
            Point {
                lon: 0.00799,
                lat: 0.,
            },
        ],
        QueryOptions::default(),
        &SearchBudget::default(),
        Default::default(),
    );
    assert_eq!(outcome.result.unwrap().duration_s, 799.);
    assert!(outcome.diagnostics.relaxed_shortcuts > 0);
    let file = path.join(FILE_NAME);
    let valid = std::fs::read(&file).unwrap();
    let (_, graph) = routeweft_dataset::open(&path, load).unwrap();
    let other = path.with_extension("other");
    routeweft_dataset::write_with_format(
        &other,
        &graph,
        "b".repeat(64),
        None,
        BTreeMap::new(),
        DatasetFormat::BinaryV2,
    )
    .unwrap();
    // An intact, correctly checksummed metric for another dataset is stale.
    std::fs::write(other.join(FILE_NAME), &valid).unwrap();
    assert!(matches!(
        Engine::open(&other, load),
        Err(Error::InvalidDataset)
    ));
    std::fs::remove_dir_all(other).unwrap();
    assert!(matches!(
        acceleration::customize(
            &path,
            load,
            CustomizationOptions {
                max_work: 0,
                ..Default::default()
            }
        ),
        Err(Error::BudgetExceeded)
    ));
    assert_eq!(std::fs::read(&file).unwrap(), valid);
    assert!(matches!(
        acceleration::customize(
            &path,
            load,
            CustomizationOptions {
                deadline: Some(std::time::Instant::now()),
                ..Default::default()
            }
        ),
        Err(Error::BudgetExceeded)
    ));
    assert_eq!(std::fs::read(&file).unwrap(), valid);
    assert!(!std::fs::read_dir(&path).unwrap().any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .ends_with(".tmp")));
    let mut corrupt = valid.clone();
    *corrupt.last_mut().unwrap() ^= 1;
    std::fs::write(&file, corrupt).unwrap();
    assert!(matches!(
        Engine::open(&path, load),
        Err(Error::InvalidDataset)
    ));
    let mut corrupt = valid.clone();
    corrupt[8] = if corrupt[8] == b'a' { b'b' } else { b'a' };
    std::fs::write(&file, corrupt).unwrap();
    assert!(matches!(
        Engine::open(&path, load),
        Err(Error::InvalidDataset)
    ));
    std::fs::write(&file, &valid[..valid.len() / 2]).unwrap();
    assert!(Engine::open(&path, load).is_err());
    std::fs::write(&file, valid).unwrap();
    let manifest = routeweft_dataset::inspect_manifest(&path, load).unwrap();
    let base = routeweft_dataset::check_resident_budget(&manifest, load, true).unwrap();
    assert!(matches!(
        Engine::open(
            &path,
            routeweft_dataset::LoadOptions {
                max_resident_bytes: base,
                ..load
            }
        ),
        Err(Error::BudgetExceeded)
    ));
    std::fs::remove_dir_all(path).unwrap();
}
#[test]
fn customize_cli_and_resident_requests_report_independent_limits_without_reloading() {
    let path = fixture();
    let dataset = path.to_str().unwrap();
    let (code, value) = run(&[
        "customize",
        "--dataset",
        dataset,
        "--levels",
        "4",
        "--max-boundary",
        "512",
        "--max-shortcuts",
        "40000000",
        "--max-index-bytes",
        "4294967296",
    ]);
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["acceleration"]["algorithm"], "mld");
    assert_eq!(value["acceleration"]["levels"], 4);
    let (invalid_code, _) = run(&["customize", "--dataset", dataset, "--levels", "5"]);
    assert_ne!(invalid_code, 0);
    for (flags, reason) in [
        (["--max-states", "1"], "states"),
        (["--max-work", "0"], "work"),
        (["--timeout-ms", "0"], "deadline"),
    ] {
        let (code, value) = run(&[
            "route",
            "--dataset",
            dataset,
            "--coordinates",
            "0,0;0.00799,0",
            flags[0],
            flags[1],
        ]);
        assert_eq!(code, 5, "{value}");
        assert_eq!(value["error"]["kind"], "BudgetExceeded");
        assert_eq!(value["diagnostics"]["failure"]["reason"], reason);
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_routeweft"))
        .args(["serve", "--dataset", dataset])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    writeln!(input, "bad json").unwrap();
    for request in [
        json!({"id":1,"coordinates":[{"lon":0.,"lat":0.},{"lon":0.00799,"lat":0.}],"limits":{"max_work":0}}),
        json!({"id":2,"coordinates":[{"lon":0.,"lat":0.},{"lon":0.00799,"lat":0.}]}),
        json!({"id":3,"operation":"nearest","coordinates":[{"lon":0.004,"lat":0.}]}),
    ] {
        writeln!(input, "{request}").unwrap();
    }
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let lines: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 5);
    assert_eq!(lines[0]["status"], "ready");
    assert_eq!(lines[1]["error"]["kind"], "InvalidInput");
    assert_eq!(lines[2]["diagnostics"]["failure"]["reason"], "work");
    assert_eq!(lines[3]["id"], 2);
    assert_eq!(lines[3]["route"]["duration_s"], 799.);
    assert_eq!(lines[3]["diagnostics"]["algorithm"], "mld");
    assert_eq!(lines[4]["status"], "ok");
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn legacy_json_graph_can_be_customized_without_changing_its_manifest() {
    let path = fixture_format(DatasetFormat::JsonV1);
    let before = std::fs::read(path.join("manifest.json")).unwrap();
    acceleration::customize(&path, Default::default(), Default::default()).unwrap();
    assert_eq!(before, std::fs::read(path.join("manifest.json")).unwrap());
    let engine = Engine::open(&path, Default::default()).unwrap();
    assert!(engine.acceleration_info().is_some());
    assert_eq!(
        engine
            .route(
                &[
                    Point { lon: 0., lat: 0. },
                    Point {
                        lon: 0.00799,
                        lat: 0.
                    }
                ],
                QueryOptions::default(),
                &SearchBudget::default()
            )
            .unwrap()
            .duration_s,
        799.
    );
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn restriction_product_metric_roundtrip_and_old_accelerator_requires_recustomization() {
    use routeweft_routing::RoutingAlgorithm;
    let path = fixture();
    let nodes: Vec<_> = (0..30)
        .map(|v| Point {
            lon: (v % 6) as f64 * 0.0001,
            lat: (v / 6) as f64 * 0.0001,
        })
        .collect();
    let mut edges = Vec::new();
    for v in 0..nodes.len() {
        for to in [
            (v % 6 < 5).then_some(v + 1),
            (v + 6 < nodes.len()).then_some(v + 6),
        ]
        .into_iter()
        .flatten()
        {
            for (from, to) in [(v, to), (to, v)] {
                edges.push(Edge {
                    from,
                    to,
                    way: 1,
                    distance_mm: 1000,
                    duration_ms: 100,
                    name: "road".into(),
                    class: "residential".into(),
                    roundabout: false,
                });
            }
        }
    }
    let forbidden: Vec<_> = (0..edges.len())
        .step_by(5)
        .filter_map(|a| {
            edges
                .iter()
                .position(|b| b.from == edges[a].to && b.to != edges[a].from)
                .map(|b| vec![a, b])
        })
        .collect();
    let graph = Graph {
        nodes: nodes.clone(),
        edges,
        forbidden,
    };
    let data = path.join("restricted");
    routeweft_dataset::write_with_format(
        &data,
        &graph,
        "c".repeat(64),
        None,
        BTreeMap::new(),
        DatasetFormat::BinaryV2,
    )
    .unwrap();
    let info = acceleration::customize(
        &data,
        Default::default(),
        CustomizationOptions {
            leaf_nodes: 4,
            fanout: 4,
            levels: 2,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(info.restriction_states > nodes.len());
    let engine = Engine::open(&data, Default::default()).unwrap();
    for source in (0..30).step_by(3) {
        for target in (0..30).step_by(4) {
            let points = [nodes[source], nodes[(source + target) / 2], nodes[target]];
            let a = engine.route_detailed(
                &points,
                QueryOptions::default(),
                &SearchBudget::default(),
                RoutingAlgorithm::Dijkstra,
            );
            let b = engine.route_detailed(
                &points,
                QueryOptions::default(),
                &SearchBudget::default(),
                RoutingAlgorithm::Auto,
            );
            assert_eq!(a.result.unwrap().duration_s, b.result.unwrap().duration_s);
        }
    }
    let file = data.join(FILE_NAME);
    let mut bytes = std::fs::read(&file).unwrap();
    assert_eq!(&bytes[..8], b"RWFTML02");
    bytes[..8].copy_from_slice(b"RWFTML01");
    std::fs::write(&file, bytes).unwrap();
    assert!(matches!(
        Engine::open(&data, Default::default()),
        Err(Error::IncompatibleFormat)
    ));
    acceleration::customize(&data, Default::default(), Default::default()).unwrap();
    assert!(Engine::open(&data, Default::default())
        .unwrap()
        .acceleration_info()
        .is_some());
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn customization_receipt_rejects_changed_parameters_and_changed_accelerator_state() {
    let path = fixture();
    let dataset = path.to_str().unwrap();
    let flags = [
        "--dataset",
        dataset,
        "--leaf-nodes",
        "64",
        "--fanout",
        "4",
        "--levels",
        "2",
    ];
    let mut args = vec!["resource-plan"];
    args.extend(flags);
    let (code, planned) = run(&args);
    assert_eq!(code, 0, "{planned}");
    assert_eq!(planned["resource_plan"]["schema"], 2);
    assert_eq!(planned["resource_plan"]["customization"]["leaf_nodes"], 64);
    let receipt = path.join("resource-plan.json");
    std::fs::write(&receipt, serde_json::to_vec(&planned).unwrap()).unwrap();
    let mut args = vec!["customize"];
    args.extend(flags);
    args.extend(["--resource-plan", receipt.to_str().unwrap()]);
    let mut wrong = args.clone();
    let index = wrong.iter().position(|v| *v == "--leaf-nodes").unwrap() + 1;
    wrong[index] = "32";
    let (code, rejected) = run(&wrong);
    assert_eq!(code, 2, "{rejected}");
    assert!(!path.join(FILE_NAME).exists());
    let (code, customized) = run(&args);
    assert_eq!(code, 0, "{customized}");
    let original = std::fs::read(path.join(FILE_NAME)).unwrap();
    let (code, rejected) = run(&args);
    assert_eq!(code, 2, "{rejected}");
    assert_eq!(std::fs::read(path.join(FILE_NAME)).unwrap(), original);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn generated_cross_edge_samples_feed_real_resident_acceptance_and_report_shortfalls() {
    let path = fixture();
    let dataset = path.to_str().unwrap();
    let (code, samples) = run(&[
        "sample-routes",
        "--dataset",
        dataset,
        "--distance-bands-m",
        "none",
        "--count",
        "3",
        "--waypoints",
        "3",
    ]);
    assert_eq!(code, 0, "{samples}");
    assert_eq!(samples["sampling"]["coverage_complete"], true);
    for sample in samples["sampling"]["samples"].as_array().unwrap() {
        assert!(sample["witness_edges"].as_array().unwrap().len() >= 2);
        assert_eq!(sample["coordinates"].as_array().unwrap().len(), 3);
    }
    let pairs = path.join("samples.json");
    std::fs::write(&pairs, serde_json::to_vec(&samples).unwrap()).unwrap();
    let (code, insufficient) = run(&["sample-routes", "--dataset", dataset, "--max-work", "0"]);
    assert_eq!(code, 3, "{insufficient}");
    assert_eq!(insufficient["sampling"]["coverage_complete"], false);
    assert!(insufficient["sampling"]["shortfall_reasons"]
        .as_array()
        .unwrap()
        .contains(&json!("work")));
    acceleration::customize(&path, Default::default(), Default::default()).unwrap();
    let output = path.join("acceptance");
    let benchmark =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../scripts/benchmark-routing.py");
    let result = Command::new("python3")
        .arg(benchmark)
        .args([
            "--binary",
            env!("CARGO_BIN_EXE_routeweft"),
            "--dataset",
            dataset,
            "--pairs",
            pairs.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
            "--mode",
            "acceptance",
            "--require-mld",
            "--require-reachable",
            "--warmup",
            "1",
            "--repeat",
            "1",
            "--max-p95-ms",
            "10000",
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let summary: Value =
        serde_json::from_slice(&std::fs::read(output.join("summary.json")).unwrap()).unwrap();
    assert_eq!(summary["acceptance_passed"], true);
    assert_eq!(summary["compared"], 3);
    assert_eq!(summary["auto"]["algorithms"], json!(["mld"]));
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn paged_cli_prepares_verifies_and_requires_explicit_compatible_index() {
    let path = fixture();
    let dir = path.to_str().unwrap();
    assert_ne!(
        run(&[
            "nearest",
            "--dataset",
            dir,
            "--storage",
            "paged",
            "--coordinate",
            "0,0"
        ])
        .0,
        0
    );
    assert_eq!(run(&["prepare-query", "--dataset", dir]).0, 0);
    let (code, verified) = run(&["verify-query", "--dataset", dir]);
    assert_eq!(code, 0, "{verified}");
    let (code, plan) = run(&["resource-plan", "--dataset", dir, "--storage", "paged"]);
    assert_eq!(code, 0, "{plan}");
    assert_eq!(plan["resource_plan"]["schema"], 1);
    let (code, result) = run(&[
        "route",
        "--dataset",
        dir,
        "--storage",
        "paged",
        "--coordinates",
        "0,0;0.00799,0",
        "--radius-m",
        "1",
    ]);
    assert_eq!(code, 0, "{result}");
    let (code, mismatch) = run(&[
        "nearest",
        "--dataset",
        dir,
        "--storage",
        "paged",
        "--query-index-sha256",
        &"0".repeat(64),
        "--coordinate",
        "0,0",
    ]);
    assert_ne!(code, 0, "{mismatch}");
    std::fs::remove_dir_all(path).unwrap();
}
