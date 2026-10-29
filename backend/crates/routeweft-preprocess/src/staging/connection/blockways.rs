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
    safe: Vec<i64>,
}
fn exclusions(
    source: &Source,
    diagnostics: &mut [RestrictionDiagnostic],
    excluded: &mut BTreeSet<i64>,
    options: BlockWaysOptions,
) -> Result<bool> {
    let lookup: BTreeMap<_, _> = diagnostics
        .iter()
        .enumerate()
        .map(|(i, d)| (d.relation_id, i))
        .collect();
    let mut members: Vec<Members> = (0..diagnostics.len()).map(|_| Members::default()).collect();
    let mut candidates = BTreeSet::new();
    let mut count = 0;
    let mut found = 0;
    source.visit(|record| {
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
                    if matches!(member.role.as_str(), "from" | "to") {
                        members[index].safe.push(member.id);
                    }
                }
            }
        }
        Ok(())
    })?;
    if found != diagnostics.len() {
        return Err(invalid("BlockWays relation identity").into());
    }
    let mut present = BTreeSet::new();
    source.visit(|record| {
        if let Record::Way(way) = record {
            if candidates.contains(&way.id) {
                present.insert(way.id);
            }
        }
        Ok(())
    })?;
    for (diagnostic, member) in diagnostics.iter_mut().zip(members) {
        if !member.safe.iter().any(|id| present.contains(id)) {
            return Ok(false);
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
    Ok(true)
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

pub(crate) fn prepare_with_policy(
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
    let mut audit = AuditCollector::new(options.restriction.audit, "external_restrictions")?;
    let mut attempts = 0;
    let mut speed_audit = Vec::new();
    let mut graph_nodes = 0;
    let mut graph_edges = 0;
    let mut graph_diagnostics = BTreeMap::new();
    let result = (|| {
        loop {
            graph_nodes = 0;
            graph_edges = 0;
            graph_diagnostics.clear();
            attempts += 1;
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
            let edge = emit_edges(staged, &joined, &edges, stage_options)?;
            graph_edges = edge.edges as usize;
            graph_diagnostics = edge.diagnostics.clone();
            owned.completed(retained, edge.peak_scratch_bytes)?;
            let (stage_options, retained) = owned.remaining()?;
            let index = index_adjacency(&edges, &adjacency, stage_options)?;
            owned.completed(retained, index.peak_scratch_bytes)?;
            let (stage_options, retained) = owned.remaining()?;
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
                    owned.completed(retained, resolved.peak_scratch_bytes)?;
                    verify_completed(&joined, &connection, &connection.files)?;
                    verify_completed(&edges, &edge, &edge.files)?;
                    verify_completed(&adjacency, &index, &index.files)?;
                    verify_completed(&restrictions, &resolved, &resolved.files)?;
                    let mut diagnostics = resolved.diagnostics;
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
                    if policy == InvalidRestrictionPolicy::Reject || !audit.can_block(&error) {
                        return Err(error);
                    }
                    if attempts >= options.restriction.audit.max_blockways_attempts as u64 {
                        error.source = audit.attempt_limit().source;
                        error.report.failure_code = Some("BlockWaysAttemptLimitExceeded".into());
                        return Err(error);
                    }
                    let before = excluded.len();
                    if !exclusions(&source, &mut audit.records[start..], &mut excluded, options)? {
                        return Err(error);
                    }
                    audit.refresh()?;
                    if before == excluded.len() {
                        return Err(invalid("restriction blocking made no progress").into());
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
        error.report.restriction_diagnostics = report.restriction_diagnostics.clone();
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
    fn unsafe_abort_keeps_exclusions_applied_to_earlier_diagnostics() {
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
        // The coordinator preserves partial exclusion context even on an unsafe abort.
        assert_eq!(actual.report.blocked_way_ids, vec![20, 40]);
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
    fn final_manifest_budget_failure_keeps_completed_blocking_audit() {
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
        assert_eq!(
            error.report.failure_code.as_deref(),
            Some("IntermediateWriteError")
        );
        assert!(error.to_string().contains("blockways_scratch_bytes"));
        assert_eq!(error.report.restriction_diagnostics.len(), 70);
        assert_eq!(error.report.blocked_way_ids, vec![20, 40]);
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
