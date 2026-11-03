//! Versioned MLD allocation accounting shared by planning and execution.
use crate::{
    acceleration::{self, CustomizationOptions},
    Error,
};
use routeweft_dataset::{LoadOptions, TopologyBounds};
use serde::Serialize;
use std::path::Path;

pub const ESTIMATOR_VERSION: u32 = 2;
const IO_BYTES: u64 = 131072;
fn add(a: u64, b: u64) -> Result<u64, Error> {
    a.checked_add(b).ok_or(Error::BudgetExceeded)
}
fn mul(a: u64, b: u64) -> Result<u64, Error> {
    a.checked_mul(b).ok_or(Error::BudgetExceeded)
}
fn sum(values: impl IntoIterator<Item = u64>) -> Result<u64, Error> {
    values.into_iter().try_fold(0, add)
}

/// Includes restriction trie/extras, vector capacity, both topology CSR arrays,
/// and the physical CSR alive during topology construction. Logical, not RSS.
pub(crate) fn topology_bytes(c: TopologyBounds) -> Result<u64, Error> {
    sum([
        mul(add(c.states, 1)?, 64)?,
        mul(c.transitions, 96)?,
        mul(add(c.restriction_references, 1)?, 256)?,
        mul(add(c.physical_nodes, 1)?, 16)?,
        mul(c.physical_edges, 16)?,
        IO_BYTES,
    ])
}

#[derive(Clone, Debug, Serialize)]
pub struct MldAllocation {
    pub topology_bytes: u64,
    pub forward_reverse_index_bytes: u64,
    pub resident_bytes: u64,
    pub partition_workspace_bytes: u64,
    pub search_workspace_bytes: u64,
    pub customization_peak_bytes: u64,
    pub load_peak_bytes: u64,
    pub output_bytes: u64,
}
pub(crate) fn allocation(
    c: TopologyBounds,
    cells: &[u64],
    shortcuts: u64,
    heap_entries: u64,
) -> Result<MldAllocation, Error> {
    let levels = cells.len() as u64;
    let total_cells = sum(cells.iter().copied())?;
    let topology_bytes = topology_bytes(c)?;
    let forward_reverse_index_bytes = sum([
        mul(add(c.states, 1)?, mul(levels, 8)?)?,
        mul(shortcuts, 64)?,
        mul(total_cells, 2)?,
        mul(c.states, 32)?,
        IO_BYTES,
    ])?;
    let resident_bytes = add(topology_bytes, forward_reverse_index_bytes)?;
    let partition_workspace_bytes = add(
        mul(add(c.physical_nodes, 1)?, 128)?,
        mul(c.physical_edges, 16)?,
    )?;
    let search_workspace_bytes = add(mul(add(c.states, 1)?, 64)?, mul(add(heap_entries, 1)?, 32)?)?;
    let customization_peak_bytes = sum([
        resident_bytes,
        partition_workspace_bytes.max(search_workspace_bytes),
        IO_BYTES,
    ])?;
    let load_peak_bytes = sum([resident_bytes, mul(add(c.states, 1)?, 16)?, IO_BYTES])?;
    let output_bytes = sum([
        116,
        mul(c.states, 4)?,
        mul(levels, 8)?,
        total_cells,
        mul(add(c.states, 1)?, mul(levels, 4)?)?,
        mul(shortcuts, 12)?,
    ])?;
    Ok(MldAllocation {
        topology_bytes,
        forward_reverse_index_bytes,
        resident_bytes,
        partition_workspace_bytes,
        search_workspace_bytes,
        customization_peak_bytes,
        load_peak_bytes,
        output_bytes,
    })
}
pub(crate) fn cells(c: TopologyBounds, options: CustomizationOptions) -> Vec<u64> {
    let leaf = c.physical_nodes.div_ceil(options.leaf_nodes as u64).max(1);
    (0..options.levels)
        .map(|l| (leaf - 1) / (options.fanout as u64).pow(l as u32) + 1)
        .collect()
}
pub(crate) fn check(observed: u64, limit: u64) -> Result<(), Error> {
    if observed > limit {
        Err(Error::BudgetExceeded)
    } else {
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct ArtifactPlan {
    pub format: &'static str,
    pub sha256: String,
    pub bytes: u64,
    pub states: u64,
    pub fanout: u32,
    pub cells: Vec<u64>,
    pub shortcuts: Vec<u64>,
    pub verification_scope: &'static str,
    pub customization_provenance: &'static str,
}
#[derive(Debug, Serialize)]
pub struct StagePlan {
    pub base_bytes: u64,
    pub accelerator: MldAllocation,
    pub peak_bytes: u64,
    pub resident_bytes: u64,
}
#[derive(Debug, Serialize)]
pub struct ResourcePlan {
    #[serde(flatten)]
    pub base: routeweft_dataset::ResourcePlan,
    pub base_estimator_version: u32,
    pub profile: String,
    pub customization: CustomizationOptions,
    pub topology_upper_bound: TopologyBounds,
    pub acceleration_present: bool,
    pub acceleration: Option<ArtifactPlan>,
    pub customize: StagePlan,
    pub accelerated_load: Option<StagePlan>,
    pub engine_resident_bytes: u64,
}

/// No Graph, automaton, query index or accelerator is constructed. Existing MLD
/// is streamed within workspace, validating checksum, layout and base binding;
/// metric optimality and restriction-state boundary semantics need Engine open.
pub fn resource_plan(
    path: &Path,
    limits: LoadOptions,
    workspace: u64,
    options: CustomizationOptions,
) -> Result<ResourcePlan, Error> {
    options.validate()?;
    let mut base = routeweft_dataset::resource_plan(path, limits, workspace)
        .map_err(crate::roads::dataset_error)?;
    let manifest =
        routeweft_dataset::inspect_manifest(path, limits).map_err(crate::roads::dataset_error)?;
    if base.dataset_id != manifest.dataset_id {
        return Err(Error::InvalidDataset);
    }
    let counts = routeweft_dataset::inspect_topology_bounds(path, limits, workspace)
        .map_err(crate::roads::dataset_error)?;
    let level_cells = cells(counts, options);
    let per_source = (options.max_boundary as u64)
        .saturating_sub(1)
        .min(counts.states.saturating_sub(1));
    let shortcuts = mul(mul(counts.states, per_source)?, options.levels as u64)?
        .min(options.max_shortcuts as u64);
    // A Dijkstra heap can contain duplicate relaxations but each settled state
    // scans each base/shortcut arc at most once for one customization source.
    let metric = allocation(
        counts,
        &level_cells,
        shortcuts,
        add(counts.transitions, shortcuts)?,
    )?;
    let engine_base = base.engine_load.estimated_bytes;
    let customize = StagePlan {
        base_bytes: engine_base,
        peak_bytes: add(engine_base, metric.customization_peak_bytes)?,
        resident_bytes: add(engine_base, metric.resident_bytes)?,
        accelerator: metric,
    };
    let acceleration = acceleration::inspect_artifact(path, &manifest, counts, workspace)?;
    let accelerated_load = if let Some(artifact) = &acceleration {
        let mut actual = counts;
        actual.states = artifact.states;
        let metric = allocation(
            actual,
            &artifact.cells,
            sum(artifact.shortcuts.iter().copied())?,
            0,
        )?;
        Some(StagePlan {
            base_bytes: engine_base,
            peak_bytes: add(engine_base, metric.load_peak_bytes)?,
            resident_bytes: add(engine_base, metric.resident_bytes)?,
            accelerator: metric,
        })
    } else {
        None
    };
    let engine_resident_bytes = accelerated_load
        .as_ref()
        .map_or(engine_base, |p| p.resident_bytes);
    base.schema = 2;
    base.estimator_version = ESTIMATOR_VERSION;
    base.excluded_unknown
        .retain(|v| *v != "optional_routing_mld");
    base.included.extend([
        "mld_topology_and_restriction_expansion",
        "mld_partition_workspace",
        "mld_forward_reverse_csr",
        "mld_deserialization_and_serialization",
        "mld_customization_heap",
    ]);
    Ok(ResourcePlan {
        base,
        base_estimator_version: routeweft_dataset::ESTIMATOR_VERSION,
        profile: manifest.profile,
        customization: options,
        topology_upper_bound: counts,
        acceleration_present: acceleration.is_some(),
        acceleration,
        customize,
        accelerated_load,
        engine_resident_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arithmetic_overflow_rejects_and_peak_phases_do_not_double_count_each_other() {
        let c = TopologyBounds {
            physical_nodes: 100,
            physical_edges: 200,
            restriction_references: 8,
            states: 104,
            transitions: 230,
        };
        let a = allocation(c, &[10, 2], 50, 100).unwrap();
        assert!(a.customization_peak_bytes >= a.resident_bytes + a.partition_workspace_bytes);
        assert!(a.customization_peak_bytes >= a.resident_bytes + a.search_workspace_bytes);
        assert!(a.load_peak_bytes > a.resident_bytes);
        assert!(allocation(c, &[10, 2], 51, 100).unwrap().resident_bytes > a.resident_bytes);
        assert!(allocation(
            TopologyBounds {
                states: u64::MAX,
                ..c
            },
            &[10],
            0,
            0
        )
        .is_err());
        assert!(allocation(c, &[u64::MAX, 1], 0, 0).is_err());
    }
}
