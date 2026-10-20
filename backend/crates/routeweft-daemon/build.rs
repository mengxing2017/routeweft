use serde_json::json;
use std::{env, fs, path::PathBuf, process::Command};

fn git(root: &std::path::Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn main() {
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("../../..");
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    // Intentionally absent: recompute on every Cargo invocation, including changes
    // to refs, worktrees, untracked source files and dependencies outside this crate.
    println!(
        "cargo:rerun-if-changed={}",
        out.join("identity-always-check").display()
    );
    for key in [
        "ROUTEWEFT_BUILD_COMMIT",
        "ROUTEWEFT_BUILD_SOURCE_DIRTY",
        "ROUTEWEFT_BUILD_SOURCE_KIND",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }
    let supplied = [
        "ROUTEWEFT_BUILD_COMMIT",
        "ROUTEWEFT_BUILD_SOURCE_DIRTY",
        "ROUTEWEFT_BUILD_SOURCE_KIND",
    ]
    .map(|key| env::var(key).ok());
    let (commit, dirty, kind) = if supplied.iter().any(Option::is_some) {
        let [Some(commit), Some(dirty), Some(kind)] = supplied else {
            panic!("all three ROUTEWEFT_BUILD_* provenance variables are required");
        };
        assert!(
            commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()),
            "commit must be a full 40-character Git SHA"
        );
        assert!(
            kind == "git-archive" || kind == "provided",
            "source kind must be git-archive or provided"
        );
        let dirty = dirty
            .parse::<bool>()
            .expect("source dirty must be true or false");
        (Some(commit.to_lowercase()), Some(dirty), kind.to_owned())
    } else if root.join(".git").exists() {
        match (
            git(&root, &["rev-parse", "HEAD"]),
            git(
                &root,
                &[
                    "status",
                    "--porcelain",
                    "--untracked-files=all",
                    "--ignore-submodules=none",
                ],
            ),
        ) {
            (Some(commit), Some(status)) => {
                (Some(commit), Some(!status.is_empty()), "git".to_owned())
            }
            _ => (None, None, "unknown".to_owned()),
        }
    } else {
        (None, None, "unknown".to_owned())
    };
    let compiler = Command::new(env::var_os("RUSTC").unwrap())
        .arg("--version")
        .output()
        .expect("rustc version");
    assert!(compiler.status.success());
    let identity = json!({
        "git_commit": commit, "source_dirty": dirty, "source_identity_kind": kind,
        "release_eligible": commit.is_some() && dirty == Some(false),
        "target": env::var("TARGET").unwrap(), "profile": env::var("PROFILE").unwrap(),
        "rustc": String::from_utf8_lossy(&compiler.stdout).trim(),
    });
    fs::write(out.join("build-identity.json"), identity.to_string()).unwrap();
}
