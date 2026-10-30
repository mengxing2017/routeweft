//! Explicit bounded loss of road coverage, never reconstruction of missing roles.
use crate::{pbf::Relation, InvalidRestrictionPolicy};
use std::collections::BTreeSet;

pub(crate) const MARKER: &str = "quarantine_ways_policy_v1";

/// Inspect the original complete relation, not the bounded diagnostic projection.
/// Absent ways cannot contribute graph edges; an unknown/nested member cannot be
/// bounded by the visible way set and must fail closed.
pub(crate) fn failure(
    policy: InvalidRestrictionPolicy,
    relation: &Relation,
    code: &str,
    present: &BTreeSet<i64>,
) -> Option<&'static str> {
    if relation.members.iter().any(|m| m.kind > 1) {
        return Some("UnboundedMemberDependency");
    }
    if policy == InvalidRestrictionPolicy::QuarantineWays {
        if !matches!(
            code,
            "MissingFromTo"
                | "MissingVia"
                | "UnsupportedMultiplicity"
                | "UnresolvedRestriction"
                | "MissingReference"
                | "UnsupportedViaShape"
        ) && !(code == "UnsupportedRestriction"
            && [
                "restriction:motorcar",
                "restriction:motor_vehicle",
                "restriction:vehicle",
                "restriction",
            ]
            .iter()
            .find_map(|key| relation.tags.get(*key))
            .is_some_and(|rule| rule == "no_right_turn_on_red"))
        {
            return Some("QuarantineCategoryNotApproved");
        }
        if !relation
            .members
            .iter()
            .any(|m| m.kind == 1 && present.contains(&m.id))
        {
            return Some("NoPresentWayMember");
        }
    } else if !relation
        .members
        .iter()
        .any(|m| m.kind == 1 && matches!(m.role.as_str(), "from" | "to") && present.contains(&m.id))
    {
        return Some(
            if relation
                .members
                .iter()
                .any(|m| m.kind == 1 && matches!(m.role.as_str(), "from" | "to"))
            {
                "ReferencedWayAbsent"
            } else {
                "NoSafeFromToMember"
            },
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pbf::{Member, Tags};

    #[test]
    fn policy_decisions_use_complete_typed_members_and_selected_rule() {
        let relation = |members: &[(u64, i64, &str)]| Relation {
            id: 5652903,
            tags: Tags::from([("restriction".into(), "no_right_turn".into())]),
            members: members
                .iter()
                .map(|&(kind, id, role)| Member {
                    kind,
                    id,
                    role: role.into(),
                })
                .collect(),
        };
        let present = BTreeSet::from([10, 20]);
        let quarantine = InvalidRestrictionPolicy::QuarantineWays;
        for members in [vec![], vec![(0, 10, "via")], vec![(1, 999, "from")]] {
            assert_eq!(
                failure(quarantine, &relation(&members), "MissingFromTo", &present),
                Some("NoPresentWayMember")
            );
        }
        for kind in [2, 3, u64::MAX] {
            assert_eq!(
                failure(
                    quarantine,
                    &relation(&[(1, 10, ""), (kind, 20, "")]),
                    "MissingFromTo",
                    &present
                ),
                Some("UnboundedMemberDependency")
            );
        }
        let rel = relation(&[(1, 10, ""), (1, 999, "")]);
        assert_eq!(failure(quarantine, &rel, "MissingFromTo", &present), None);
        assert_eq!(
            failure(
                InvalidRestrictionPolicy::BlockWays,
                &rel,
                "MissingFromTo",
                &present
            ),
            Some("NoSafeFromToMember")
        );
        assert_eq!(
            failure(
                InvalidRestrictionPolicy::BlockWays,
                &relation(&[(1, 999, "from"), (1, 10, "via")]),
                "MissingReference",
                &present
            ),
            Some("ReferencedWayAbsent")
        );
        for code in [
            "RestrictionLimitExceeded",
            "InvalidMemberType",
            "UnknownCategory",
        ] {
            assert_eq!(
                failure(quarantine, &rel, code, &present),
                Some("QuarantineCategoryNotApproved")
            );
        }
        let mut red = rel.clone();
        red.tags
            .insert("restriction".into(), "no_right_turn_on_red".into());
        assert_eq!(
            failure(quarantine, &red, "UnsupportedRestriction", &present),
            None
        );
        red.tags
            .insert("restriction:motorcar".into(), "unrecognized".into());
        assert_eq!(
            failure(quarantine, &red, "UnsupportedRestriction", &present),
            Some("QuarantineCategoryNotApproved")
        );
    }
}
