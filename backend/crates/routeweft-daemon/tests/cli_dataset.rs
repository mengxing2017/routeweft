use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;
fn command(args: &[&str]) -> (i32, Value) {
    let out = Command::new(env!("CARGO_BIN_EXE_routeweft"))
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    (
        out.status.code().unwrap(),
        serde_json::from_slice(&out.stdout).unwrap(),
    )
}
fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}
#[test]
fn generated_pbf_to_cli_route_and_corrupt_input() {
    let root = temp("routeweft-cli");
    std::fs::create_dir_all(&root).unwrap();
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/generate_fixture.py");
    for (i, extra) in [
        vec![],
        vec!["--dense"],
        vec!["--raw"],
        vec!["--dense", "--raw"],
    ]
    .into_iter()
    .enumerate()
    {
        let input = root.join(format!("fixture{i}.pbf"));
        let output = root.join(format!("dataset{i}"));
        let out = Command::new("python3")
            .arg(&script)
            .arg(&input)
            .args(extra)
            .output()
            .unwrap();
        assert!(out.status.success());
        let (code, m) = command(&[
            "build",
            "--input",
            input.to_str().unwrap(),
            "--profile",
            "driving",
            "--output",
            output.to_str().unwrap(),
        ]);
        assert_eq!(code, 0, "{m}");
        let path = output.to_str().unwrap();
        let id = &m["manifest"]["dataset_id"];
        for cmd in ["inspect", "verify"] {
            let (code, m) = command(&[cmd, "--dataset", path, "--json"]);
            assert_eq!(code, 0);
            assert_eq!(&m["manifest"]["dataset_id"], id);
        }
        let (code, nearest) = command(&[
            "nearest",
            "--dataset",
            path,
            "--coordinate",
            "0.0005,0",
            "--json",
        ]);
        assert_eq!(code, 0);
        assert_eq!(&nearest["dataset_id"], id);
        let (code, route) = command(&[
            "route",
            "--dataset",
            path,
            "--coordinates",
            "0.0002,0;0.0008,0;0.002,0.0005",
            "--json",
        ]);
        assert_eq!(code, 0, "{route}");
        assert_eq!(route["route"]["legs"].as_array().unwrap().len(), 2);
        assert_eq!(&route["route"]["dataset_id"], id);
        assert!(route["route"]["duration_s"].as_f64().unwrap() > 0.);
        let (code, error) = command(&[
            "route",
            "--dataset",
            path,
            "--coordinates",
            "0,0;0.002,0",
            "--max-work",
            "0",
        ]);
        assert_eq!(code, 5);
        assert_eq!(error["error"]["kind"], "BudgetExceeded");
        std::fs::write(output.join("graph.json"), b"{}").unwrap();
        let (code, error) = command(&["verify", "--dataset", path]);
        assert_eq!(code, 4);
        assert_eq!(error["error"]["kind"], "InvalidDataset");
        let mut truncated = std::fs::read(&input).unwrap();
        truncated.truncate(truncated.len() / 2);
        std::fs::write(&input, truncated).unwrap();
        let failed = root.join(format!("failed{i}"));
        let (code, _) = command(&[
            "build",
            "--input",
            input.to_str().unwrap(),
            "--profile",
            "driving",
            "--output",
            failed.to_str().unwrap(),
        ]);
        assert_ne!(code, 0);
        assert!(!failed.exists());
    }
    for args in [
        vec!["route", "driving", "0,0;1,1"],
        vec!["nearest", "--unknown", "x"],
        vec!["verify", "--dataset", "x", "--dataset", "y"],
    ] {
        let (code, error) = command(&args);
        assert_eq!(code, 2);
        assert_eq!(error["error"]["kind"], "InvalidInput");
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn preflight_reports_real_relation_identity_without_publishing_a_dataset() {
    let root = temp("routeweft-preflight");
    std::fs::create_dir_all(&root).unwrap();
    let input = root.join("relation.pbf");
    let output = root.join("must-not-exist");
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/generate_fixture.py");
    for (case, expected) in [
        ("missing-via", "MissingVia"),
        ("missing-reference", "MissingReference"),
        ("valid", ""),
    ] {
        let generated = Command::new("python3")
            .arg(&script)
            .arg(&input)
            .args(["--restriction-case", case])
            .output()
            .unwrap();
        assert!(generated.status.success());
        for cmd in ["build", "preflight"] {
            let mut args = vec![
                cmd,
                "--input",
                input.to_str().unwrap(),
                "--profile",
                "driving",
            ];
            if cmd == "build" {
                args.extend(["--output", output.to_str().unwrap()]);
            }
            let (code, value) = command(&args);
            if case == "valid" {
                assert_eq!(code, 0, "{value}");
                assert!(value["report"]["valid"].as_bool().unwrap());
                assert_eq!(
                    value["report"]["restriction_diagnostics"]
                        .as_array()
                        .unwrap()
                        .len(),
                    0
                );
                if cmd == "build" {
                    std::fs::remove_dir_all(&output).unwrap();
                }
            } else {
                assert_eq!(code, 4, "{value}");
                assert_eq!(value["error"]["kind"], "InvalidDataset");
                assert_eq!(
                    value["error"]["details"]["failure_code"],
                    "InvalidRestriction"
                );
                let d = &value["error"]["details"]["restriction_diagnostics"][0];
                assert_eq!(d["relation_id"], 6843783);
                assert_eq!(d["code"], expected);
                assert!(!output.exists());
            }
        }
    }
    std::fs::create_dir_all(&output).unwrap();
    std::fs::write(output.join("sentinel"), b"keep").unwrap();
    let (code, value) = command(&[
        "build",
        "--input",
        input.to_str().unwrap(),
        "--profile",
        "driving",
        "--output",
        output.to_str().unwrap(),
        "--invalid-restrictions",
        "block_ways",
    ]);
    assert_eq!(code, 4);
    assert_eq!(
        value["error"]["details"]["failure_code"],
        "DatasetWriteError"
    );
    assert_eq!(value["error"]["details"]["policy"], "block_ways");
    assert_eq!(
        value["error"]["details"]["source_sha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    assert_eq!(std::fs::read(output.join("sentinel")).unwrap(), b"keep");
    let (code, version) = command(&["version", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(version["dataset_formats"][0], 1);
    let (code, _) = command(&["version", "--input", "bad"]);
    assert_eq!(code, 2);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn conservative_cli_policy_is_explicit_and_preserves_auditable_warnings() {
    let root = temp("routeweft-block-cli");
    std::fs::create_dir_all(&root).unwrap();
    let input = root.join("relation.pbf");
    let output = root.join("dataset");
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/generate_fixture.py");
    // This fixture blocks every road: conservative mode must fail closed,
    // rather than emit an empty graph or fall back to unrestricted roads.
    let generated = Command::new("python3")
        .arg(script)
        .arg(&input)
        .args(["--restriction-case", "missing-via"])
        .output()
        .unwrap();
    assert!(generated.status.success());
    let (code, value) = command(&[
        "build",
        "--input",
        input.to_str().unwrap(),
        "--profile",
        "driving",
        "--output",
        output.to_str().unwrap(),
        "--invalid-restrictions",
        "block_ways",
    ]);
    assert_eq!(code, 4, "{value}");
    assert!(!output.exists());
    assert_eq!(
        value["error"]["details"]["blocked_way_ids"],
        serde_json::json!([10, 20])
    );
    assert_eq!(
        value["error"]["details"]["restriction_diagnostics"][0]["action"],
        "blocked_ways"
    );
    let (code, _) = command(&[
        "preflight",
        "--input",
        input.to_str().unwrap(),
        "--profile",
        "driving",
        "--invalid-restrictions",
        "ignore",
    ]);
    assert_eq!(code, 2);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn structural_scan_has_explicitly_weaker_scope_and_capacity_diagnostics() {
    let root = temp("routeweft-structure");
    std::fs::create_dir_all(&root).unwrap();
    let input = root.join("fixture.pbf");
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/generate_fixture.py");
    // Invalid restrictions are structurally well-formed, but full preflight must reject.
    for extra in [
        vec![],
        vec!["--dense"],
        vec!["--raw"],
        vec!["--dense", "--raw"],
    ] {
        assert!(Command::new("python3")
            .arg(&script)
            .arg(&input)
            .args(["--restriction-case", "missing-via"])
            .args(extra)
            .status()
            .unwrap()
            .success());
        let (code, scan) = command(&[
            "preflight",
            "--input",
            input.to_str().unwrap(),
            "--profile",
            "driving",
            "--mode",
            "structure",
        ]);
        assert_eq!(code, 0, "{scan}");
        assert_eq!(scan["report"]["validation_scope"], "structure_only");
        assert_eq!(scan["report"]["semantic_validation"], false);
        assert_eq!(scan["report"]["duplicate_ids_checked"], false);
        assert!(scan["report"]["nodes"].as_u64().unwrap() > 0);
        assert!(scan["report"]["decoded_bytes"].as_u64().unwrap() > 0);
        let (code, _) = command(&[
            "preflight",
            "--input",
            input.to_str().unwrap(),
            "--profile",
            "driving",
        ]);
        assert_eq!(code, 4);
        let (code, _) = command(&[
            "preflight",
            "--input",
            input.to_str().unwrap(),
            "--profile",
            "driving",
            "--mode",
            "structure",
            "--invalid-restrictions",
            "block_ways",
        ]);
        assert_eq!(code, 2);
    }
    // Sparse file only tests the admission guard; it is not a large-data acceptance fixture.
    let large = root.join("too-large.pbf");
    std::fs::File::create(&large)
        .unwrap()
        .set_len(536_870_913)
        .unwrap();
    for command_name in ["build", "preflight"] {
        let output = root.join("must-not-publish");
        let mut args = vec![
            command_name,
            "--input",
            large.to_str().unwrap(),
            "--profile",
            "driving",
        ];
        if command_name == "build" {
            args.extend(["--output", output.to_str().unwrap()]);
        }
        let (code, error) = command(&args);
        assert_eq!(code, 4);
        assert_eq!(error["error"]["kind"], "BudgetExceeded");
        assert_eq!(error["error"]["resource"]["resource"], "input_bytes");
        assert_eq!(error["error"]["resource"]["limit"], 536_870_912_u64);
        assert_eq!(error["error"]["resource"]["observed"], 536_870_913_u64);
        assert!(!output.exists());
    }
    let (code, version) = command(&["version", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(version["capabilities"]["large_pbf_3gib"], false);
    assert_eq!(version["capabilities"]["external_memory_build"], false);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn metadata_inspection_is_explicit_and_full_verification_still_detects_corruption() {
    let root = temp("routeweft-metadata");
    std::fs::create_dir_all(&root).unwrap();
    let input = root.join("fixture.pbf");
    let output = root.join("dataset");
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/generate_fixture.py");
    assert!(Command::new("python3")
        .arg(script)
        .arg(&input)
        .status()
        .unwrap()
        .success());
    let (code, _) = command(&[
        "build",
        "--input",
        input.to_str().unwrap(),
        "--profile",
        "driving",
        "--output",
        output.to_str().unwrap(),
    ]);
    assert_eq!(code, 0);
    std::fs::write(output.join("graph.json"), b"broken").unwrap();
    let (code, metadata) = command(&[
        "inspect",
        "--dataset",
        output.to_str().unwrap(),
        "--mode",
        "metadata",
    ]);
    assert_eq!(code, 0);
    assert_eq!(metadata["verified"], false);
    assert_eq!(metadata["verification_scope"], "manifest_only");
    for cmd in ["inspect", "verify"] {
        assert_eq!(command(&[cmd, "--dataset", output.to_str().unwrap()]).0, 4);
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn binary_v2_cli_preserves_driving_results_and_v1_default() {
    let root = temp("routeweft-v2-cli");
    std::fs::create_dir_all(&root).unwrap();
    let input = root.join("fixture.pbf");
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/generate_fixture.py");
    assert!(Command::new("python3")
        .arg(script)
        .arg(&input)
        .args(["--dense", "--restriction-case", "valid"])
        .status()
        .unwrap()
        .success());
    let mut reference_report = Value::Null;
    let mut reference_route = Value::Null;
    let mut reference_nearest = Value::Null;
    for format in [1, 2] {
        let output = root.join(format!("dataset{format}"));
        let mut args = vec![
            "build",
            "--input",
            input.to_str().unwrap(),
            "--profile",
            "driving",
            "--output",
            output.to_str().unwrap(),
        ];
        if format == 2 {
            args.extend(["--format", "2"]);
        }
        let (code, built) = command(&args);
        assert_eq!(code, 0, "{built}");
        assert_eq!(built["manifest"]["format"], format);
        if format == 1 {
            reference_report = built["report"].clone();
        } else {
            assert_eq!(reference_report, built["report"]);
        }
        let (code, verified) = command(&["verify", "--dataset", output.to_str().unwrap()]);
        assert_eq!(code, 0);
        assert_eq!(verified["verified"], true);
        let (code, mut nearest) = command(&[
            "nearest",
            "--dataset",
            output.to_str().unwrap(),
            "--coordinate",
            "0.0005,0",
        ]);
        assert_eq!(code, 0);
        let nearest = nearest.as_object_mut().unwrap();
        nearest.remove("dataset_id");
        nearest.remove("format");
        if format == 1 {
            reference_nearest = Value::Object(nearest.clone());
        } else {
            assert_eq!(reference_nearest, Value::Object(nearest.clone()));
        }
        let (code, mut routed) = command(&[
            "route",
            "--dataset",
            output.to_str().unwrap(),
            "--coordinates",
            "0,0;0.001,0",
        ]);
        assert_eq!(code, 0);
        let route = routed["route"].as_object_mut().unwrap();
        route.remove("dataset_id");
        route.remove("format");
        if format == 1 {
            reference_route = Value::Object(route.clone());
        } else {
            assert_eq!(reference_route, Value::Object(route.clone()));
        }
    }
    let (code, version) = command(&["version"]);
    assert_eq!(code, 0);
    assert_eq!(version["dataset_formats"], serde_json::json!([1, 2]));
    assert_eq!(version["capabilities"]["external_memory_build"], false);
    assert_eq!(version["capabilities"]["large_pbf_3gib"], false);
    let bad = root.join("bad");
    let (code, _) = command(&[
        "build",
        "--input",
        input.to_str().unwrap(),
        "--profile",
        "driving",
        "--output",
        bad.to_str().unwrap(),
        "--format",
        "3",
    ]);
    assert_eq!(code, 2);
    assert!(!bad.exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn explicit_external_builder_matches_memory_v2_and_queries_for_both_policies() {
    let root = temp("routeweft-cli-external");
    std::fs::create_dir_all(&root).unwrap();
    let input = root.join("input.pbf");
    let scratch = root.join("scratch");
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/generate_fixture.py");
    assert!(Command::new("python3")
        .arg(script)
        .arg(&input)
        .args(["--dense", "--restriction-case", "valid"])
        .status()
        .unwrap()
        .success());
    for policy in ["reject", "block_ways"] {
        let memory = root.join(format!("memory-{policy}"));
        let external = root.join(format!("external-{policy}"));
        let mut results = Vec::new();
        for (builder, output) in [("memory", &memory), ("external", &external)] {
            let mut args = vec![
                "build",
                "--input",
                input.to_str().unwrap(),
                "--output",
                output.to_str().unwrap(),
                "--profile",
                "driving",
                "--format",
                "2",
                "--builder",
                builder,
                "--invalid-restrictions",
                policy,
            ];
            if builder == "external" {
                args.extend(["--scratch-dir", scratch.to_str().unwrap()]);
            }
            let (code, response) = command(&args);
            assert_eq!(code, 0, "{response}");
            if builder == "external" {
                assert_eq!(response["build"]["builder"], "external");
                assert_eq!(response["build"]["resources"]["peak_completed_files"], 18);
            }
            results.push(response);
        }
        assert_eq!(results[0]["manifest"], results[1]["manifest"]);
        assert_eq!(results[0]["report"], results[1]["report"]);
        for file in ["graph.bin", "manifest.json"] {
            assert_eq!(
                std::fs::read(memory.join(file)).unwrap(),
                std::fs::read(external.join(file)).unwrap()
            );
        }
        for query in [
            vec!["verify"],
            vec!["nearest", "--coordinate", "0.0005,0"],
            vec!["route", "--coordinates", "0.0002,0;0.0008,0;0.002,0.0005"],
        ] {
            let mut a = query.clone();
            a.extend(["--dataset", memory.to_str().unwrap()]);
            let mut b = query;
            b.extend(["--dataset", external.to_str().unwrap()]);
            assert_eq!(command(&a), command(&b));
        }
        assert_eq!(std::fs::read_dir(&scratch).unwrap().count(), 0);
        let (code, _) = command(&[
            "build",
            "--input",
            input.to_str().unwrap(),
            "--output",
            external.to_str().unwrap(),
            "--profile",
            "driving",
            "--format",
            "2",
            "--builder",
            "external",
        ]);
        assert_eq!(code, 4);
        assert_eq!(std::fs::read_dir(&scratch).unwrap().count(), 0);
    }
    let (_, version) = command(&["version"]);
    assert_eq!(
        version["capabilities"]["build_modes"],
        serde_json::json!(["memory", "external"])
    );
    assert_eq!(version["capabilities"]["external_memory_build"], false);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn external_cli_validates_options_and_reports_budget_deadline_and_restrictions() {
    let root = temp("routeweft-cli-external-errors");
    std::fs::create_dir_all(&root).unwrap();
    let input = root.join("input.pbf");
    let output = root.join("dataset");
    let scratch = root.join("scratch");
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/generate_fixture.py");
    assert!(Command::new("python3")
        .arg(script)
        .arg(&input)
        .args(["--restriction-case", "missing-via"])
        .status()
        .unwrap()
        .success());
    let base = vec![
        "build",
        "--input",
        input.to_str().unwrap(),
        "--output",
        output.to_str().unwrap(),
        "--profile",
        "driving",
    ];
    for extra in [
        vec!["--builder", "unknown"],
        vec!["--builder", "external"],
        vec!["--scratch-dir", "unused"],
        vec![
            "--builder",
            "external",
            "--format",
            "2",
            "--max-scratch-bytes",
            "-1",
        ],
        vec![
            "--builder",
            "external",
            "--format",
            "2",
            "--build-timeout-ms",
            "not-a-number",
        ],
    ] {
        let mut args = base.clone();
        args.extend(extra);
        let (code, value) = command(&args);
        assert_eq!(code, 2, "{value}");
        assert!(!output.exists());
    }
    let mut base = base;
    base.extend([
        "--builder",
        "external",
        "--format",
        "2",
        "--scratch-dir",
        scratch.to_str().unwrap(),
    ]);
    let (code, rejected) = command(&base);
    assert_eq!(code, 4);
    assert_eq!(
        rejected["error"]["details"]["failure_code"],
        "InvalidRestriction"
    );
    assert_eq!(
        rejected["error"]["details"]["restriction_diagnostics"][0]["code"],
        "MissingVia"
    );
    for (flag, value, code, kind) in [
        ("--max-scratch-bytes", "1", 4, "BudgetExceeded"),
        ("--max-scratch-files", "1", 4, "BudgetExceeded"),
        ("--build-timeout-ms", "0", 5, "BudgetExceeded"),
    ] {
        let mut args = base.clone();
        args.extend([flag, value]);
        let (actual_code, response) = command(&args);
        assert_eq!(actual_code, code, "{response}");
        assert_eq!(response["error"]["kind"], kind);
        assert!(!output.exists());
        assert_eq!(std::fs::read_dir(&scratch).unwrap().count(), 0);
    }
    std::fs::remove_dir_all(root).unwrap();
}
