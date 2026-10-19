use crate::pbf::Relation;
use serde::Serialize;
use std::collections::BTreeMap;

/// Explicit opt-in: never infer a missing turn location or ignore a restriction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InvalidRestrictionPolicy {
    #[default]
    Reject,
    BlockWays,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct BuildOptions {
    pub invalid_restrictions: InvalidRestrictionPolicy,
}
#[derive(Clone, Debug, Serialize)]
pub struct MemberDiagnostic {
    pub kind: String,
    pub id: i64,
    pub role: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct RestrictionDiagnostic {
    pub relation_id: i64,
    /// Stable machine-readable category, independent of the human message.
    pub code: String,
    pub message: String,
    pub tags: BTreeMap<String, String>,
    pub members: Vec<MemberDiagnostic>,
    pub members_truncated: bool,
    pub action: String,
}
impl RestrictionDiagnostic {
    pub(crate) fn new(rel: &Relation, code: &str, message: impl Into<String>) -> Self {
        Self {
            relation_id: rel.id,
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
#[derive(Clone, Debug, Default, Serialize)]
pub struct InputReport {
    pub valid: bool,
    pub failure_code: Option<String>,
    pub policy: InvalidRestrictionPolicy,
    pub source_sha256: String,
    pub source_snapshot: Option<i64>,
    pub nodes: usize,
    pub edges: usize,
    pub restrictions: usize,
    pub diagnostics: BTreeMap<String, u64>,
    pub restriction_diagnostics: Vec<RestrictionDiagnostic>,
    pub blocked_way_ids: Vec<i64>,
}
#[derive(Debug)]
pub struct Error {
    pub source: routeweft_dataset::Error,
    pub report: Box<InputReport>,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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
