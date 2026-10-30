use crate::pbf::Relation;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Isolate bounded malformed restrictions by default; strict rejection is explicit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvalidRestrictionPolicy {
    Reject,
    BlockWays,
    /// Version 1: quarantine approved malformed relations, including empty roles.
    /// May lose road coverage; never infer a turn or silently drop its restriction.
    #[default]
    QuarantineWays,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct BuildOptions {
    pub invalid_restrictions: InvalidRestrictionPolicy,
    pub invalid_maxspeed: InvalidMaxspeedPolicy,
    pub restriction_audit: crate::RestrictionAuditLimits,
}
/// Explicit opt-in to estimate costs from the profile when a speed tag is invalid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvalidMaxspeedPolicy {
    #[default]
    Reject,
    UseDefault,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MaxspeedDiagnostic {
    pub way_id: i64,
    pub field: String,
    pub value: String,
    #[serde(deserialize_with = "crate::audit::short_string")]
    pub action: String,
    pub effective_kmh: Option<f64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemberDiagnostic {
    #[serde(deserialize_with = "crate::audit::short_string")]
    pub kind: String,
    pub id: i64,
    #[serde(deserialize_with = "crate::audit::role_string")]
    pub role: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RestrictionDiagnostic {
    pub relation_id: i64,
    #[serde(default)]
    pub attempt: u32,
    /// Stable machine-readable category, independent of the human message.
    #[serde(deserialize_with = "crate::audit::short_string")]
    pub code: String,
    #[serde(deserialize_with = "crate::audit::message_string")]
    pub message: String,
    #[serde(deserialize_with = "crate::audit::tags")]
    pub tags: BTreeMap<String, String>,
    #[serde(deserialize_with = "crate::audit::members")]
    pub members: Vec<MemberDiagnostic>,
    pub members_truncated: bool,
    #[serde(deserialize_with = "crate::audit::short_string")]
    pub action: String,
}
impl RestrictionDiagnostic {
    pub(crate) fn new(rel: &Relation, code: &str, message: impl Into<String>) -> Self {
        Self {
            relation_id: rel.id,
            attempt: 0,
            code: code.into(),
            message: message.into(),
            tags: rel
                .tags
                .iter()
                .filter(|(k, _)| {
                    k.as_str() == "type" || k.as_str() == "except" || k.starts_with("restriction")
                })
                .take(32)
                .map(|(k, v)| (k.chars().take(128).collect(), v.chars().take(512).collect()))
                .collect(),
            members: rel
                .members
                .iter()
                .take(256)
                .map(|m| MemberDiagnostic {
                    kind: match m.kind {
                        0 => "node",
                        1 => "way",
                        2 => "relation",
                        _ => "unknown",
                    }
                    .into(),
                    id: m.id,
                    role: m.role.chars().take(64).collect(),
                })
                .collect(),
            members_truncated: rel.members.len() > 256,
            action: "rejected".into(),
        }
    }
}
/// Why conservative exclusion cannot safely continue. No roles are inferred.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockWaysFailure {
    pub relation_id: i64,
    pub attempt: u32,
    pub reason: String,
    pub recoverable: bool,
    pub members: Vec<MemberDiagnostic>,
}
impl BlockWaysFailure {
    pub(crate) fn new(d: &RestrictionDiagnostic, reason: &str) -> Self {
        Self {
            relation_id: d.relation_id,
            attempt: d.attempt,
            reason: reason.into(),
            recoverable: false,
            members: d.members.clone(),
        }
    }
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct InputReport {
    pub valid: bool,
    pub failure_code: Option<String>,
    pub policy: InvalidRestrictionPolicy,
    pub maxspeed_policy: InvalidMaxspeedPolicy,
    pub maxspeed_diagnostics: Vec<MaxspeedDiagnostic>,
    pub source_sha256: String,
    pub source_snapshot: Option<i64>,
    pub nodes: usize,
    pub edges: usize,
    pub restrictions: usize,
    pub diagnostics: BTreeMap<String, u64>,
    pub build_metrics: BTreeMap<String, u64>,
    pub restriction_diagnostics: Vec<RestrictionDiagnostic>,
    pub restriction_audit: crate::RestrictionAuditSummary,
    /// Legacy alias of planned_blocked_way_ids; does not imply publication.
    pub blocked_way_ids: Vec<i64>,
    pub planned_blocked_way_ids: Vec<i64>,
    /// Exclusions used in the last completed node/edge materialization.
    pub applied_blocked_way_ids: Vec<i64>,
    pub blockways_failure: Option<BlockWaysFailure>,
}
#[derive(Debug)]
pub struct Error {
    pub source: routeweft_dataset::Error,
    pub report: Box<InputReport>,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if matches!(self.source, routeweft_dataset::Error::ResourceLimit(_)) {
            return self.source.fmt(f);
        }
        if let Some(blocker) = &self.report.blockways_failure {
            return write!(
                f,
                "relation {}: {} (attempt {}, recoverable=false)",
                blocker.relation_id, blocker.reason, blocker.attempt
            );
        }
        if self
            .report
            .restriction_diagnostics
            .iter()
            .all(|d| d.action == "blocked_ways")
        {
            return self.source.fmt(f);
        }
        if let Some(d) = self
            .report
            .restriction_diagnostics
            .iter()
            .find(|d| d.action == "rejected")
        {
            write!(f, "relation {}: {}: {}", d.relation_id, d.code, d.message)
        } else {
            self.source.fmt(f)
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}
impl From<routeweft_dataset::Error> for Error {
    fn from(source: routeweft_dataset::Error) -> Self {
        Self {
            source,
            report: Box::default(),
        }
    }
}
impl From<std::io::Error> for Error {
    fn from(source: std::io::Error) -> Self {
        routeweft_dataset::Error::Io(source).into()
    }
}
pub type Result<T> = std::result::Result<T, Error>;
