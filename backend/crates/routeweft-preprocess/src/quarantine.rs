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
