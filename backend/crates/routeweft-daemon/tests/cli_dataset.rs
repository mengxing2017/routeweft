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
#[test]
fn generated_pbf_to_cli_route_and_corrupt_input() {
    let root = std::env::temp_dir().join(format!("routeweft-cli-{}", std::process::id()));
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
