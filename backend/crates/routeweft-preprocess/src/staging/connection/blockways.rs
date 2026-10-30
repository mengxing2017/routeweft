//! Bounded intermediate BlockWays retries. No final dataset or runtime publication.
use super::edges::resolve_with_audit;
use super::{connect_excluding, Source};
use crate::audit::{AuditCollector, BoundedWriter, BLOCKWAYS_MANIFEST_MAX_BYTES};
use crate::staging::{emit_edges, index_adjacency, Record, RestrictionOptions, StageOptions, NEXT};
use crate::{InvalidRestrictionPolicy, RestrictionDiagnostic, Result};
use routeweft_dataset::{hash_file, invalid, resource_limit};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

/// Total owned scratch/file allowance uses restriction.stage, including retained phases.
#[derive(Clone, Copy)]
pub struct BlockWaysOptions {
    pub invalid_maxspeed: crate::InvalidMaxspeedPolicy,
    pub restriction: RestrictionOptions,
    pub max_blocked_ways: usize,
    /// Aggregate kind=way member references retained for a failed round.
    pub max_member_references: usize,
}
impl Default for BlockWaysOptions {
    fn default() -> Self {
        Self {
            invalid_maxspeed: Default::default(),
            restriction: RestrictionOptions::default(),
            max_blocked_ways: 32_768,
            max_member_references: 65_536,
        }
    }
}
/// Completed intermediate graph and restriction audit; semantic_validation remains false.
#[derive(Debug, Serialize, Deserialize)]
pub struct BlockWaysReport {
    #[serde(default)]
    pub graph_capacity: routeweft_dataset::GraphCapacity,
    #[serde(default)]
    pub maxspeed_policy: crate::InvalidMaxspeedPolicy,
    #[serde(default)]
    pub maxspeed_diagnostics: Vec<crate::MaxspeedDiagnostic>,
    pub format_version: u32,
    pub policy: InvalidRestrictionPolicy,
    pub source_records_sha256: String,
    pub source_sha256: String,
    pub source_snapshot: Option<i64>,
    pub nodes: u64,
    pub edges: u64,
    pub restrictions: u64,
    pub diagnostics: BTreeMap<String, u64>,
    #[serde(default)]
    pub build_metrics: BTreeMap<String, u64>,
    #[serde(deserialize_with = "crate::audit::deserialize_diagnostics")]
    pub restriction_diagnostics: Vec<RestrictionDiagnostic>,
    #[serde(default)]
    pub restriction_audit: crate::RestrictionAuditSummary,
    pub blocked_way_ids: Vec<i64>,
    pub attempts: u64,
    pub peak_scratch_bytes: u64,
    /// Completed files observed between phases, including the final manifest.
    /// Active child run files are budgeted separately, not sampled here.
    pub peak_retained_files: usize,
    pub max_blocked_ways: usize,
    pub max_member_references: usize,
    pub scratch_limit_bytes: u64,
    pub file_limit: usize,
    pub files: BTreeMap<String, (u64, String)>,
    pub restrictions_resolved: bool,
    pub semantic_validation: bool,
}
struct Owned {
    path: PathBuf,
    published: bool,
    peak: u64,
    peak_retained_files: usize,
    options: StageOptions,
}
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
fn files(directory: &Path, root: &Path, result: &mut BTreeMap<String, u64>) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            files(&entry.path(), root, result)?;
        } else if kind.is_file() {
            let path = entry.path();
            let name = path
                .strip_prefix(root)
                .map_err(|_| invalid("BlockWays owned path"))?
                .to_str()
                .ok_or_else(|| invalid("BlockWays UTF8 path"))?
                .replace('\\', "/");
            result.insert(name, entry.metadata()?.len());
        } else {
            return Err(invalid("BlockWays scratch must contain regular files").into());
        }
    }
    Ok(())
}
impl Owned {
    fn usage(&mut self) -> Result<(u64, usize)> {
        let mut list = BTreeMap::new();
        files(&self.path, &self.path, &mut list)?;
        let bytes = list.values().try_fold(0u64, |sum, bytes| {
            sum.checked_add(*bytes)
                .ok_or_else(|| invalid("BlockWays scratch overflow"))
        })?;
        self.observe(bytes, list.len())?;
        Ok((bytes, list.len()))
    }
    fn observe(&mut self, bytes: u64, count: usize) -> Result<()> {
        self.peak = self.peak.max(bytes);
        self.peak_retained_files = self.peak_retained_files.max(count);
        if bytes > self.options.scratch_bytes {
            return Err(resource_limit(
                "blockways_scratch_bytes",
                self.options.scratch_bytes,
                bytes,
                "external_blockways",
            )
            .into());
        }
        if count > self.options.run_files {
            return Err(resource_limit(
                "blockways_files",
                self.options.run_files as u64,
                count as u64,
                "external_blockways",
            )
            .into());
        }
        Ok(())
    }
    fn remaining(&mut self) -> Result<(StageOptions, u64)> {
        let (bytes, count) = self.usage()?;
        // Child stages account payloads/runs, but write one completion manifest separately.
        // All child manifest readers cap at 1 MiB; reserve that bound and one file.
        let metadata = 1024 * 1024 + BLOCKWAYS_MANIFEST_MAX_BYTES;
        if self.options.run_files.saturating_sub(count) <= 1 {
            return Err(resource_limit(
                "blockways_files",
                self.options.run_files as u64,
                count as u64 + 2,
                "external_blockways",
            )
            .into());
        }
        if self.options.scratch_bytes - bytes < metadata {
            return Err(resource_limit(
                "blockways_scratch_bytes",
                self.options.scratch_bytes,
                bytes + metadata,
                "external_blockways",
            )
            .into());
        }
        Ok((
            StageOptions {
                scratch_bytes: self.options.scratch_bytes - bytes - metadata,
                run_files: self.options.run_files - count - 1,
                ..self.options
            },
            bytes,
        ))
    }

    fn completed(&mut self, retained: u64, stage_peak: u64) -> Result<()> {
        self.observe(
            retained
                .checked_add(stage_peak)
                .ok_or_else(|| invalid("BlockWays scratch overflow"))?,
            0,
        )?;
        self.usage()?;
        Ok(())
    }
}
fn verify_completed(
    directory: &Path,
    report: &impl Serialize,
    descriptors: &BTreeMap<String, (u64, String)>,
) -> Result<()> {
    let manifest = directory.join("manifest.json");
    let encoded = serde_json::to_vec(report).map_err(|e| invalid(e.to_string()))?;
    if !fs::symlink_metadata(&manifest)?.file_type().is_file()
        || fs::metadata(&manifest)?.len() != encoded.len() as u64
        || hash_file(&manifest)? != routeweft_dataset::hash(&encoded)
    {
        return Err(invalid("BlockWays child manifest changed").into());
    }
    for (name, (bytes, digest)) in descriptors {
        let path = directory.join(name);
        if !fs::symlink_metadata(&path)?.file_type().is_file()
            || fs::metadata(&path)?.len() != *bytes
            || hash_file(&path)? != *digest
        {
            return Err(invalid("BlockWays child payload changed").into());
        }
    }
    Ok(())
}
#[derive(Default)]
struct Members {
    all: Vec<i64>,
    relation: Option<crate::pbf::Relation>,
}
fn exclusions(
    source: &Source,
    diagnostics: &mut [RestrictionDiagnostic],
    excluded: &mut BTreeSet<i64>,
    options: BlockWaysOptions,
    policy: InvalidRestrictionPolicy,
) -> Result<Option<crate::BlockWaysFailure>> {
    let lookup: BTreeMap<_, _> = diagnostics
        .iter()
        .enumerate()
        .map(|(i, d)| (d.relation_id, i))
        .collect();
    let mut members: Vec<Members> = (0..diagnostics.len()).map(|_| Members::default()).collect();
    let mut candidates = BTreeSet::new();
    let mut count = 0;
    let mut found = 0;
    let mut relation_bytes = 0u64;
    source.visit_kinds(&[2], |record| {
        if let Record::Relation(rel) = record {
            if let Some(&index) = lookup.get(&rel.id) {
                found += 1;
                for member in rel.members.iter().filter(|m| m.kind == 1) {
                    count += 1;
                    if count > options.max_member_references {
                        return Err(resource_limit(
                            "blockways_member_references",
                            options.max_member_references as u64,
                            count as u64,
                            "external_blockways",
                        )
                        .into());
                    }
                    members[index].all.push(member.id);
                    candidates.insert(member.id);
                }
                relation_bytes += serde_json::to_vec(&rel)
                    .map_err(|e| invalid(e.to_string()))?
                    .len() as u64;
                if relation_bytes > 16 * 1024 * 1024 {
                    return Err(resource_limit(
                        "exclusion_source_relation_bytes",
                        16 * 1024 * 1024,
                        relation_bytes,
                        "external_blockways",
                    )
                    .into());
                }
                members[index].relation = Some(rel);
            }
        }
        Ok(())
    })?;
    if found != diagnostics.len() {
        return Err(invalid("BlockWays relation identity").into());
    }
    let mut present = BTreeSet::new();
    source.visit_kinds(&[1], |record| {
        if let Record::Way(way) = record {
            if candidates.contains(&way.id) {
                present.insert(way.id);
            }
        }
        Ok(())
    })?;
    for (diagnostic, member) in diagnostics.iter_mut().zip(members) {
        if let Some(reason) = crate::quarantine::failure(
            policy,
            member
                .relation
                .as_ref()
                .ok_or_else(|| invalid("missing original relation"))?,
            &diagnostic.code,
            &present,
        ) {
            return Ok(Some(crate::BlockWaysFailure::new(diagnostic, reason)));
        }
        for id in member.all {
            if present.contains(&id) && !excluded.contains(&id) {
                if excluded.len() == options.max_blocked_ways {
                    return Err(resource_limit(
                        "blockways_blocked_ways",
                        options.max_blocked_ways as u64,
                        excluded.len() as u64 + 1,
                        "external_blockways",
                    )
                    .into());
                }
                excluded.insert(id);
            }
        }
        diagnostic.action = "blocked_ways".into();
    }
    Ok(None)
}
/// Rebuild the complete intermediate graph after every conservative exclusion.
/// The source is a completed stage directory. The result is not an Engine dataset.
pub fn prepare_block_ways(
    staged: &Path,
    output: &Path,
    options: BlockWaysOptions,
) -> Result<BlockWaysReport> {
    prepare_with_policy(staged, output, options, InvalidRestrictionPolicy::BlockWays)
}

pub fn prepare_with_policy(
    staged: &Path,
    output: &Path,
    options: BlockWaysOptions,
    policy: InvalidRestrictionPolicy,
) -> Result<BlockWaysReport> {
    options.restriction.audit.validate()?;
    if options.max_blocked_ways > 32_768 || options.max_member_references > 65_536 {
        return Err(invalid("BlockWays limits exceed intermediate caps").into());
    }
    if output.exists() {
        return Err(invalid("BlockWays output exists").into());
    }
    let source = Source::open(staged, options.restriction.stage.record_bytes)?;
    let stage_digest = hash_file(&staged.join("manifest.json"))?;
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let path = parent.join(format!(
        ".routeweft-blockways-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path)?;
    let mut owned = Owned {
        path,
        published: false,
        peak: 0,
        peak_retained_files: 0,
        options: options.restriction.stage,
    };
    let mut excluded = BTreeSet::new();
    let mut applied = Vec::new();
    let mut audit = AuditCollector::new(options.restriction.audit, "external_restrictions")?;
    let mut attempts = 0;
    let mut speed_audit = Vec::new();
    let mut graph_nodes = 0;
    let mut graph_edges = 0;
    let mut graph_diagnostics = BTreeMap::new();
    let mut measurements = BTreeMap::new();
    let result = (|| {
        loop {
            graph_nodes = 0;
            graph_edges = 0;
            graph_diagnostics.clear();
            attempts += 1;
            let started = std::time::Instant::now();
            let progress = |stage, completed, scratch_peak_bytes, diagnostics_observed| {
                options
                    .restriction
                    .stage
                    .observe(crate::staging::BuildProgress {
                        stage,
                        attempt: attempts,
                        completed,
                        diagnostics_observed,
                        scratch_peak_bytes,
                        scratch_scope: "blockways",
                        ..Default::default()
                    });
            };
            progress("connection", 0, owned.peak, audit.summary.records_observed);
            audit.begin(attempts as u32);
            let start = audit.records.len();
            let graph = owned.path.join("graph");
            fs::create_dir(&graph)?;
            let joined = graph.join("connection");
            let edges = graph.join("edges");
            let adjacency = graph.join("adjacency");
            let restrictions = graph.join("restrictions");
            let (stage_options, retained) = owned.remaining()?;
            speed_audit.clear();
            let connection = connect_excluding(
                staged,
                &joined,
                stage_options,
                &excluded,
                options.invalid_maxspeed,
            )?;
            graph_nodes = connection.nodes as usize;
            graph_diagnostics = connection.diagnostics.clone();
            speed_audit = connection.maxspeed_diagnostics.clone();
            owned.completed(retained, connection.peak_scratch_bytes)?;
            let (stage_options, retained) = owned.remaining()?;
            measurements.insert(
                format!("attempt_{attempts}_connection_ms"),
                started.elapsed().as_millis() as u64,
            );
            let phase_started = std::time::Instant::now();
            progress(
                "edges",
                connection.nodes,
                owned.peak,
                audit.summary.records_observed,
            );
            let edge = emit_edges(staged, &joined, &edges, stage_options)?;
            graph_edges = edge.edges as usize;
            applied = excluded.iter().copied().collect();
            graph_diagnostics = edge.diagnostics.clone();
            owned.completed(retained, edge.peak_scratch_bytes)?;
            let (stage_options, retained) = owned.remaining()?;
            measurements.insert(
                format!("attempt_{attempts}_edges_ms"),
                phase_started.elapsed().as_millis() as u64,
            );
            let phase_started = std::time::Instant::now();
            progress(
                "adjacency",
                edge.edges,
                owned.peak,
                audit.summary.records_observed,
            );
            let index = index_adjacency(&edges, &adjacency, stage_options)?;
            owned.completed(retained, index.peak_scratch_bytes)?;
            let (stage_options, retained) = owned.remaining()?;
            measurements.insert(
                format!("attempt_{attempts}_adjacency_ms"),
                phase_started.elapsed().as_millis() as u64,
            );
            let phase_started = std::time::Instant::now();
            progress(
                "restrictions",
                0,
                owned.peak,
                audit.summary.records_observed,
            );
            match resolve_with_audit(
                staged,
                &joined,
                &edges,
                &adjacency,
                &restrictions,
                RestrictionOptions {
                    stage: stage_options,
                    ..options.restriction
                },
                &mut audit,
            ) {
                Ok(resolved) => {
                    measurements.insert(
                        format!("attempt_{attempts}_restrictions_ms"),
                        phase_started.elapsed().as_millis() as u64,
                    );
                    owned.completed(retained, resolved.peak_scratch_bytes)?;
                    verify_completed(&joined, &connection, &connection.files)?;
                    verify_completed(&edges, &edge, &edge.files)?;
                    verify_completed(&adjacency, &index, &index.files)?;
                    verify_completed(&restrictions, &resolved, &resolved.files)?;
                    let mut build_metrics = resolved.build_metrics;
                    build_metrics.extend(measurements.clone());
                    let mut diagnostics = resolved.diagnostics;
                    if policy == InvalidRestrictionPolicy::QuarantineWays {
                        diagnostics.insert(crate::quarantine::MARKER.into(), 1);
                    }
                    diagnostics.insert(
                        "invalid_restrictions_blocked".into(),
                        audit.records.len() as u64,
                    );
                    diagnostics.insert(
                        "invalid_restriction_ways_blocked".into(),
                        excluded.len() as u64,
                    );
                    if hash_file(&source.run.path)? != source.run.digest
                        || hash_file(&staged.join("manifest.json"))? != stage_digest
                    {
                        return Err(invalid("BlockWays source changed").into());
                    }
                    let mut sizes = BTreeMap::new();
                    files(&owned.path, &owned.path, &mut sizes)?;
                    let mut descriptors = BTreeMap::new();
                    for (name, size) in sizes {
                        descriptors
                            .insert(name.clone(), (size, hash_file(&owned.path.join(name))?));
                    }
                    return Ok(BlockWaysReport {
                        graph_capacity: options.restriction.stage.graph_capacity,
                        maxspeed_policy: options.invalid_maxspeed,
                        maxspeed_diagnostics: speed_audit.clone(),
                        format_version: 2,
                        policy,
                        source_records_sha256: source.run.digest.clone(),
                        source_sha256: source.report.source_sha256.clone(),
                        source_snapshot: source.report.source_snapshot,
                        nodes: resolved.nodes,
                        edges: resolved.edges,
                        restrictions: resolved.restrictions,
                        diagnostics,
                        build_metrics,
                        restriction_audit: audit.summary.clone(),
                        restriction_diagnostics: std::mem::take(&mut audit.records),
                        blocked_way_ids: excluded.iter().copied().collect(),
                        attempts,
                        peak_scratch_bytes: owned.peak,
                        peak_retained_files: owned.peak_retained_files,
                        max_blocked_ways: options.max_blocked_ways,
                        max_member_references: options.max_member_references,
                        scratch_limit_bytes: options.restriction.stage.scratch_bytes,
                        file_limit: options.restriction.stage.run_files,
                        files: descriptors,
                        restrictions_resolved: true,
                        semantic_validation: false,
                    });
                }
                Err(mut error) => {
                    owned.usage()?;
                    measurements.insert(
                        format!("attempt_{attempts}_restrictions_ms"),
                        phase_started.elapsed().as_millis() as u64,
                    );
                    error.report.build_metrics.extend(measurements.clone());
                    error.report.build_metrics.insert(
                        "build_attempt_elapsed_ms".into(),
                        started.elapsed().as_millis() as u64,
                    );
                    progress(
                        "restriction_scan_complete",
                        audit.records.len() as u64,
                        owned.peak,
                        audit.summary.records_observed,
                    );
                    if policy == InvalidRestrictionPolicy::Reject || !audit.can_block(&error) {
                        return Err(error);
                    }
                    if attempts >= options.restriction.audit.max_blockways_attempts as u64 {
                        error.source = audit.attempt_limit().source;
                        error.report.failure_code = Some("BlockWaysAttemptLimitExceeded".into());
                        return Err(error);
                    }
                    let before = excluded.len();
                    if let Some(blocker) = exclusions(
                        &source,
                        &mut audit.records[start..],
                        &mut excluded,
                        options,
                        policy,
                    )? {
                        error.report.blockways_failure = Some(blocker);
                        error.report.build_metrics.insert(
                            format!("attempt_{attempts}_new_exclusions"),
                            (excluded.len() - before) as u64,
                        );
                        return Err(error);
                    }
                    audit.refresh()?;
                    measurements.insert(
                        format!("attempt_{attempts}_new_exclusions"),
                        (excluded.len() - before) as u64,
                    );
                    if before == excluded.len() {
                        error.report.blockways_failure = audit.records[start..]
                            .first()
                            .map(|d| crate::BlockWaysFailure::new(d, "NoNewExclusions"));
                        return Err(error);
                    }
                    // No stale edge IDs, adjacency or restrictions survive the retry.
                    fs::remove_dir_all(graph)?;
                }
            }
        }
    })();
    let mut report = match result {
        Ok(report) => report,
        Err(mut error) => {
            audit.refresh()?;
            error.report.build_metrics.extend(measurements.clone());
            error
                .report
                .build_metrics
                .insert("blockways_peak_accounted_scratch_bytes".into(), owned.peak);
            audit.attach(&mut error.report);
            if error.report.nodes == 0 {
                error.report.nodes = graph_nodes;
            }
            if error.report.edges == 0 {
                error.report.edges = graph_edges;
            }
            if error.report.diagnostics.is_empty() {
                error.report.diagnostics = graph_diagnostics;
            }
            error.report.planned_blocked_way_ids = excluded.iter().copied().collect();
            error.report.applied_blocked_way_ids = applied;
            error.report.blocked_way_ids = excluded.into_iter().collect();
            error.report.policy = policy;
            error.report.maxspeed_policy = options.invalid_maxspeed;
            if error.report.maxspeed_diagnostics.is_empty() {
                error.report.maxspeed_diagnostics = speed_audit;
            }
            error.report.source_sha256 = source.report.source_sha256;
            error.report.source_snapshot = source.report.source_snapshot;
            return Err(error);
        }
    };
    let publication = (|| -> Result<()> {
        // Include the top-level manifest itself in the final scratch/file allowance.
        let (retained, count) = owned.usage()?;
        loop {
            let mut encoded = BoundedWriter {
                inner: std::io::sink(),
                written: 0,
                limit: BLOCKWAYS_MANIFEST_MAX_BYTES,
            };
            serde_json::to_writer(&mut encoded, &report).map_err(|_| {
                resource_limit(
                    "blockways_manifest_bytes",
                    BLOCKWAYS_MANIFEST_MAX_BYTES,
                    BLOCKWAYS_MANIFEST_MAX_BYTES + 1,
                    "external_blockways",
                )
            })?;
            let total = retained
                .checked_add(encoded.written)
                .ok_or_else(|| invalid("BlockWays manifest size"))?;
            let peak = owned.peak.max(total);
            let peak_retained_files = owned.peak_retained_files.max(count + 1);
            owned.observe(total, count + 1)?;
            if report.peak_scratch_bytes == peak
                && report.peak_retained_files == peak_retained_files
            {
                let mut file = File::options()
                    .write(true)
                    .create_new(true)
                    .open(owned.path.join("manifest.json"))?;
                let mut writer = BoundedWriter {
                    inner: BufWriter::new(&mut file),
                    written: 0,
                    limit: BLOCKWAYS_MANIFEST_MAX_BYTES,
                };
                serde_json::to_writer(&mut writer, &report).map_err(|e| invalid(e.to_string()))?;
                writer.flush()?;
                drop(writer);
                file.sync_all()?;
                break;
            }
            report.peak_scratch_bytes = peak;
            report.peak_retained_files = peak_retained_files;
        }
        File::open(&owned.path)?.sync_all()?;
        fs::create_dir(output)?;
        if let Err(error) = fs::rename(&owned.path, output) {
            let _ = fs::remove_dir(output);
            return Err(error.into());
        }
        owned.published = true;
        Ok(())
    })();
    if let Err(mut error) = publication {
        error.report.maxspeed_policy = report.maxspeed_policy;
        error.report.maxspeed_diagnostics = report.maxspeed_diagnostics.clone();
        error.report.policy = policy;
        error.report.source_sha256 = report.source_sha256.clone();
        error.report.source_snapshot = report.source_snapshot;
        error.report.nodes = report.nodes as usize;
        error.report.edges = report.edges as usize;
        error.report.diagnostics = report.diagnostics.clone();
        error.report.build_metrics = report.build_metrics.clone();
        error.report.restriction_diagnostics = std::mem::take(&mut report.restriction_diagnostics);
        error.report.restriction_audit = report.restriction_audit.clone();
        error.report.planned_blocked_way_ids = report.blocked_way_ids.clone();
        error.report.applied_blocked_way_ids = report.blocked_way_ids.clone();
        error.report.blocked_way_ids = report.blocked_way_ids.clone();
        error.report.failure_code = Some("IntermediateWriteError".into());
        return Err(error);
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{source, temporary};
    use super::*;
    use crate::pbf::{Member, Node, Osm, Relation, Tags, Way};
    use crate::staging::resolve_restrictions;
    use routeweft_dataset::{Graph, Point};
    use std::io::{BufRead, BufReader};
    fn fixture() -> Osm {
        let mut osm = Osm::default();
        for id in -2..=8 {
            osm.nodes.insert(
                id,
                Node {
                    id,
                    point: Point {
                        lon: 108.0 + (id + 2) as f64 * 0.001,
                        lat: 22.0,
                    },
                    tags: Tags::new(),
                },
            );
        }
        for (id, refs) in [
            (10, vec![-2, -1]),
            (20, vec![-1, 0]),
            (30, vec![-1, 1]),
            (40, vec![0, 2]),
            (50, vec![1, 3]),
            (60, vec![4, 5]),
        ] {
            osm.ways.insert(
                id,
                Way {
                    id,
                    refs,
                    tags: Tags::from([("highway".into(), "residential".into())]),
                },
            );
        }
        osm
    }
    fn relation(id: i64, rule: &str, members: &[(u64, i64, &str)]) -> Relation {
        Relation {
            id,
            tags: Tags::from([
                ("type".into(), "restriction".into()),
                ("restriction".into(), rule.into()),
            ]),
            members: members
                .iter()
                .map(|(kind, id, role)| Member {
                    kind: *kind,
                    id: *id,
                    role: (*role).into(),
                })
                .collect(),
        }
    }
    fn cascade() -> Osm {
        let mut osm = fixture();
        osm.relations.push(relation(
            1,
            "no_right_turn",
            &[(1, 20, "from"), (1, 40, "to")],
        ));
        osm.relations.push(relation(
            2,
            "only_straight_on",
            &[(1, 10, "from"), (0, -1, "via"), (1, 20, "to")],
        ));
        osm.relations.push(relation(
            3,
            "no_right_turn",
            &[(1, 30, "from"), (0, 1, "via"), (1, 50, "to")],
        ));
        osm
    }
    fn graph(output: &Path) -> Graph {
        let node_manifest: super::super::ConnectionReport = serde_json::from_reader(
            File::open(output.join("graph/connection/manifest.json")).unwrap(),
        )
        .unwrap();
        let (bytes, digest) = &node_manifest.files["nodes.run"];
        let run = super::super::DiskRun {
            path: output.join("graph/connection/nodes.run"),
            count: node_manifest.nodes,
            bytes: *bytes,
            digest: digest.clone(),
        };
        let mut rows = super::super::Cursor::open(&run).unwrap();
        let mut nodes = Vec::new();
        while let Some(row) = rows.next().unwrap() {
            nodes.push(Point {
                lon: f64::from_bits(row[2]),
                lat: f64::from_bits(row[3]),
            });
        }
        let edges = BufReader::new(File::open(output.join("graph/edges/edges.jsonl")).unwrap())
            .lines()
            .map(|s| serde_json::from_str(&s.unwrap()).unwrap())
            .collect();
        let forbidden =
            BufReader::new(File::open(output.join("graph/restrictions/forbidden.jsonl")).unwrap())
                .lines()
                .map(|s| serde_json::from_str(&s.unwrap()).unwrap())
                .collect();
        let graph = Graph {
            nodes,
            edges,
            forbidden,
        };
        graph.validate(Default::default()).unwrap();
        graph
    }
    fn compare(osm: &Osm, output: &Path, report: &BlockWaysReport) {
        let (expected, audit) = crate::prepare(
            osm,
            crate::BuildOptions {
                invalid_restrictions: InvalidRestrictionPolicy::BlockWays,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(graph(output), expected);
        assert_eq!(report.diagnostics, audit.diagnostics);
        assert_eq!(report.blocked_way_ids, audit.blocked_way_ids);
        assert_eq!(
            serde_json::to_value(&report.restriction_diagnostics).unwrap(),
            serde_json::to_value(&audit.restriction_diagnostics).unwrap()
        );
        for (name, (bytes, digest)) in &report.files {
            assert_eq!(fs::metadata(output.join(name)).unwrap().len(), *bytes);
            assert_eq!(hash_file(&output.join(name)).unwrap(), *digest);
        }
    }
    #[test]
    fn child_verification_rejects_changed_payload_or_manifest() {
        let parent = temporary();
        let osm = fixture();
        let staged = source(&parent, &osm);
        let joined = parent.join("connection");
        let report = super::super::connect(&staged, &joined, StageOptions::default()).unwrap();
        verify_completed(&joined, &report, &report.files).unwrap();
        let payload = joined.join("references.run");
        let original = fs::read(&payload).unwrap();
        let mut changed = original.clone();
        changed[16] ^= 1;
        fs::write(&payload, changed).unwrap();
        assert!(verify_completed(&joined, &report, &report.files).is_err());
        fs::write(&payload, original).unwrap();
        fs::write(joined.join("manifest.json"), b"{}").unwrap();
        assert!(verify_completed(&joined, &report, &report.files).is_err());
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn final_streamed_dataset_matches_memory_builder_after_cascading_exclusions() {
        let parent = temporary();
        let osm = cascade();
        let staged = source(&parent, &osm);
        let ready = parent.join("ready");
        prepare_block_ways(&staged, &ready, Default::default()).unwrap();
        let (expected, audit) = crate::prepare(
            &osm,
            crate::BuildOptions {
                invalid_restrictions: InvalidRestrictionPolicy::BlockWays,
                ..Default::default()
            },
        )
        .unwrap();
        let published = parent.join("published");
        let report =
            super::super::publish_block_ways(&ready, &published, Default::default()).unwrap();
        assert!(report.input.valid);
        let failed_output = parent.join("failed-publication");
        let failure = super::super::publish_block_ways(
            &ready,
            &failed_output,
            routeweft_dataset::BinaryWriteOptions {
                dictionary_bytes: 0,
                ..Default::default()
            },
        )
        .err()
        .unwrap();
        assert!(!failed_output.exists());
        assert_eq!(
            failure.report.failure_code.as_deref(),
            Some("DatasetPublicationError")
        );
        assert_eq!(failure.report.blocked_way_ids, audit.blocked_way_ids);
        assert_eq!(
            serde_json::to_value(&failure.report.restriction_diagnostics).unwrap(),
            serde_json::to_value(&audit.restriction_diagnostics).unwrap()
        );
        assert_eq!(report.input.blocked_way_ids, audit.blocked_way_ids);
        assert_eq!(
            serde_json::to_value(&report.input.restriction_diagnostics).unwrap(),
            serde_json::to_value(&audit.restriction_diagnostics).unwrap()
        );
        let reference = parent.join("reference");
        let manifest = routeweft_dataset::write_with_format(
            &reference,
            &expected,
            "0".repeat(64),
            audit.source_snapshot,
            audit.diagnostics,
            routeweft_dataset::DatasetFormat::BinaryV2,
        )
        .unwrap();
        assert_eq!(report.manifest, manifest);
        assert_eq!(
            routeweft_dataset::open(&published, Default::default())
                .unwrap()
                .1,
            expected
        );
        for file in ["graph.bin", "manifest.json"] {
            assert_eq!(
                fs::read(published.join(file)).unwrap(),
                fs::read(reference.join(file)).unwrap()
            );
        }
        let engine = routeweft_routing::Engine::open(&published, Default::default()).unwrap();
        engine
            .nearest(expected.nodes[0], Default::default(), &Default::default())
            .unwrap();
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn configured_count_boundary_and_final_allowed_round_match_memory() {
        for count in [255, 256, 257] {
            let parent = temporary();
            let mut osm = fixture();
            for id in 1..=count {
                osm.relations.push(relation(
                    id,
                    "no_right_turn",
                    &[(1, 20, "from"), (1, 40, "to")],
                ));
            }
            let staged = source(&parent, &osm);
            let limits = crate::RestrictionAuditLimits {
                max_diagnostics: 256,
                ..Default::default()
            };
            let options = BlockWaysOptions {
                restriction: RestrictionOptions {
                    audit: limits,
                    ..Default::default()
                },
                ..Default::default()
            };
            let error = prepare_with_policy(
                &staged,
                &parent.join("failed"),
                options,
                InvalidRestrictionPolicy::Reject,
            )
            .unwrap_err();
            let memory = crate::prepare(
                &osm,
                crate::BuildOptions {
                    restriction_audit: limits,
                    ..Default::default()
                },
            )
            .unwrap_err();
            assert_eq!(
                error.report.restriction_audit,
                memory.report.restriction_audit
            );
            assert_eq!(
                error.report.restriction_diagnostics.len(),
                count.min(256) as usize
            );
            assert_eq!(error.report.restriction_audit.truncated, count > 256);
            assert_eq!(
                error.report.failure_code.as_deref(),
                Some(if count > 256 {
                    "RestrictionAuditLimitExceeded"
                } else {
                    "InvalidRestriction"
                })
            );
            fs::remove_dir_all(parent).unwrap();
        }
        let parent = temporary();
        let osm = cascade();
        let staged = source(&parent, &osm);
        let options = BlockWaysOptions {
            restriction: RestrictionOptions {
                audit: crate::RestrictionAuditLimits {
                    max_blockways_attempts: 3,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let report = prepare_block_ways(&staged, &parent.join("ready"), options).unwrap();
        assert_eq!(report.attempts, 3);
        let options = BlockWaysOptions {
            restriction: RestrictionOptions {
                audit: crate::RestrictionAuditLimits {
                    max_blockways_attempts: 2,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let error = prepare_block_ways(&staged, &parent.join("failed"), options).unwrap_err();
        assert_eq!(error.report.restriction_audit.attempt, 2);
        assert_eq!(
            error.report.failure_code.as_deref(),
            Some("BlockWaysAttemptLimitExceeded")
        );
        assert!(
            matches!(error.source,routeweft_dataset::Error::ResourceLimit(ref r) if r.observed==3 && r.limit==2)
        );
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn province_sized_audit_survives_reject_blocking_and_publication() {
        let parent = temporary();
        let mut osm = fixture();
        for id in 1..=728 {
            osm.relations.push(relation(
                id,
                "no_right_turn",
                &[(1, 20, "from"), (1, 40, "to")],
            ));
        }
        let staged = source(&parent, &osm);
        let rejected = parent.join("rejected");
        let disk = prepare_with_policy(
            &staged,
            &rejected,
            Default::default(),
            InvalidRestrictionPolicy::Reject,
        )
        .unwrap_err();
        let memory = crate::prepare(&osm, Default::default()).unwrap_err();
        assert_eq!(disk.report.restriction_diagnostics.len(), 728);
        assert_eq!(disk.report.nodes, memory.report.nodes);
        assert_eq!(disk.report.edges, memory.report.edges);
        assert_eq!(
            disk.report.restriction_audit,
            memory.report.restriction_audit
        );
        assert!(disk.report.restriction_audit.scan_complete);
        assert_eq!(
            disk.report.failure_code.as_deref(),
            Some("InvalidRestriction")
        );
        assert!(!rejected.exists());
        let output = parent.join("ready");
        let report = prepare_block_ways(&staged, &output, Default::default()).unwrap();
        assert_eq!(report.format_version, 2);
        assert_eq!(report.attempts, 2);
        assert_eq!(report.restriction_diagnostics.len(), 728);
        assert_eq!(report.blocked_way_ids, [20, 40]);
        let published = crate::staging::publish_block_ways(
            &output,
            &parent.join("dataset"),
            Default::default(),
        )
        .unwrap();
        assert_eq!(published.input.restriction_audit.records_retained, 728);
        routeweft_dataset::open(&parent.join("dataset"), Default::default()).unwrap();
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn audit_exhaustion_and_attempt_limits_never_trigger_partial_rebuilds() {
        let parent = temporary();
        let mut osm = fixture();
        // Round one: 200 missing-via failures. Round two: 100 only exits now excluded.
        for id in 1..=200 {
            osm.relations.push(relation(
                id,
                "no_right_turn",
                &[(1, 20, "from"), (1, 40, "to")],
            ));
        }
        for id in 201..=300 {
            osm.relations.push(relation(
                id,
                "only_straight_on",
                &[(1, 10, "from"), (0, -1, "via"), (1, 20, "to")],
            ));
        }
        let staged = source(&parent, &osm);
        let options = BlockWaysOptions {
            restriction: RestrictionOptions {
                audit: crate::RestrictionAuditLimits {
                    max_diagnostics: 256,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let error = prepare_block_ways(&staged, &parent.join("failed"), options).unwrap_err();
        assert_eq!(error.report.restriction_diagnostics.len(), 256);
        assert_eq!(error.report.restriction_audit.records_observed, 257);
        assert_eq!(error.report.restriction_audit.attempt, 2);
        assert_eq!(error.report.restriction_audit.attempts_completed, 1);
        assert_eq!(error.report.blocked_way_ids, [20, 40]);
        assert!(error.report.restriction_diagnostics[..200]
            .iter()
            .all(|d| d.action == "blocked_ways"));
        assert!(error.report.restriction_diagnostics[200..]
            .iter()
            .all(|d| d.action == "rejected"));
        let memory = crate::prepare(
            &osm,
            crate::BuildOptions {
                invalid_restrictions: InvalidRestrictionPolicy::BlockWays,
                restriction_audit: options.restriction.audit,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(
            error.report.restriction_audit,
            memory.report.restriction_audit
        );
        let options = BlockWaysOptions {
            restriction: RestrictionOptions {
                audit: crate::RestrictionAuditLimits {
                    max_blockways_attempts: 1,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let error = prepare_block_ways(&staged, &parent.join("failed"), options).unwrap_err();
        assert_eq!(
            error.report.failure_code.as_deref(),
            Some("BlockWaysAttemptLimitExceeded")
        );
        assert_eq!(error.report.restriction_diagnostics.len(), 200);
        assert!(error.report.blocked_way_ids.is_empty());
        assert!(error.report.restriction_audit.scan_complete);
        assert!(!parent.join("failed").exists());
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn larger_than_legacy_manifest_roundtrips_without_changing_child_limits() {
        let parent = temporary();
        let mut osm = fixture();
        for id in 1..=100 {
            let mut rel = relation(id, "no_right_turn", &[(1, 20, "from"), (1, 40, "to")]);
            for tag in 0..30 {
                rel.tags
                    .insert(format!("restriction:extra{tag:02}"), "x".repeat(512));
            }
            osm.relations.push(rel);
        }
        let staged = source(&parent, &osm);
        let output = parent.join("ready");
        prepare_block_ways(&staged, &output, Default::default()).unwrap();
        assert!(fs::metadata(output.join("manifest.json")).unwrap().len() > 1024 * 1024);
        crate::staging::publish_block_ways(&output, &parent.join("dataset"), Default::default())
            .unwrap();
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn cascading_exclusions_rebuild_and_renumber_graph_with_matching_audit() {
        let parent = temporary();
        let osm = cascade();
        let staged = source(&parent, &osm);
        let output = parent.join("ready");
        let report = prepare_block_ways(&staged, &output, BlockWaysOptions::default()).unwrap();
        assert_eq!(report.attempts, 3);
        assert_eq!(report.blocked_way_ids, vec![10, 20, 40]);
        compare(&osm, &output, &report);
        assert_eq!(fs::read_dir(&output).unwrap().count(), 2);
        assert!(prepare_block_ways(&staged, &output, BlockWaysOptions::default()).is_err());
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn all_full_members_are_considered_including_after_diagnostic_truncation() {
        let parent = temporary();
        let mut osm = fixture();
        let mut members = vec![(1, 10, "from"), (1, 20, "to")];
        members.extend(std::iter::repeat_n((0, 999, "unknown"), 260));
        members.push((1, 60, "unknown"));
        osm.relations.push(relation(1, "no_right_turn", &members));
        let staged = source(&parent, &osm);
        let output = parent.join("ready");
        let report = prepare_block_ways(&staged, &output, BlockWaysOptions::default()).unwrap();
        assert!(report.restriction_diagnostics[0].members_truncated);
        assert_eq!(report.blocked_way_ids, vec![10, 20, 60]);
        compare(&osm, &output, &report);
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn retained_via_does_not_justify_blocking_when_all_from_to_are_absent() {
        let parent = temporary();
        let mut osm = fixture();
        osm.relations.push(relation(
            1,
            "no_right_turn",
            &[(1, 999, "from"), (1, 30, "via"), (1, 998, "to")],
        ));
        let staged = source(&parent, &osm);
        let output = parent.join("ready");
        let expected = crate::prepare(
            &osm,
            crate::BuildOptions {
                invalid_restrictions: InvalidRestrictionPolicy::BlockWays,
                ..Default::default()
            },
        )
        .unwrap_err();
        let actual = prepare_block_ways(&staged, &output, BlockWaysOptions::default()).unwrap_err();
        assert_eq!(
            serde_json::to_value(actual.report.restriction_diagnostics).unwrap(),
            serde_json::to_value(expected.report.restriction_diagnostics).unwrap()
        );
        assert!(actual.report.blocked_way_ids.is_empty());
        assert!(!output.exists());
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn unsafe_abort_preserves_plans_without_claiming_application() {
        let parent = temporary();
        let mut osm = fixture();
        osm.relations.push(relation(
            1,
            "no_right_turn",
            &[(1, 20, "from"), (1, 40, "to")],
        ));
        osm.relations.push(relation(
            2,
            "no_right_turn",
            &[(1, 999, "from"), (1, 30, "via"), (1, 998, "to")],
        ));
        let staged = source(&parent, &osm);
        let output = parent.join("ready");
        let expected = crate::prepare(
            &osm,
            crate::BuildOptions {
                invalid_restrictions: InvalidRestrictionPolicy::BlockWays,
                ..Default::default()
            },
        )
        .unwrap_err();
        let actual = prepare_block_ways(&staged, &output, BlockWaysOptions::default()).unwrap_err();
        assert_eq!(
            serde_json::to_value(&actual.report.restriction_diagnostics).unwrap(),
            serde_json::to_value(&expected.report.restriction_diagnostics).unwrap()
        );
        assert_eq!(actual.report.blocked_way_ids, vec![20, 40]);
        assert_eq!(actual.report.planned_blocked_way_ids, vec![20, 40]);
        assert!(actual.report.applied_blocked_way_ids.is_empty());
        assert_eq!(
            actual.report.planned_blocked_way_ids,
            expected.report.planned_blocked_way_ids
        );
        let blocker = actual.report.blockways_failure.as_ref().unwrap();
        assert_eq!(blocker.relation_id, 2);
        assert_eq!(blocker.reason, "ReferencedWayAbsent");
        assert_eq!(blocker.attempt, 1);
        assert!(!blocker.recoverable);
        assert!(actual
            .to_string()
            .contains("relation 2: ReferencedWayAbsent"));
        assert!(!output.exists());
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn quarantine_checks_full_members_beyond_projection_and_repeated_exclusions_are_idempotent() {
        for unbounded in [false, true] {
            let parent = temporary();
            let mut osm = fixture();
            let mut members = vec![(1, 20, "")];
            members.extend(std::iter::repeat_n((0, 999, ""), 260));
            members.push(if unbounded { (2, 999, "") } else { (1, 60, "") });
            osm.relations
                .push(relation(5652903, "no_right_turn", &members));
            let staged = source(&parent, &osm);
            let policy = InvalidRestrictionPolicy::QuarantineWays;
            let memory = crate::prepare(
                &osm,
                crate::BuildOptions {
                    invalid_restrictions: policy,
                    ..Default::default()
                },
            );
            let disk =
                prepare_with_policy(&staged, &parent.join("ready"), Default::default(), policy);
            if unbounded {
                for error in [memory.unwrap_err(), disk.unwrap_err()] {
                    let blocker = error.report.blockways_failure.as_ref().unwrap();
                    assert_eq!(blocker.reason, "UnboundedMemberDependency");
                    assert_eq!(blocker.relation_id, 5652903);
                    assert!(!blocker.recoverable);
                    assert!(error.report.restriction_diagnostics[0].members_truncated);
                    assert!(blocker.members.iter().all(|m| m.kind != "relation"));
                    assert!(error.report.planned_blocked_way_ids.is_empty());
                    assert!(error.report.applied_blocked_way_ids.is_empty());
                    assert!(!error.report.restriction_audit.truncated);
                }
                assert!(!parent.join("ready").exists());
            } else {
                let (graph, audit) = memory.unwrap();
                let disk = disk.unwrap();
                assert_eq!(disk.blocked_way_ids, vec![20, 60]);
                assert_eq!(graph, self::graph(&parent.join("ready")));
                assert!(graph
                    .edges
                    .iter()
                    .all(|e| !disk.blocked_way_ids.contains(&e.way)));
                let source = Source::open(&staged, 4 * 1024 * 1024).unwrap();
                let mut records = audit.restriction_diagnostics;
                let mut excluded = BTreeSet::new();
                for _ in 0..2 {
                    assert!(exclusions(
                        &source,
                        &mut records,
                        &mut excluded,
                        Default::default(),
                        policy
                    )
                    .unwrap()
                    .is_none());
                    assert_eq!(excluded, BTreeSet::from([20, 60]));
                }
            }
            fs::remove_dir_all(parent).unwrap();
        }
    }
    #[test]
    fn quarantine_missing_from_to_ways_only_excludes_present_dependency() {
        let parent = temporary();
        let mut osm = fixture();
        osm.relations.push(relation(
            5652903,
            "no_right_turn",
            &[(1, 999, "from"), (1, 30, "via"), (1, 998, "to")],
        ));
        let staged = source(&parent, &osm);
        let policy = InvalidRestrictionPolicy::QuarantineWays;
        let (memory, audit) = crate::prepare(
            &osm,
            crate::BuildOptions {
                invalid_restrictions: policy,
                ..Default::default()
            },
        )
        .unwrap();
        let output = parent.join("ready");
        let disk = prepare_with_policy(&staged, &output, Default::default(), policy).unwrap();
        assert_eq!(memory, graph(&output));
        assert_eq!(disk.blocked_way_ids, vec![30]);
        assert_eq!(audit.planned_blocked_way_ids, vec![30]);
        assert_eq!(audit.applied_blocked_way_ids, vec![30]);
        assert!(memory.edges.iter().all(|e| e.way != 30));
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn quarantine_member_budget_uses_full_source_not_projected_member_count() {
        let parent = temporary();
        let mut osm = fixture();
        let members = vec![(1, 20, ""); 65_537];
        osm.relations
            .push(relation(5652903, "no_right_turn", &members));
        let staged = source(&parent, &osm);
        let policy = InvalidRestrictionPolicy::QuarantineWays;
        let memory = crate::prepare(
            &osm,
            crate::BuildOptions {
                invalid_restrictions: policy,
                ..Default::default()
            },
        )
        .unwrap_err();
        let disk = prepare_with_policy(&staged, &parent.join("ready"), Default::default(), policy)
            .unwrap_err();
        for error in [&memory, &disk] {
            assert!(
                matches!(&error.source, routeweft_dataset::Error::ResourceLimit(r)
                if r.resource == "blockways_member_references" && r.limit == 65_536 && r.observed == 65_537)
            );
            assert!(error.report.restriction_diagnostics[0].members_truncated);
            assert!(error.report.planned_blocked_way_ids.is_empty());
            assert!(error.report.applied_blocked_way_ids.is_empty());
            assert!(!error.report.valid);
        }
        assert!(!parent.join("ready").exists());
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn quarantine_budget_failures_keep_plans_unapplied_and_never_publish() {
        for count_limit in [false, true] {
            let parent = temporary();
            let mut osm = fixture();
            for id in [5652903, 5652904] {
                osm.relations
                    .push(relation(id, "no_right_turn", &[(1, 20, ""), (1, 40, "")]));
            }
            let staged = source(&parent, &osm);
            let limits = crate::RestrictionAuditLimits {
                max_diagnostics: if count_limit { 1 } else { 4096 },
                max_blockways_attempts: 1,
                ..Default::default()
            };
            let policy = InvalidRestrictionPolicy::QuarantineWays;
            let memory = crate::prepare(
                &osm,
                crate::BuildOptions {
                    invalid_restrictions: policy,
                    restriction_audit: limits,
                    ..Default::default()
                },
            )
            .unwrap_err();
            let disk = prepare_with_policy(
                &staged,
                &parent.join("ready"),
                BlockWaysOptions {
                    restriction: RestrictionOptions {
                        audit: limits,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                policy,
            )
            .unwrap_err();
            for error in [&memory, &disk] {
                assert!(!error.report.valid);
                assert_eq!(error.report.restriction_audit.attempt, 1);
                assert_eq!(error.report.restriction_audit.truncated, count_limit);
                assert_eq!(
                    error.report.failure_code.as_deref(),
                    Some(if count_limit {
                        "RestrictionAuditLimitExceeded"
                    } else {
                        "BlockWaysAttemptLimitExceeded"
                    })
                );
                assert!(error.report.planned_blocked_way_ids.is_empty());
                assert!(error.report.applied_blocked_way_ids.is_empty());
            }
            assert_eq!(
                memory.report.restriction_audit,
                disk.report.restriction_audit
            );
            assert!(!parent.join("ready").exists());
            fs::remove_dir_all(parent).unwrap();
        }
    }
    #[test]
    fn unbounded_dependencies_cannot_be_declared_irrelevant_without_visible_roads() {
        for policy in [
            InvalidRestrictionPolicy::BlockWays,
            InvalidRestrictionPolicy::QuarantineWays,
        ] {
            let parent = temporary();
            let mut osm = fixture();
            osm.relations
                .push(relation(5652903, "no_right_turn", &[(2, 999, "via")]));
            let staged = source(&parent, &osm);
            let memory = crate::prepare(
                &osm,
                crate::BuildOptions {
                    invalid_restrictions: policy,
                    ..Default::default()
                },
            )
            .unwrap_err();
            let disk =
                prepare_with_policy(&staged, &parent.join("ready"), Default::default(), policy)
                    .unwrap_err();
            for error in [&memory, &disk] {
                let blocker = error.report.blockways_failure.as_ref().unwrap();
                assert_eq!(blocker.relation_id, 5652903);
                assert_eq!(blocker.attempt, 1);
                assert_eq!(blocker.reason, "UnboundedMemberDependency");
                assert!(!blocker.recoverable);
                assert!(error.report.applied_blocked_way_ids.is_empty());
            }
            assert!(!parent.join("ready").exists());
            fs::remove_dir_all(parent).unwrap();
        }
    }
    #[test]
    fn quarantine_empty_roles_cascades_only_exits_and_publishes_loss_marker() {
        let parent = temporary();
        let mut osm = fixture();
        osm.relations.push(relation(
            5652903,
            "no_right_turn",
            &[(1, 20, ""), (1, 40, "")],
        ));
        osm.relations.push(relation(
            2,
            "only_straight_on",
            &[(1, 10, "from"), (0, -1, "via"), (1, 20, "to")],
        ));
        let staged = source(&parent, &osm);
        let output = parent.join("ready");
        assert!(crate::prepare(&osm, Default::default()).is_err());
        let policy = InvalidRestrictionPolicy::QuarantineWays;
        let (memory, audit) = crate::prepare(
            &osm,
            crate::BuildOptions {
                invalid_restrictions: policy,
                ..Default::default()
            },
        )
        .unwrap();
        let disk = prepare_with_policy(&staged, &output, Default::default(), policy).unwrap();
        assert_eq!(disk.attempts, 3);
        assert_eq!(disk.blocked_way_ids, vec![10, 20, 40]);
        assert_eq!(graph(&output), memory);
        assert_eq!(audit.diagnostics, disk.diagnostics);
        assert!(memory
            .edges
            .iter()
            .all(|e| !disk.blocked_way_ids.contains(&e.way)));
        let published = parent.join("published");
        let result =
            super::super::publish_block_ways(&output, &published, Default::default()).unwrap();
        assert_eq!(result.manifest.diagnostics[crate::quarantine::MARKER], 1);
        assert_eq!(result.input.applied_blocked_way_ids, disk.blocked_way_ids);
        // The policy marker is authenticated along with the child counts/audit.
        let manifest = output.join("manifest.json");
        let mut changed: serde_json::Value =
            serde_json::from_reader(File::open(&manifest).unwrap()).unwrap();
        changed["policy"] = serde_json::json!("block_ways");
        fs::write(manifest, serde_json::to_vec(&changed).unwrap()).unwrap();
        assert!(super::super::publish_block_ways(
            &output,
            &parent.join("tampered"),
            Default::default()
        )
        .is_err());
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn quarantine_refuses_nested_dependencies_and_unapproved_values() {
        for (rule, member, reason) in [
            ("no_right_turn", (2, 999, ""), "UnboundedMemberDependency"),
            (
                "unrecognized",
                (0, -1, "via"),
                "QuarantineCategoryNotApproved",
            ),
        ] {
            let parent = temporary();
            let mut osm = fixture();
            osm.relations
                .push(relation(1, rule, &[(1, 10, ""), member]));
            let staged = source(&parent, &osm);
            let policy = InvalidRestrictionPolicy::QuarantineWays;
            let memory = crate::prepare(
                &osm,
                crate::BuildOptions {
                    invalid_restrictions: policy,
                    ..Default::default()
                },
            )
            .unwrap_err();
            let disk =
                prepare_with_policy(&staged, &parent.join("ready"), Default::default(), policy)
                    .unwrap_err();
            for error in [&memory, &disk] {
                assert_eq!(
                    error.report.blockways_failure.as_ref().unwrap().reason,
                    reason
                );
                assert!(!error.report.valid);
                assert!(error.report.applied_blocked_way_ids.is_empty());
            }
            fs::remove_dir_all(parent).unwrap();
        }
    }
    #[test]
    fn guangdong_empty_roles_fail_closed_in_both_builders() {
        let parent = temporary();
        let mut osm = fixture();
        // Both members exist and affect the driving graph, as in the real input.
        for (original, id) in [(10, 1196383222), (30, 220883467)] {
            let mut way = osm.ways.remove(&original).unwrap();
            way.id = id;
            osm.ways.insert(id, way);
        }
        osm.relations.push(relation(
            1,
            "no_right_turn",
            &[(1, 20, "from"), (1, 40, "to")],
        ));
        osm.relations.push(relation(
            5652903,
            "no_right_turn",
            &[(1, 1196383222, ""), (1, 220883467, "")],
        ));
        let staged = source(&parent, &osm);
        let output = parent.join("ready");
        let memory = crate::prepare(
            &osm,
            crate::BuildOptions {
                invalid_restrictions: InvalidRestrictionPolicy::BlockWays,
                ..Default::default()
            },
        )
        .unwrap_err();
        let disk = prepare_block_ways(&staged, &output, BlockWaysOptions::default()).unwrap_err();
        for error in [&memory, &disk] {
            let blocker = error.report.blockways_failure.as_ref().unwrap();
            assert_eq!(blocker.relation_id, 5652903);
            assert_eq!(blocker.reason, "NoSafeFromToMember");
            assert!(blocker.members.iter().all(|m| m.role.is_empty()));
            assert_eq!(error.report.planned_blocked_way_ids, vec![20, 40]);
            assert!(error.report.applied_blocked_way_ids.is_empty());
            assert!(!error.report.valid);
            assert_eq!(error.report.restriction_audit.attempt, 1);
        }
        assert_eq!(
            serde_json::to_value(&memory.report.blockways_failure).unwrap(),
            serde_json::to_value(&disk.report.blockways_failure).unwrap()
        );
        assert!(!output.exists());
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn no_driving_graph_after_blocking_preserves_audit_on_error() {
        let parent = temporary();
        let mut osm = fixture();
        osm.ways.retain(|id, _| *id == 10);
        osm.relations.push(relation(
            1,
            "no_right_turn",
            &[(1, 10, "from"), (1, 10, "to")],
        ));
        let staged = source(&parent, &osm);
        let output = parent.join("ready");
        let expected = crate::prepare(
            &osm,
            crate::BuildOptions {
                invalid_restrictions: InvalidRestrictionPolicy::BlockWays,
                ..Default::default()
            },
        )
        .unwrap_err();
        let actual = prepare_block_ways(&staged, &output, BlockWaysOptions::default()).unwrap_err();
        assert_eq!(actual.to_string(), expected.to_string());
        assert_eq!(
            actual.report.blocked_way_ids,
            expected.report.blocked_way_ids
        );
        assert_eq!(
            serde_json::to_value(actual.report.restriction_diagnostics).unwrap(),
            serde_json::to_value(expected.report.restriction_diagnostics).unwrap()
        );
        assert!(!output.exists());
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn budgets_abort_and_cleanup_owned_artifacts_without_publishing() {
        let parent = temporary();
        let osm = cascade();
        let staged = source(&parent, &osm);
        for (options, resource) in [
            (
                BlockWaysOptions {
                    max_blocked_ways: 0,
                    ..Default::default()
                },
                "blockways_blocked_ways",
            ),
            (
                BlockWaysOptions {
                    max_member_references: 0,
                    ..Default::default()
                },
                "blockways_member_references",
            ),
            (
                BlockWaysOptions {
                    restriction: RestrictionOptions {
                        stage: StageOptions {
                            scratch_bytes: 16,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    ..Default::default()
                },
                "blockways_scratch_bytes",
            ),
            (
                BlockWaysOptions {
                    restriction: RestrictionOptions {
                        stage: StageOptions {
                            run_files: 1,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    ..Default::default()
                },
                "blockways_files",
            ),
        ] {
            let output = parent.join("failed");
            let error = prepare_block_ways(&staged, &output, options).unwrap_err();
            assert!(error.to_string().contains(resource), "{error}");
            if matches!(
                resource,
                "blockways_blocked_ways" | "blockways_member_references"
            ) {
                assert!(error.report.nodes > 0);
                assert!(error.report.edges > 0);
                assert!(!error.report.diagnostics.is_empty());
                assert!(!error.report.restriction_diagnostics.is_empty());
            }
            assert!(!output.exists());
            assert!(!fs::read_dir(&parent).unwrap().any(|p| p
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".routeweft-blockways")));
        }
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn manifest_allowance_is_reserved_before_building_the_graph() {
        let parent = temporary();
        let mut osm = fixture();
        for id in 1..=70 {
            let mut rel = relation(id, "no_right_turn", &[(1, 20, "from"), (1, 40, "to")]);
            for tag in 0..30 {
                rel.tags
                    .insert(format!("restriction:extra{tag:02}"), "x".repeat(512));
            }
            osm.relations.push(rel);
        }
        let staged = source(&parent, &osm);
        let output = parent.join("ready");
        let options = BlockWaysOptions {
            restriction: RestrictionOptions {
                stage: StageOptions {
                    scratch_bytes: 1024 * 1024 + 100_000,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let error = prepare_block_ways(&staged, &output, options).unwrap_err();
        assert!(error.to_string().contains("blockways_scratch_bytes"));
        assert!(error.report.restriction_diagnostics.is_empty());
        assert!(!error.report.restriction_audit.scan_complete);
        assert!(error.report.blocked_way_ids.is_empty());
        assert!(!output.exists());
        assert!(!fs::read_dir(&parent).unwrap().any(|p| p
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".routeweft-blockways")));
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn retained_phase_files_reduce_the_next_phase_scratch_allowance() {
        let parent = temporary();
        let osm = fixture();
        let staged = source(&parent, &osm);
        let joined = parent.join("direct-join");
        let edges = parent.join("direct-edges");
        let adjacency = parent.join("direct-adjacency");
        let options = StageOptions::default();
        let connection = super::super::connect(&staged, &joined, options).unwrap();
        let edge = emit_edges(&staged, &joined, &edges, options).unwrap();
        let index = index_adjacency(&edges, &adjacency, options).unwrap();
        let restriction = resolve_restrictions(
            &staged,
            &joined,
            &edges,
            &adjacency,
            &parent.join("direct-restrictions"),
            RestrictionOptions::default(),
        )
        .unwrap();
        let single_peak = [
            connection.peak_scratch_bytes,
            edge.peak_scratch_bytes,
            index.peak_scratch_bytes,
            restriction.peak_scratch_bytes,
        ]
        .into_iter()
        .max()
        .unwrap();
        // Every independent payload phase fits this quota, but their retained outputs do not.
        let options = BlockWaysOptions {
            restriction: RestrictionOptions {
                stage: StageOptions {
                    scratch_bytes: 1024 * 1024 + single_peak,
                    ..options
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let output = parent.join("ready");
        let error = prepare_block_ways(&staged, &output, options).unwrap_err();
        assert!(error.to_string().contains("scratch_bytes"), "{error}");
        assert!(!output.exists());
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    #[ignore = "requires ROUTEWEFT_BLOCKWAYS_PBF pointing at a local fixture"]
    fn real_pbf_blockways_graph_and_audit_match_memory() {
        let input = std::path::PathBuf::from(std::env::var("ROUTEWEFT_BLOCKWAYS_PBF").unwrap());
        let parent = std::env::temp_dir().join(format!(
            "routeweft-blockways-local-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&parent).unwrap();
        let staged = parent.join("source");
        crate::staging::stage(&input, &staged, StageOptions::default()).unwrap();
        let output = parent.join("ready");
        let report = prepare_block_ways(&staged, &output, BlockWaysOptions::default()).unwrap();
        let osm = crate::pbf::read_for_driving(&input).unwrap();
        compare(&osm, &output, &report);
        println!("{}", serde_json::to_string(&report).unwrap());
        fs::remove_dir_all(parent).unwrap();
    }
}
