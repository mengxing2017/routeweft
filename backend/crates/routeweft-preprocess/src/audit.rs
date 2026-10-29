//! Shared bounded restriction audit. Logical charges are not process RSS limits.
use crate::{Error, InputReport, RestrictionDiagnostic, Result};
use routeweft_dataset::{invalid, resource_limit};
use serde::{Deserialize, Serialize};
use std::io::{self, Write};

pub const MAX_AUDIT_RECORDS: usize = 32_768;
pub const MAX_AUDIT_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_AUDIT_MEMORY_BYTES: u64 = 64 * 1024 * 1024;
pub const BLOCKWAYS_MANIFEST_MAX_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RestrictionAuditLimits {
    pub max_diagnostics: usize,
    pub max_audit_bytes: u64,
    pub max_blockways_attempts: u32,
}
impl Default for RestrictionAuditLimits {
    fn default() -> Self {
        Self {
            max_diagnostics: 4096,
            max_audit_bytes: MAX_AUDIT_BYTES,
            max_blockways_attempts: 64,
        }
    }
}
impl RestrictionAuditLimits {
    pub fn validate(self) -> routeweft_dataset::Result<()> {
        if self.max_diagnostics == 0
            || self.max_diagnostics > MAX_AUDIT_RECORDS
            || self.max_audit_bytes == 0
            || self.max_audit_bytes > MAX_AUDIT_BYTES
            || self.max_blockways_attempts == 0
            || self.max_blockways_attempts > 257
        {
            return Err(invalid("restriction audit limits out of range"));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RestrictionAuditSummary {
    pub records_observed: u64,
    pub records_retained: u64,
    pub retained_bytes: u64,
    pub retained_memory_bytes: u64,
    pub scan_complete: bool,
    pub truncated: bool,
    pub attempt: u32,
    pub attempts_completed: u32,
    pub limits: RestrictionAuditLimits,
}

/// Limits serialization without allocating another encoded copy.
pub(crate) struct BoundedWriter<W> {
    pub inner: W,
    pub written: u64,
    pub limit: u64,
}
impl<W: Write> Write for BoundedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let observed = self.written.saturating_add(bytes.len() as u64);
        if observed > self.limit {
            return Err(io::Error::other("bounded JSON write limit"));
        }
        self.inner.write_all(bytes)?;
        self.written = observed;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
fn encoded_bytes(value: &impl Serialize) -> routeweft_dataset::Result<u64> {
    let mut writer = BoundedWriter {
        inner: io::sink(),
        written: 0,
        limit: MAX_AUDIT_BYTES,
    };
    serde_json::to_writer(&mut writer, value).map_err(|_| invalid("audit record JSON size"))?;
    Ok(writer.written)
}
fn memory_bytes(d: &RestrictionDiagnostic) -> u64 {
    // Four slots cover minimum Vec capacity and geometric growth; node overhead is conservative.
    (4 * std::mem::size_of::<RestrictionDiagnostic>() + 256) as u64
        + d.code.capacity() as u64
        + d.message.capacity() as u64
        + d.action.capacity().max(12) as u64
        + d.members.capacity() as u64 * std::mem::size_of::<crate::MemberDiagnostic>() as u64
        + d.members
            .iter()
            .map(|m| (m.kind.capacity() + m.role.capacity()) as u64)
            .sum::<u64>()
        + d.tags
            .iter()
            .map(|(k, v)| (128 + k.capacity() + v.capacity()) as u64)
            .sum::<u64>()
}

pub(crate) struct AuditCollector {
    pub records: Vec<RestrictionDiagnostic>,
    pub summary: RestrictionAuditSummary,
    reserved_bytes: u64,
    stage: &'static str,
}
impl AuditCollector {
    pub fn new(limits: RestrictionAuditLimits, stage: &'static str) -> Result<Self> {
        limits.validate()?;
        Ok(Self {
            records: Vec::new(),
            summary: RestrictionAuditSummary {
                limits,
                ..Default::default()
            },
            reserved_bytes: 0,
            stage,
        })
    }
    pub fn begin(&mut self, attempt: u32) {
        self.summary.attempt = attempt;
        self.summary.scan_complete = false;
    }
    pub fn complete(&mut self) {
        self.summary.scan_complete = true;
        self.summary.attempts_completed = self.summary.attempt;
    }
    fn exceeded(&mut self, name: &str, limit: u64, observed: u64) -> Error {
        self.summary.truncated = true;
        let mut error: Error = resource_limit(name, limit, observed, self.stage).into();
        error.report.failure_code = Some("RestrictionAuditLimitExceeded".into());
        error
    }
    pub fn push(&mut self, mut d: RestrictionDiagnostic) -> Result<()> {
        d.attempt = self.summary.attempt;
        self.retain(d)
    }
    fn retain(&mut self, d: RestrictionDiagnostic) -> Result<()> {
        self.summary.records_observed += 1;
        let limits = self.summary.limits;
        if self.records.len() >= limits.max_diagnostics {
            return Err(self.exceeded(
                "restriction_audit_records",
                limits.max_diagnostics as u64,
                self.summary.records_observed,
            ));
        }
        let bytes = encoded_bytes(&d)?;
        // Reserve the possible transition from rejected to blocked_ways before admission.
        let reserved = bytes + 12_u64.saturating_sub(d.action.len() as u64);
        let observed = self.reserved_bytes.saturating_add(reserved);
        if observed > limits.max_audit_bytes {
            return Err(self.exceeded("restriction_audit_bytes", limits.max_audit_bytes, observed));
        }
        let memory = self
            .summary
            .retained_memory_bytes
            .saturating_add(memory_bytes(&d));
        if memory > MAX_AUDIT_MEMORY_BYTES {
            return Err(self.exceeded(
                "restriction_audit_memory_bytes",
                MAX_AUDIT_MEMORY_BYTES,
                memory,
            ));
        }
        self.records.try_reserve(1).map_err(|_| {
            self.exceeded(
                "restriction_audit_memory_bytes",
                MAX_AUDIT_MEMORY_BYTES,
                memory,
            )
        })?;
        self.records.push(d);
        self.reserved_bytes = observed;
        self.summary.records_retained += 1;
        self.summary.retained_bytes += bytes;
        self.summary.retained_memory_bytes = memory;
        Ok(())
    }
    pub fn refresh(&mut self) -> Result<()> {
        self.summary.retained_bytes = self
            .records
            .iter()
            .try_fold(0, |sum, d| Ok::<_, Error>(sum + encoded_bytes(d)?))?;
        Ok(())
    }
    pub fn attach(&mut self, report: &mut InputReport) {
        report.restriction_audit = self.summary.clone();
        report.restriction_diagnostics = std::mem::take(&mut self.records);
    }
    pub fn can_block(&self, error: &Error) -> bool {
        error.report.failure_code.as_deref() == Some("InvalidRestriction")
            && self.summary.scan_complete
            && !self.summary.truncated
    }
    pub fn attempt_limit(&self) -> Error {
        let mut error: Error = resource_limit(
            "blockways_attempts",
            self.summary.limits.max_blockways_attempts as u64,
            self.summary.attempt as u64 + 1,
            if self.stage == "external_restrictions" {
                "external_blockways"
            } else {
                self.stage
            },
        )
        .into();
        error.report.failure_code = Some("BlockWaysAttemptLimitExceeded".into());
        error
    }
}

/// Bound the array before reading another record. Individual record fields are bounded too.
pub(crate) fn deserialize_diagnostics<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> std::result::Result<Vec<RestrictionDiagnostic>, D::Error> {
    struct Visitor;
    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = Vec<RestrictionDiagnostic>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("bounded restriction diagnostics")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let limits = RestrictionAuditLimits {
                max_diagnostics: MAX_AUDIT_RECORDS,
                ..Default::default()
            };
            let mut audit = AuditCollector::new(limits, "publication_source")
                .map_err(serde::de::Error::custom)?;
            while let Some(d) = seq.next_element::<RestrictionDiagnostic>()? {
                audit.retain(d).map_err(serde::de::Error::custom)?;
            }
            Ok(audit.records)
        }
    }
    de.deserialize_seq(Visitor)
}

pub(crate) fn validate_audit(
    summary: &RestrictionAuditSummary,
    records: &[RestrictionDiagnostic],
) -> routeweft_dataset::Result<()> {
    summary.limits.validate()?;
    if !summary.scan_complete
        || summary.truncated
        || summary.attempt == 0
        || summary.attempt > summary.limits.max_blockways_attempts
        || summary.attempts_completed != summary.attempt
        || summary.records_observed != records.len() as u64
        || summary.records_retained != records.len() as u64
    {
        return Err(invalid("incomplete restriction audit"));
    }
    let mut bytes = 0;
    let mut memory = 0;
    let mut previous = (0, i64::MIN);
    for d in records {
        if d.attempt == 0
            || d.attempt > summary.attempt
            || (d.attempt, d.relation_id) <= previous
            || d.action != "blocked_ways"
        {
            return Err(invalid("restriction audit identity or action"));
        }
        previous = (d.attempt, d.relation_id);
        bytes += encoded_bytes(d)?;
        memory += memory_bytes(d);
    }
    if records.len() > summary.limits.max_diagnostics || bytes != summary.retained_bytes
        || bytes > summary.limits.max_audit_bytes || memory > MAX_AUDIT_MEMORY_BYTES
        // Allocator capacity can differ on deserialization; never trust the claimed charge.
        || summary.retained_memory_bytes > MAX_AUDIT_MEMORY_BYTES
    {
        return Err(invalid("restriction audit budget or summary mismatch"));
    }
    Ok(())
}

struct Text<const N: usize>(String);
impl<'de, const N: usize> Deserialize<'de> for Text<N> {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> std::result::Result<Self, D::Error> {
        struct V<const N: usize>;
        impl<const N: usize> serde::de::Visitor<'_> for V<N> {
            type Value = Text<N>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "string of at most {N} bytes")
            }
            fn visit_str<E: serde::de::Error>(
                self,
                value: &str,
            ) -> std::result::Result<Self::Value, E> {
                if value.len() > N {
                    return Err(E::custom("audit string limit"));
                }
                Ok(Text(value.to_owned()))
            }
        }
        de.deserialize_str(V::<N>)
    }
}
pub(crate) fn short_string<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> std::result::Result<String, D::Error> {
    Ok(Text::<256>::deserialize(de)?.0)
}
pub(crate) fn role_string<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> std::result::Result<String, D::Error> {
    Ok(Text::<256>::deserialize(de)?.0)
}
pub(crate) fn message_string<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> std::result::Result<String, D::Error> {
    Ok(Text::<4096>::deserialize(de)?.0)
}
pub(crate) fn tags<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> std::result::Result<std::collections::BTreeMap<String, String>, D::Error> {
    struct V;
    impl<'de> serde::de::Visitor<'de> for V {
        type Value = std::collections::BTreeMap<String, String>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("at most 32 bounded tags")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut tags = std::collections::BTreeMap::new();
            while let Some(key) = map.next_key::<Text<512>>()? {
                if tags.len() == 32 {
                    return Err(serde::de::Error::custom("audit tags limit"));
                }
                let value = map.next_value::<Text<2048>>()?;
                if tags.insert(key.0, value.0).is_some() {
                    return Err(serde::de::Error::custom("duplicate audit tag"));
                }
            }
            Ok(tags)
        }
    }
    de.deserialize_map(V)
}
pub(crate) fn members<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> std::result::Result<Vec<crate::MemberDiagnostic>, D::Error> {
    struct V;
    impl<'de> serde::de::Visitor<'de> for V {
        type Value = Vec<crate::MemberDiagnostic>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("at most 256 bounded members")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut members = Vec::new();
            while let Some(m) = seq.next_element()? {
                if members.len() == 256 {
                    return Err(serde::de::Error::custom("audit members limit"));
                }
                members.push(m);
            }
            Ok(members)
        }
    }
    de.deserialize_seq(V)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(id: i64) -> RestrictionDiagnostic {
        RestrictionDiagnostic::new(
            &crate::pbf::Relation {
                id,
                tags: Default::default(),
                members: Vec::new(),
            },
            "MissingVia",
            "missing via",
        )
    }
    #[test]
    fn count_boundary_preserves_observed_lower_bound_and_records() {
        let mut audit = AuditCollector::new(
            RestrictionAuditLimits {
                max_diagnostics: 256,
                ..Default::default()
            },
            "external_restrictions",
        )
        .unwrap();
        audit.begin(1);
        for id in 0..256 {
            audit.push(record(id)).unwrap();
        }
        let mut error = audit.push(record(256)).unwrap_err();
        audit.attach(&mut error.report);
        assert_eq!(error.report.restriction_diagnostics.len(), 256);
        let summary = &error.report.restriction_audit;
        assert_eq!(summary.records_observed, 257);
        assert_eq!(summary.records_retained, 256);
        assert!(summary.truncated);
        assert!(!summary.scan_complete);
        assert!(
            matches!(error.source, routeweft_dataset::Error::ResourceLimit(ref r) if r.resource == "restriction_audit_records" && r.limit == 256 && r.observed == 257)
        );
        assert!(error.to_string().contains("restriction_audit_records"));
    }
    #[test]
    fn byte_budget_and_blocked_action_reservation_are_bounded() {
        let mut audit = AuditCollector::new(
            RestrictionAuditLimits {
                max_audit_bytes: 1,
                ..Default::default()
            },
            "external_restrictions",
        )
        .unwrap();
        audit.begin(1);
        let error = audit.push(record(1)).unwrap_err();
        assert!(audit.records.is_empty());
        assert!(
            matches!(error.source, routeweft_dataset::Error::ResourceLimit(ref r) if r.resource == "restriction_audit_bytes")
        );
        let mut audit = AuditCollector::new(Default::default(), "external_restrictions").unwrap();
        audit.begin(1);
        audit.push(record(1)).unwrap();
        let bytes = audit.summary.retained_bytes;
        audit.records[0].action = "blocked_ways".into();
        audit.refresh().unwrap();
        assert_eq!(audit.summary.retained_bytes, bytes + 4);
        audit.complete();
        validate_audit(&audit.summary, &audit.records).unwrap();
    }
    #[test]
    fn logical_memory_budget_is_enforced_before_retaining() {
        let mut audit = AuditCollector::new(Default::default(), "external_restrictions").unwrap();
        audit.begin(1);
        let mut d = record(1);
        // Capacity, rather than serialized length, is charged.
        d.members = Vec::with_capacity(
            MAX_AUDIT_MEMORY_BYTES as usize / std::mem::size_of::<crate::MemberDiagnostic>() + 1,
        );
        let error = audit.push(d).unwrap_err();
        assert!(
            matches!(error.source, routeweft_dataset::Error::ResourceLimit(ref r) if r.resource == "restriction_audit_memory_bytes")
        );
        assert!(audit.records.is_empty());
    }
    #[test]
    fn malformed_audit_fields_and_forged_summary_are_rejected() {
        let mut audit = AuditCollector::new(Default::default(), "external_restrictions").unwrap();
        audit.begin(1);
        audit.push(record(1)).unwrap();
        audit.records[0].action = "blocked_ways".into();
        audit.refresh().unwrap();
        audit.complete();
        let mut bad = audit.summary.clone();
        bad.records_retained += 1;
        assert!(validate_audit(&bad, &audit.records).is_err());
        let mut value = serde_json::to_value(&audit.records[0]).unwrap();
        value["members"] = serde_json::json!(vec![
            serde_json::json!({"kind":"way","id":1,"role":"from"});
            257
        ]);
        assert!(serde_json::from_value::<RestrictionDiagnostic>(value).is_err());
        let mut value = serde_json::to_value(&audit.records[0]).unwrap();
        value["message"] = "x".repeat(4097).into();
        assert!(serde_json::from_value::<RestrictionDiagnostic>(value).is_err());
    }
    #[test]
    fn array_reader_rejects_more_than_hard_limit_without_trusting_summary() {
        #[derive(Deserialize)]
        struct Report {
            #[serde(deserialize_with = "deserialize_diagnostics")]
            records: Vec<RestrictionDiagnostic>,
        }
        let empty = serde_json::to_string(&record(1)).unwrap();
        let json = format!(
            "{{\"records\":[{}]}}",
            vec![empty; MAX_AUDIT_RECORDS + 1].join(",")
        );
        assert!(serde_json::from_str::<Report>(&json).is_err());
        assert!(serde_json::from_str::<Report>("{\"records\":[]}")
            .unwrap()
            .records
            .is_empty());
    }
}
