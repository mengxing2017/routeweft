//! Versioned logical allocation estimates shared by admission and execution.
use crate::{invalid, Error, LoadOptions, Manifest, Result};
use serde::Serialize;

pub const ESTIMATOR_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct AllocationEstimate {
    pub estimated_bytes: u64,
    pub nodes_bytes: u64,
    pub edges_bytes: u64,
    pub restriction_bytes: u64,
    pub strings_bytes: u64,
    pub dictionary_bytes: u64,
}

/// Pure estimate, independent of the selected resident allocation budget.
/// V1 visitors use legacy ceilings, not untrusted manifest counts.
pub fn estimate_resident(
    m: &Manifest,
    limits: LoadOptions,
    indexes: bool,
) -> Result<AllocationEstimate> {
    if !matches!(m.format, 1 | 2) {
        return Err(Error::IncompatibleFormat);
    }
    let legacy = LoadOptions::default();
    let (nodes, edges, refs) = if m.format == 1 {
        (
            legacy.max_nodes as u64,
            legacy.max_edges as u64,
            legacy.max_restriction_edges as u64,
        )
    } else {
        (
            m.nodes as u64,
            m.edges as u64,
            (m.restrictions as u64)
                .saturating_mul(256)
                .min(limits.max_restriction_edges as u64),
        )
    };
    let nodes_bytes = nodes
        .checked_mul(if indexes { 128 } else { 32 })
        .ok_or(Error::LimitExceeded)?;
    let edges_bytes = edges
        .checked_mul(if indexes { 576 } else { 256 })
        .ok_or(Error::LimitExceeded)?;
    let restriction_bytes = refs
        .checked_mul(if indexes { 320 } else { 64 })
        .ok_or(Error::LimitExceeded)?;
    let strings_bytes = edges.saturating_mul(4160).min(256 * 1024 * 1024);
    let dictionary_bytes = edges
        .saturating_mul(2 * (4096 + 4 + 64))
        .min(256 * 1024 * 1024);
    let estimated_bytes = [
        nodes_bytes,
        edges_bytes,
        restriction_bytes,
        strings_bytes,
        dictionary_bytes,
    ]
    .into_iter()
    .try_fold(0u64, |sum, n| {
        sum.checked_add(n).ok_or(Error::LimitExceeded)
    })?;
    Ok(AllocationEstimate {
        estimated_bytes,
        nodes_bytes,
        edges_bytes,
        restriction_bytes,
        strings_bytes,
        dictionary_bytes,
    })
}

#[derive(Clone, Debug, Serialize)]
pub struct ResourcePlan {
    pub schema: u32,
    pub estimator_version: u32,
    pub estimate_kind: &'static str,
    pub dataset_id: String,
    pub graph_sha256: String,
    pub source_sha256: String,
    pub format: u32,
    pub counts: PlanCounts,
    pub effective_load_options: LoadOptions,
    pub dataset_load: AllocationEstimate,
    pub engine_load: AllocationEstimate,
    pub included: Vec<&'static str>,
    pub excluded_unknown: Vec<&'static str>,
    pub validation_workspace_budget_bytes: Option<u64>,
    pub verification_scope: &'static str,
}
#[derive(Clone, Debug, Serialize)]
pub struct PlanCounts {
    pub nodes: usize,
    pub edges: usize,
    pub restrictions: usize,
    pub graph_bytes: u64,
}

/// Use only with an authenticated manifest (e.g. a just-published build).
pub fn resource_plan_from_manifest(m: &Manifest, limits: LoadOptions) -> Result<ResourcePlan> {
    Ok(ResourcePlan {
        schema: 1,
        estimator_version: ESTIMATOR_VERSION,
        estimate_kind: "conservative_logical_allocations_not_rss",
        dataset_id: m.dataset_id.clone(),
        graph_sha256: m.graph_sha256.clone(),
        source_sha256: m.source_sha256.clone(),
        format: m.format,
        counts: PlanCounts {
            nodes: m.nodes,
            edges: m.edges,
            restrictions: m.restrictions,
            graph_bytes: m.graph_bytes,
        },
        effective_load_options: limits,
        dataset_load: estimate_resident(m, limits, false)?,
        engine_load: estimate_resident(m, limits, true)?,
        included: vec![
            "graph_vectors_with_spare_capacity",
            "strings_and_dictionary",
            "allocator_allowance",
            "csr",
            "rtree_construction",
            "turn_trie",
        ],
        excluded_unknown: vec![
            "optional_routing_mld",
            "query_workspace",
            "coverage_workspace",
            "audit_and_output_buffers",
            "process_runtime_and_os_overhead",
            "build_and_scratch_peak",
        ],
        validation_workspace_budget_bytes: None,
        verification_scope: "published_manifest",
    })
}

/// No resident graph or query index is constructed. V2 validates all graph
/// semantics using the bounded streaming verifier; V1 authenticates bytes only.
/// Inputs must remain immutable throughout planning and subsequent execution.
pub fn resource_plan(
    path: &std::path::Path,
    limits: LoadOptions,
    workspace_bytes: u64,
) -> Result<ResourcePlan> {
    let m = crate::inspect_manifest(path, limits)?;
    let mut plan = resource_plan_from_manifest(&m, limits)?;
    if m.format == 2 {
        let verified = crate::validate_binary_streaming(path, limits, workspace_bytes)?;
        if verified != m {
            return Err(invalid("dataset changed during resource planning"));
        }
        plan.validation_workspace_budget_bytes = Some(workspace_bytes);
        plan.verification_scope = "full_streaming_v2";
    } else {
        use sha2::{Digest, Sha256};
        use std::io::{BufReader, Read};
        let file = crate::controlled_file(path, &m.graph_file, limits.max_bytes)?;
        if file.metadata()?.len() != m.graph_bytes {
            return Err(invalid("integrity"));
        }
        let mut reader = BufReader::new(file).take(m.graph_bytes.saturating_add(1));
        let mut digest = Sha256::new();
        let mut buffer = [0u8; 65536];
        let mut bytes = 0u64;
        loop {
            let n = reader.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            bytes += n as u64;
            digest.update(&buffer[..n]);
        }
        if bytes != m.graph_bytes || format!("{:x}", digest.finalize()) != m.graph_sha256 {
            return Err(invalid("integrity"));
        }
        plan.validation_workspace_budget_bytes = Some(131072);
        plan.verification_scope = "identity_size_hash_v1_not_semantic";
    }
    Ok(plan)
}
