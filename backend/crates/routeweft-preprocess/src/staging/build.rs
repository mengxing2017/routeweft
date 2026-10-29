//! Explicit bounded external v2 build coordinator. No resident Osm or Graph.
use super::{
    connection::prepare_with_policy, publish_block_ways, stage, BlockWaysOptions, BuildProgress,
    RestrictionOptions, StageOptions,
};
use crate::{BuildOptions, Error, InputReport, Result};
use routeweft_dataset::{hash_file, invalid, resource_limit, BinaryWriteOptions, Manifest};
use serde::Serialize;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Global disk allowance is stage.scratch_bytes / stage.run_files, including all
/// retained intermediates and the final two files. Other limits are per phase.
#[derive(Clone, Copy)]
pub struct ExternalBuildOptions {
    pub build: BuildOptions,
    pub stage: StageOptions,
    pub writer: BinaryWriteOptions,
    pub restriction_access_bytes: u64,
    pub restriction_aggregation_bytes: u64,
}
impl Default for ExternalBuildOptions {
    fn default() -> Self {
        let restriction = RestrictionOptions::default();
        Self {
            build: BuildOptions::default(),
            stage: StageOptions::default(),
            writer: BinaryWriteOptions::default(),
            restriction_access_bytes: restriction.relation_access_bytes,
            restriction_aggregation_bytes: restriction.aggregation_bytes,
        }
    }
}
/// Cooperative checks at phase boundaries, including before final commit.
/// A running phase is not interrupted; this is not a hard wall-clock timeout.
#[derive(Clone, Copy, Default)]
pub struct ExternalBuildControl<'a> {
    pub cancelled: Option<&'a AtomicBool>,
    pub deadline: Option<Instant>,
}
impl ExternalBuildControl<'_> {
    fn check(self) -> Result<()> {
        let reason = if self.cancelled.is_some_and(|v| v.load(Ordering::Relaxed)) {
            Some(("BuildCancelled", "external build cancelled"))
        } else if self.deadline.is_some_and(|v| Instant::now() >= v) {
            Some(("BuildTimeout", "external build phase deadline exceeded"))
        } else {
            None
        };
        if let Some((code, message)) = reason {
            let mut error: Error = invalid(message).into();
            error.report.failure_code = Some(code.into());
            return Err(error);
        }
        Ok(())
    }
}
#[derive(Debug, Serialize)]
pub struct ExternalBuildResources {
    pub scratch_limit_bytes: u64,
    pub file_limit: usize,
    /// Conservative accounted upper bound, including reserved stage metadata.
    pub peak_accounted_scratch_bytes: u64,
    /// Completed checkpoints only; active files are enforced by child allowances.
    pub peak_completed_files: usize,
    pub staging_initial_runs: u64,
    pub graph_attempts: u64,
}
#[derive(Serialize)]
pub struct ExternalBuildResult {
    pub manifest: Manifest,
    pub report: InputReport,
    pub resources: ExternalBuildResources,
}
struct Owned {
    root: PathBuf,
    options: StageOptions,
    peak: u64,
    completed_files: usize,
}
impl Drop for Owned {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn usage(directory: &Path) -> Result<(u64, usize)> {
    let mut bytes = 0_u64;
    let mut files = 0_usize;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let (b, f) = if kind.is_dir() {
            usage(&entry.path())?
        } else if kind.is_file() {
            (entry.metadata()?.len(), 1)
        } else {
            return Err(invalid("external scratch must contain regular files").into());
        };
        bytes = bytes
            .checked_add(b)
            .ok_or_else(|| invalid("external scratch overflow"))?;
        files = files
            .checked_add(f)
            .ok_or_else(|| invalid("external file count overflow"))?;
    }
    Ok((bytes, files))
}
impl Owned {
    fn observe(&mut self, bytes: u64, files: usize) -> Result<()> {
        if bytes > self.options.scratch_bytes {
            return Err(resource_limit(
                "build_scratch_bytes",
                self.options.scratch_bytes,
                bytes,
                "external_build",
            )
            .into());
        }
        if files > self.options.run_files {
            return Err(resource_limit(
                "build_scratch_files",
                self.options.run_files as u64,
                files as u64,
                "external_build",
            )
            .into());
        }
        self.peak = self.peak.max(bytes);
        self.completed_files = self.completed_files.max(files);
        Ok(())
    }
    fn remaining(&mut self) -> Result<(u64, usize, u64)> {
        let (bytes, files) = usage(&self.root)?;
        self.observe(bytes, files)?;
        Ok((
            self.options.scratch_bytes - bytes,
            self.options.run_files - files,
            bytes,
        ))
    }
}
#[derive(Clone, Copy)]
enum Phase {
    Staging,
    Graph,
    Publication,
    Commit,
}

/// Build v2 through disk staging, policy-aware restriction resolution and full
/// streamed publication. Scratch parent and output parent must be on the same
/// filesystem for the final rename. Source and parents must remain immutable.
/// Existing targets are never overwritten. Controlled failures clean owned scratch;
/// crashes may leave private directories, and directory sync can fail after commit.
pub fn build_external(
    input: &Path,
    output: &Path,
    scratch_parent: &Path,
    options: ExternalBuildOptions,
    control: ExternalBuildControl<'_>,
) -> Result<ExternalBuildResult> {
    run(input, output, scratch_parent, options, |_| control.check()).map_err(|mut error| {
        error.report.policy = options.build.invalid_restrictions;
        error.report.maxspeed_policy = options.build.invalid_maxspeed;
        if error.report.failure_code.is_none() {
            error.report.failure_code = Some("ExternalBuildError".into());
        }
        error
    })
}
fn run(
    input: &Path,
    output: &Path,
    scratch_parent: &Path,
    options: ExternalBuildOptions,
    mut checkpoint: impl FnMut(Phase) -> Result<()>,
) -> Result<ExternalBuildResult> {
    options.build.restriction_audit.validate()?;
    if fs::symlink_metadata(output).is_ok() {
        return Err(invalid("output already exists").into());
    }
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fn absolute(path: &Path) -> std::io::Result<PathBuf> {
        let joined = std::env::current_dir()?.join(path);
        let mut normalized = PathBuf::new();
        for component in joined.components() {
            match component {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    normalized.pop();
                }
                other => normalized.push(other.as_os_str()),
            }
        }
        let mut ancestor = normalized;
        let mut missing = Vec::new();
        while !ancestor.try_exists()? {
            missing.push(
                ancestor
                    .file_name()
                    .ok_or_else(|| std::io::Error::other("scratch path"))?
                    .to_owned(),
            );
            ancestor.pop();
        }
        let mut resolved = fs::canonicalize(ancestor)?;
        for part in missing.into_iter().rev() {
            resolved.push(part);
        }
        Ok(resolved)
    }
    if absolute(scratch_parent)?.starts_with(absolute(output)?) {
        return Err(invalid("scratch parent must be outside output").into());
    }
    fs::create_dir_all(parent)?;
    fs::create_dir_all(scratch_parent)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| invalid("clock"))?
        .as_nanos();
    let root = scratch_parent.join(format!(".routeweft-build-{}-{nonce}", std::process::id()));
    fs::create_dir(&root)?;
    let mut owned = Owned {
        root,
        options: options.stage,
        peak: 0,
        completed_files: 0,
    };
    let mut context = InputReport {
        policy: options.build.invalid_restrictions,
        maxspeed_policy: options.build.invalid_maxspeed,
        ..Default::default()
    };
    let build_started = Instant::now();
    let mut staging_ms = 0;
    let result = (|| {
        checkpoint(Phase::Staging)?;
        options.stage.observe(BuildProgress {
            stage: "staging",
            scratch_scope: "build_checkpoints",
            cancellation_checkpoint: true,
            ..Default::default()
        });
        // Stage budgets runs, not its completion manifest. Reserve its fixed bound.
        const METADATA: u64 = 1024 * 1024;
        owned.observe(METADATA, 2)?;
        let staged = owned.root.join("stage");
        let staged_report = stage(
            input,
            &staged,
            StageOptions {
                scratch_bytes: options.stage.scratch_bytes - METADATA,
                run_files: options.stage.run_files - 1,
                ..options.stage
            },
        )?;
        owned.observe(
            staged_report
                .peak_scratch_bytes
                .checked_add(METADATA)
                .ok_or_else(|| invalid("staging peak overflow"))?,
            0,
        )?;
        staging_ms = build_started.elapsed().as_millis() as u64;
        context.source_sha256 = staged_report.source_sha256.clone();
        context.source_snapshot = staged_report.source_snapshot;
        for (key, count) in ["input_nodes", "input_ways", "input_relations"]
            .into_iter()
            .zip(staged_report.counts)
        {
            context.diagnostics.insert(key.into(), count);
        }
        let (remaining_bytes, remaining_files, retained) = owned.remaining()?;
        checkpoint(Phase::Graph)?;
        options.stage.observe(BuildProgress {
            stage: "staging_complete",
            completed: staged_report.counts.iter().sum(),
            scratch_peak_bytes: owned.peak,
            scratch_scope: "build_checkpoints",
            cancellation_checkpoint: true,
            ..Default::default()
        });
        let prepared = owned.root.join("prepared");
        let graph = prepare_with_policy(
            &staged,
            &prepared,
            BlockWaysOptions {
                invalid_maxspeed: options.build.invalid_maxspeed,
                restriction: RestrictionOptions {
                    stage: StageOptions {
                        scratch_bytes: remaining_bytes,
                        run_files: remaining_files,
                        ..options.stage
                    },
                    relation_access_bytes: options.restriction_access_bytes,
                    aggregation_bytes: options.restriction_aggregation_bytes,
                    audit: options.build.restriction_audit,
                },
                ..Default::default()
            },
            options.build.invalid_restrictions,
        )?;
        context = InputReport {
            valid: false,
            failure_code: None,
            policy: graph.policy,
            maxspeed_policy: graph.maxspeed_policy,
            maxspeed_diagnostics: graph.maxspeed_diagnostics,
            source_sha256: graph.source_sha256,
            source_snapshot: graph.source_snapshot,
            nodes: graph.nodes as usize,
            edges: graph.edges as usize,
            restrictions: graph.restrictions as usize,
            diagnostics: graph.diagnostics,
            build_metrics: graph.build_metrics,
            restriction_diagnostics: graph.restriction_diagnostics,
            restriction_audit: graph.restriction_audit,
            planned_blocked_way_ids: graph.blocked_way_ids.clone(),
            applied_blocked_way_ids: graph.blocked_way_ids.clone(),
            blockways_failure: None,
            blocked_way_ids: graph.blocked_way_ids,
        };
        owned.observe(
            retained
                .checked_add(graph.peak_scratch_bytes)
                .ok_or_else(|| invalid("graph peak overflow"))?,
            0,
        )?;
        let (remaining_bytes, remaining_files, _) = owned.remaining()?;
        if remaining_files < 2 {
            return Err(resource_limit(
                "build_scratch_files",
                options.stage.run_files as u64,
                (options.stage.run_files - remaining_files + 2) as u64,
                "external_build",
            )
            .into());
        }
        checkpoint(Phase::Publication)?;
        options.stage.observe(BuildProgress {
            stage: "publication",
            attempt: graph.attempts,
            diagnostics_observed: context.restriction_audit.records_observed,
            scratch_peak_bytes: owned.peak,
            scratch_scope: "build_checkpoints",
            cancellation_checkpoint: true,
            ..Default::default()
        });
        let final_dataset = owned.root.join("dataset");
        let publication = publish_block_ways(
            &prepared,
            &final_dataset,
            BinaryWriteOptions {
                graph_capacity: options.stage.graph_capacity,
                output_bytes: options.writer.output_bytes.min(remaining_bytes),
                ..options.writer
            },
        )?;
        owned.remaining()?;
        checkpoint(Phase::Commit)?;
        options.stage.observe(BuildProgress {
            stage: "commit",
            attempt: graph.attempts,
            diagnostics_observed: context.restriction_audit.records_observed,
            scratch_peak_bytes: owned.peak,
            scratch_scope: "build_checkpoints",
            cancellation_checkpoint: true,
            ..Default::default()
        });
        if fs::metadata(input)?.len() > 512 * 1024 * 1024
            || hash_file(input)? != context.source_sha256
        {
            let mut error: Error = invalid("source changed during external build").into();
            error.report.failure_code = Some("SourceChanged".into());
            return Err(error);
        }
        checkpoint(Phase::Commit)?;
        File::open(&owned.root)?.sync_all()?;
        fs::create_dir(output)?;
        if let Err(error) = fs::rename(&final_dataset, output) {
            let _ = fs::remove_dir(output);
            return Err(error.into());
        }
        File::open(parent)?.sync_all()?;
        let mut report = publication.input;
        report.valid = true;
        report.build_metrics.insert("staging_ms".into(), staging_ms);
        report.build_metrics.insert(
            "total_elapsed_ms".into(),
            build_started.elapsed().as_millis() as u64,
        );
        options.stage.observe(BuildProgress {
            stage: "complete",
            attempt: graph.attempts,
            completed: report.edges as u64,
            diagnostics_observed: report.restriction_audit.records_observed,
            scratch_peak_bytes: owned.peak,
            scratch_scope: "build_checkpoints",
            ..Default::default()
        });
        Ok(ExternalBuildResult {
            manifest: publication.manifest,
            report,
            resources: ExternalBuildResources {
                scratch_limit_bytes: options.stage.scratch_bytes,
                file_limit: options.stage.run_files,
                peak_accounted_scratch_bytes: owned.peak,
                peak_completed_files: owned.completed_files,
                staging_initial_runs: staged_report.initial_runs,
                graph_attempts: graph.attempts,
            },
        })
    })();
    result.map_err(|mut error: Error| {
        error
            .report
            .build_metrics
            .insert("staging_ms".into(), staging_ms);
        error.report.build_metrics.insert(
            "total_elapsed_ms".into(),
            build_started.elapsed().as_millis() as u64,
        );
        error.report.valid = false;
        error.report.policy = options.build.invalid_restrictions;
        error.report.maxspeed_policy = options.build.invalid_maxspeed;
        if error.report.failure_code.is_none() {
            error.report.failure_code = Some("ExternalBuildError".into());
        }
        if error.report.source_sha256.is_empty() {
            error.report.source_sha256 = context.source_sha256;
            error.report.source_snapshot = context.source_snapshot;
        }
        if error.report.nodes == 0 {
            error.report.nodes = context.nodes;
            error.report.edges = context.edges;
            error.report.restrictions = context.restrictions;
        }
        if error.report.maxspeed_diagnostics.is_empty() {
            error.report.maxspeed_diagnostics = context.maxspeed_diagnostics;
        }
        if error.report.diagnostics.is_empty() {
            error.report.diagnostics = context.diagnostics;
        }
        if error.report.restriction_audit.attempt == 0 {
            error.report.restriction_audit = context.restriction_audit;
        }
        if error.report.restriction_diagnostics.is_empty() {
            error.report.restriction_diagnostics = context.restriction_diagnostics;
        }
        for (key, value) in context.build_metrics {
            error.report.build_metrics.entry(key).or_insert(value);
        }
        if error.report.blocked_way_ids.is_empty() {
            error.report.blocked_way_ids = context.blocked_way_ids;
            error.report.planned_blocked_way_ids = context.planned_blocked_way_ids;
            error.report.applied_blocked_way_ids = context.applied_blocked_way_ids;
        }
        error
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    fn fixture(case: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "routeweft-build-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let input = root.join("input.pbf");
        let scratch = root.join("work");
        fs::create_dir(&scratch).unwrap();
        let script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/generate_fixture.py");
        assert!(Command::new("python3")
            .arg(script)
            .arg(&input)
            .args(["--restriction-case", case])
            .status()
            .unwrap()
            .success());
        (root, input, scratch)
    }
    fn clean(scratch: &Path, output: &Path) {
        assert_eq!(fs::read_dir(scratch).unwrap().count(), 0);
        assert!(!output.exists());
    }
    #[test]
    fn both_policies_build_identical_v2_and_enforce_combined_disk_allowances() {
        let (root, input, scratch) = fixture("valid");
        for policy in [
            crate::InvalidRestrictionPolicy::Reject,
            crate::InvalidRestrictionPolicy::BlockWays,
        ] {
            let options = ExternalBuildOptions {
                build: BuildOptions {
                    invalid_restrictions: policy,
                    ..Default::default()
                },
                ..Default::default()
            };
            let output = root.join(format!("external-{policy:?}"));
            let built =
                build_external(&input, &output, &scratch, options, Default::default()).unwrap();
            let reference = root.join(format!("memory-{policy:?}"));
            let (manifest, report) = crate::build_with_format(
                &input,
                &reference,
                options.build,
                routeweft_dataset::DatasetFormat::BinaryV2,
            )
            .unwrap();
            assert_eq!(built.manifest, manifest);
            let mut actual = serde_json::to_value(&built.report).unwrap();
            let mut expected = serde_json::to_value(&report).unwrap();
            actual.as_object_mut().unwrap().remove("build_metrics");
            expected.as_object_mut().unwrap().remove("build_metrics");
            assert_eq!(actual, expected);
            assert!(built
                .report
                .build_metrics
                .contains_key("build_way_closure_node_lookups"));
            for file in ["graph.bin", "manifest.json"] {
                assert_eq!(
                    fs::read(output.join(file)).unwrap(),
                    fs::read(reference.join(file)).unwrap()
                );
            }
            assert!(built.resources.peak_accounted_scratch_bytes <= options.stage.scratch_bytes);
            assert_eq!(built.resources.peak_completed_files, 18);
            assert_eq!(fs::read_dir(&scratch).unwrap().count(), 0);
        }
        let output = root.join("failure");
        let staged = stage(&input, &root.join("calibration"), Default::default()).unwrap();
        // Enough for staging + its manifest reservation, but not for the next
        // phase once the two retained staging files are deducted.
        let mut options = ExternalBuildOptions::default();
        options.stage.scratch_bytes = staged.peak_scratch_bytes + 1024 * 1024;
        let error = build_external(&input, &output, &scratch, options, Default::default())
            .err()
            .unwrap();
        assert!(matches!(
            error.source,
            routeweft_dataset::Error::ResourceLimit(_)
        ));
        clean(&scratch, &output);
        // Retained stage (2) + prepared graph (14) leave only one of 17 files;
        // publication requires two, although every earlier phase can complete.
        let mut options = ExternalBuildOptions::default();
        options.stage.run_files = 17;
        let error = build_external(&input, &output, &scratch, options, Default::default())
            .err()
            .unwrap();
        assert!(
            matches!(error.source, routeweft_dataset::Error::ResourceLimit(ref r) if r.resource == "build_scratch_files")
        );
        clean(&scratch, &output);
        let mut options = ExternalBuildOptions::default();
        options.writer.dictionary_bytes = 0;
        let error = build_external(&input, &output, &scratch, options, Default::default())
            .err()
            .unwrap();
        assert!(!error.report.valid);
        assert_eq!(error.report.nodes, 4);
        assert_eq!(error.report.source_sha256.len(), 64);
        clean(&scratch, &output);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn reject_diagnostics_match_memory_and_blockways_never_drops_errors() {
        let (root, input, scratch) = fixture("missing-via");
        let output = root.join("failed");
        for policy in [
            crate::InvalidRestrictionPolicy::Reject,
            crate::InvalidRestrictionPolicy::BlockWays,
        ] {
            let options = ExternalBuildOptions {
                build: BuildOptions {
                    invalid_restrictions: policy,
                    ..Default::default()
                },
                ..Default::default()
            };
            let expected = crate::preflight(&input, options.build).unwrap_err();
            let actual = build_external(&input, &output, &scratch, options, Default::default())
                .err()
                .unwrap();
            assert_eq!(
                serde_json::to_value(&actual.report.restriction_diagnostics).unwrap(),
                serde_json::to_value(&expected.report.restriction_diagnostics).unwrap()
            );
            assert_eq!(
                actual.report.blocked_way_ids,
                expected.report.blocked_way_ids
            );
            assert_eq!(actual.report.policy, policy);
            clean(&scratch, &output);
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cancellation_deadline_source_change_and_late_target_preserve_cleanup() {
        let (root, input, scratch) = fixture("valid");
        let output = root.join("failed");
        let flag = AtomicBool::new(true);
        let error = build_external(
            &input,
            &output,
            &scratch,
            Default::default(),
            ExternalBuildControl {
                cancelled: Some(&flag),
                deadline: None,
            },
        )
        .err()
        .unwrap();
        assert_eq!(error.report.failure_code.as_deref(), Some("BuildCancelled"));
        clean(&scratch, &output);
        let error = build_external(
            &input,
            &output,
            &scratch,
            Default::default(),
            ExternalBuildControl {
                cancelled: None,
                deadline: Some(Instant::now()),
            },
        )
        .err()
        .unwrap();
        assert_eq!(error.report.failure_code.as_deref(), Some("BuildTimeout"));
        clean(&scratch, &output);
        flag.store(false, Ordering::Relaxed);
        let error = run(&input, &output, &scratch, Default::default(), |phase| {
            if matches!(phase, Phase::Commit) {
                flag.store(true, Ordering::Relaxed);
            }
            ExternalBuildControl {
                cancelled: Some(&flag),
                deadline: None,
            }
            .check()
        })
        .err()
        .unwrap();
        assert_eq!(error.report.failure_code.as_deref(), Some("BuildCancelled"));
        assert_eq!(error.report.nodes, 4);
        clean(&scratch, &output);
        let original = fs::read(&input).unwrap();
        let error = run(&input, &output, &scratch, Default::default(), |phase| {
            if matches!(phase, Phase::Commit) {
                fs::write(&input, b"changed")?;
            }
            Ok(())
        })
        .err()
        .unwrap();
        assert_eq!(error.report.failure_code.as_deref(), Some("SourceChanged"));
        clean(&scratch, &output);
        fs::write(&input, original).unwrap();
        assert!(run(&input, &output, &scratch, Default::default(), |phase| {
            if matches!(phase, Phase::Commit) && !output.exists() {
                fs::create_dir(&output)?;
                fs::write(output.join("keep"), b"keep")?;
            }
            Ok(())
        })
        .is_err());
        assert_eq!(fs::read(output.join("keep")).unwrap(), b"keep");
        assert_eq!(fs::read_dir(&scratch).unwrap().count(), 0);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn corrupt_input_and_overlapping_output_scratch_are_rejected_without_artifacts() {
        let (root, input, scratch) = fixture("valid");
        let output = root.join("failed");
        assert!(build_external(
            &input,
            &output,
            &output.join("scratch"),
            Default::default(),
            Default::default()
        )
        .is_err());
        assert!(!output.exists());
        #[cfg(unix)]
        {
            let alias = root.join("alias");
            std::os::unix::fs::symlink(&root, &alias).unwrap();
            assert!(build_external(
                &input,
                &output,
                &alias.join("failed/scratch"),
                Default::default(),
                Default::default()
            )
            .is_err());
            assert!(!output.exists());
        }
        fs::write(&input, b"bad").unwrap();
        assert!(build_external(
            &input,
            &output,
            &scratch,
            Default::default(),
            Default::default()
        )
        .is_err());
        clean(&scratch, &output);
        fs::remove_dir_all(root).unwrap();
    }
}
