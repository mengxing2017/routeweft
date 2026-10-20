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
