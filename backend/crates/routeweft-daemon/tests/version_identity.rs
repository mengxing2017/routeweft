use serde_json::Value;
use std::process::Command;

#[test]
fn identity_is_embedded_and_aliases_agree() {
    let binary = env!("CARGO_BIN_EXE_routeweft");
    let run = |args: &[&str], cwd: &std::path::Path| {
        let output = Command::new(binary)
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        output.stdout
    };
    let cwd = std::env::current_dir().unwrap();
    let output = run(&["version", "--json"], &cwd);
    assert_eq!(output, run(&["version", "--json"], &std::env::temp_dir()));
    let value: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(value["schema"], 1);
    assert_eq!(value["dataset_formats"], serde_json::json!([1, 2]));
    let build = &value["build"];
    assert!(build["target"].as_str().unwrap().contains('-'));
    assert!(build["rustc"].as_str().unwrap().starts_with("rustc "));
    if let Some(commit) = build["git_commit"].as_str() {
        assert_eq!(commit.len(), 40);
        assert!(commit.bytes().all(|b| b.is_ascii_hexdigit()));
    }
    if build["source_dirty"] != false || build["git_commit"].is_null() {
        assert_eq!(build["release_eligible"], false);
    }
    let alias = run(&["--version"], &cwd);
    assert_eq!(alias, run(&["-V"], &cwd));
    let text = String::from_utf8(alias).unwrap();
    assert!(text.contains(value["version"].as_str().unwrap()));
    assert!(text.contains(build["git_commit"].as_str().unwrap_or("unknown")));
}
