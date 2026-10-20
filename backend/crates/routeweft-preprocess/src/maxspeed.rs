use crate::{pbf::Way, InvalidMaxspeedPolicy, MaxspeedDiagnostic, Result};
use std::collections::BTreeMap;

pub(crate) fn resolve(
    way: &Way,
    field: &str,
    fallback: f64,
    policy: InvalidMaxspeedPolicy,
    audit: &mut Vec<MaxspeedDiagnostic>,
    stats: &mut BTreeMap<String, u64>,
) -> Result<f64> {
    let value = crate::value(&way.tags, field);
    match crate::speed(value, fallback) {
        Ok(speed) => Ok(speed),
        Err(reason) => {
            // Keep complete source values, with a bounded audit instead of silent
            // truncation. The bound includes a conservative per-entry allowance.
            let bytes = audit
                .iter()
                .map(|d| d.field.len() + d.value.len() + 128)
                .sum::<usize>()
                + field.len()
                + value.len()
                + 128;
            if audit.len() >= 256 {
                return Err(routeweft_dataset::resource_limit(
                    "maxspeed_audit_entries",
                    256,
                    audit.len() as u64 + 1,
                    "profile",
                )
                .into());
            }
            if bytes > 64 * 1024 {
                return Err(routeweft_dataset::resource_limit(
                    "maxspeed_audit_bytes",
                    64 * 1024,
                    bytes as u64,
                    "profile",
                )
                .into());
            }
            let use_default = policy == InvalidMaxspeedPolicy::UseDefault;
            audit.push(MaxspeedDiagnostic {
                way_id: way.id,
                field: field.into(),
                value: value.into(),
                action: if use_default {
                    "use_default"
                } else {
                    "rejected"
                }
                .into(),
                effective_kmh: use_default.then_some(fallback),
            });
            if use_default {
                crate::count(stats, "maxspeed_fallback_values");
                Ok(fallback)
            } else {
                let mut error =
                    crate::invalid(format!("way {}: {field}={value}: {reason}", way.id));
                error.report.failure_code = Some("InvalidMaxspeed".into());
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pbf::Tags;

    #[test]
    fn opt_in_fallback_keeps_directional_limits_and_audits_exact_source_values() {
        for invalid in ["辅路", "signals", "NaN", "inf", "-1", "0", "301", "30;50"] {
            let way = Way {
                id: 1533487270,
                refs: vec![1, 2],
                tags: Tags::from([
                    ("highway".into(), "residential".into()),
                    ("maxspeed".into(), "20".into()),
                    ("maxspeed:forward".into(), invalid.into()),
                    ("maxspeed:backward".into(), "10 mph".into()),
                ]),
            };
            let mut audit = Vec::new();
            let error = crate::select_way(
                &way,
                false,
                &mut BTreeMap::new(),
                InvalidMaxspeedPolicy::Reject,
                &mut audit,
            )
            .err()
            .unwrap();
            assert_eq!(
                error.report.failure_code.as_deref(),
                Some("InvalidMaxspeed")
            );
            assert_eq!(audit[0].value, invalid);
            assert_eq!(audit[0].effective_kmh, None);
            audit.clear();
            let mut stats = BTreeMap::new();
            let selection = crate::select_way(
                &way,
                false,
                &mut stats,
                InvalidMaxspeedPolicy::UseDefault,
                &mut audit,
            )
            .unwrap()
            .unwrap();
            assert_eq!(selection.forward, 20.0);
            assert!((selection.backward - 16.09344).abs() < 1e-8);
            assert_eq!(audit[0].way_id, way.id);
            assert_eq!(audit[0].field, "maxspeed:forward");
            assert_eq!(audit[0].value, invalid);
            assert_eq!(audit[0].effective_kmh, Some(20.0));
            assert_eq!(stats["maxspeed_fallback_values"], 1);
            audit.clear();
            assert!(crate::select_way(
                &way,
                true,
                &mut stats,
                InvalidMaxspeedPolicy::UseDefault,
                &mut audit
            )
            .unwrap()
            .is_none());
            assert!(audit.is_empty());
        }
    }

    #[test]
    fn valid_speeds_and_absent_values_do_not_create_fallbacks() {
        for (value, expected) in [
            ("", 30.),
            (" 20 km/h ", 20.),
            ("10 mph", 16.09344),
            ("50", 30.),
        ] {
            let way = Way {
                id: 1,
                refs: vec![],
                tags: Tags::from([("maxspeed".into(), value.into())]),
            };
            let mut audit = Vec::new();
            assert!(
                (resolve(
                    &way,
                    "maxspeed",
                    30.,
                    InvalidMaxspeedPolicy::UseDefault,
                    &mut audit,
                    &mut BTreeMap::new()
                )
                .unwrap()
                    - expected)
                    .abs()
                    < 1e-8
            );
            assert!(audit.is_empty());
        }
    }

    #[test]
    fn audit_limits_fail_without_silently_dropping_warnings() {
        let mut way = Way {
            id: 1,
            refs: vec![],
            tags: Tags::from([("maxspeed".into(), "辅路".into())]),
        };
        let mut audit = Vec::new();
        for _ in 0..256 {
            resolve(
                &way,
                "maxspeed",
                30.,
                InvalidMaxspeedPolicy::UseDefault,
                &mut audit,
                &mut BTreeMap::new(),
            )
            .unwrap();
        }
        let error = resolve(
            &way,
            "maxspeed",
            30.,
            InvalidMaxspeedPolicy::UseDefault,
            &mut audit,
            &mut BTreeMap::new(),
        )
        .unwrap_err();
        assert!(
            matches!(error.source, routeweft_dataset::Error::ResourceLimit(ref r) if r.resource == "maxspeed_audit_entries" && r.observed == 257)
        );
        audit.clear();
        way.tags.insert("maxspeed".into(), "x".repeat(65536));
        let error = resolve(
            &way,
            "maxspeed",
            30.,
            InvalidMaxspeedPolicy::UseDefault,
            &mut audit,
            &mut BTreeMap::new(),
        )
        .unwrap_err();
        assert!(
            matches!(error.source, routeweft_dataset::Error::ResourceLimit(ref r) if r.resource == "maxspeed_audit_bytes")
        );
        assert!(audit.is_empty());
    }
}
