//! Multi-level metrics on a sparse restriction-state graph.
//! Original edges remain authoritative for snapping and geometry recovery.
mod partition;
mod storage;
use crate::topology::Topology;

use crate::diagnostics::Context;
use crate::{Error, SearchBudget};
use routeweft_dataset::{Adjacency, Graph, LoadOptions, Manifest};
use serde::Serialize;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

pub const FILE_NAME: &str = "routing.mld";
const MAX_LEVELS: usize = 4;

#[derive(Clone, Copy)]
pub struct CustomizationOptions {
    pub leaf_nodes: usize,
    pub fanout: usize,
    pub levels: usize,
    pub max_boundary: usize,
    pub max_shortcuts: usize,
    pub max_work: usize,
    /// Bounds logical accelerator/workspace allocations, not process RSS.
    pub max_bytes: u64,
    pub deadline: Option<Instant>,
}
impl Default for CustomizationOptions {
    fn default() -> Self {
        Self {
            leaf_nodes: 256,
            fanout: 8,
            levels: 3,
            max_boundary: 128,
            max_shortcuts: 20_000_000,
            max_work: 2_000_000_000,
            max_bytes: 2 * 1024 * 1024 * 1024,
            deadline: None,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AccelerationInfo {
    pub algorithm: &'static str,
    pub levels: usize,
    pub cells: Vec<usize>,
    pub customized_cells: Vec<usize>,
    pub shortcuts: Vec<usize>,
    pub estimated_bytes: u64,
    pub restriction_states: usize,
    pub restriction_transitions: usize,
    pub boundary_nodes: Vec<usize>,
    pub partition: &'static str,
}

#[derive(Clone, Copy, Debug)]
struct Shortcut {
    to: u32,
    cost: u64,
}
struct Level {
    offsets: Vec<u32>,
    arcs: Vec<Shortcut>,
    usable: Vec<bool>,
}
pub(crate) struct Index {
    leaf: Vec<u32>,
    fanout: u32,
    boundary: Vec<u8>,
    components: Vec<u32>,
    topology: Topology,
    reverse: Vec<Level>,
    levels: Vec<Level>,
}
#[derive(Clone, Copy)]
pub(crate) struct Link {
    pub to: usize,
    pub cost: u64,
    pub edge: usize,
    pub level: Option<usize>,
}

fn root(parents: &mut [u32], mut node: usize) -> usize {
    while parents[node] as usize != node {
        parents[node] = parents[parents[node] as usize];
        node = parents[node] as usize;
    }
    node
}

impl Index {
    pub(crate) fn cell(&self, level: usize, node: usize) -> usize {
        (self.leaf[node] / self.fanout.pow(level as u32)) as usize
    }
    pub(crate) fn query_level(&self, node: usize, open: &[HashSet<usize>]) -> Option<usize> {
        (0..self.levels.len()).rev().find(|&level| {
            let cell = self.cell(level, node);
            self.boundary[node] as usize > level
                && self.levels[level].usable[cell]
                && !open[level].contains(&cell)
        })
    }
    fn lower_level(&self, node: usize, below: usize) -> Option<usize> {
        (0..below).rev().find(|&level| {
            self.boundary[node] as usize > level
                && self.levels[level].usable[self.cell(level, node)]
        })
    }
    pub(crate) fn open_cells(&self, nodes: impl Iterator<Item = usize>) -> Vec<HashSet<usize>> {
        let mut result = vec![HashSet::new(); self.levels.len()];
        let physical: HashSet<_> = nodes.collect();
        let nodes = physical.iter().copied().chain(
            self.topology
                .extras
                .iter()
                .flatten()
                .copied()
                .filter(|&id| physical.contains(&self.state(id).0)),
        );
        for node in nodes {
            for (level, cells) in result.iter_mut().enumerate() {
                cells.insert(self.cell(level, node));
            }
        }
        result
    }
    pub(crate) fn active(&self, node: usize, open: &[HashSet<usize>]) -> bool {
        (0..self.levels.len())
            .rev()
            .find(|&l| {
                self.levels[l].usable[self.cell(l, node)] && !open[l].contains(&self.cell(l, node))
            })
            .is_none_or(|l| self.boundary[node] as usize > l)
    }
    pub(crate) fn node(&self, vertex: usize, automaton: usize) -> Option<usize> {
        self.topology.node(vertex, automaton)
    }
    pub(crate) fn is_boundary(&self, vertex: usize) -> bool {
        self.boundary[vertex] > 0
    }
    pub(crate) fn state(&self, node: usize) -> (usize, usize) {
        self.topology.states[node]
    }
    pub(crate) fn original(&self, edge: usize) -> usize {
        self.topology.edges[edge].original
    }
    /// A clique need not be re-entered after arriving through that SAME clique.
    pub(crate) fn visit(
        &self,
        node: usize,
        level: Option<usize>,
        reverse: bool,
        skip_clique: bool,
        mut visit: impl FnMut(Link) -> Result<(), Error>,
    ) -> Result<(), Error> {
        if let Some(level) = level.filter(|_| !skip_clique) {
            let metric = if reverse {
                &self.reverse[level]
            } else {
                &self.levels[level]
            };
            for arc in
                &metric.arcs[metric.offsets[node] as usize..metric.offsets[node + 1] as usize]
            {
                visit(Link {
                    to: arc.to as usize,
                    cost: arc.cost,
                    edge: 0,
                    level: Some(level),
                })?;
            }
        }
        let ids = if reverse {
            &self.topology.incoming[node]
        } else {
            &self.topology.outgoing[node]
        };
        for &id in ids {
            let e = self.topology.edges[id];
            let to = if reverse { e.from } else { e.to };
            if level.is_none_or(|l| self.cell(l, node) != self.cell(l, to)) {
                visit(Link {
                    to,
                    cost: e.cost,
                    edge: id,
                    level: None,
                })?;
            }
        }
        Ok(())
    }
    fn rebuild_reverse(&mut self) {
        self.reverse = self
            .levels
            .iter()
            .map(|l| {
                let mut offsets = vec![0_u32; self.leaf.len() + 1];
                for a in &l.arcs {
                    offsets[a.to as usize + 1] += 1;
                }
                for i in 0..self.leaf.len() {
                    offsets[i + 1] += offsets[i];
                }
                let mut arcs = vec![Shortcut { to: 0, cost: 0 }; l.arcs.len()];
                for from in 0..self.leaf.len() {
                    for a in &l.arcs[l.offsets[from] as usize..l.offsets[from + 1] as usize] {
                        arcs[offsets[a.to as usize] as usize] = Shortcut {
                            to: from as u32,
                            cost: a.cost,
                        };
                        offsets[a.to as usize] += 1;
                    }
                }
                offsets.copy_within(..self.leaf.len(), 1);
                offsets[0] = 0;
                Level {
                    offsets,
                    arcs,
                    usable: Vec::new(),
                }
            })
            .collect();
    }
    pub fn info(&self) -> AccelerationInfo {
        AccelerationInfo {
            algorithm: "mld",
            restriction_states: self.topology.states.len(),
            restriction_transitions: self.topology.edges.len(),
            boundary_nodes: (0..self.levels.len())
                .map(|l| self.boundary.iter().filter(|&&b| b as usize > l).count())
                .collect(),
            partition: "recursive-balanced-graph-cut",
            levels: self.levels.len(),
            cells: self.levels.iter().map(|l| l.usable.len()).collect(),
            customized_cells: self
                .levels
                .iter()
                .map(|l| l.usable.iter().filter(|&&v| v).count())
                .collect(),
            shortcuts: self.levels.iter().map(|l| l.arcs.len()).collect(),
            estimated_bytes: self.estimated_bytes(),
        }
    }
    fn estimated_bytes(&self) -> u64 {
        (self.leaf.len() as u64 * 9)
            + self.topology.bytes()
            + self
                .levels
                .iter()
                .map(|l| {
                    l.offsets.len() as u64 * 8 + l.arcs.len() as u64 * 32 + l.usable.len() as u64
                })
                .sum::<u64>()
    }
    pub(crate) fn build(
        graph: &Graph,
        _outgoing: &Adjacency,
        options: CustomizationOptions,
    ) -> Result<Self, Error> {
        let original_n = graph.nodes.len();
        let n = original_n;
        if n == 0
            || n > u32::MAX as usize
            || options.leaf_nodes < 2
            || !(2..=32).contains(&options.fanout)
            || !(1..=MAX_LEVELS).contains(&options.levels)
            || options.max_boundary == 0
            || options.max_boundary > 1024
            || options.max_shortcuts > u32::MAX as usize
        {
            return Err(Error::InvalidInput);
        }
        // Includes sorting keys, dense distances, touched nodes, heap, parents,
        // all level offsets and conservative allocator/vector growth overhead.
        let workspace = n as u64 * (128 + options.levels as u64 * 8) + graph.edges.len() as u64;
        if workspace > options.max_bytes {
            return Err(Error::BudgetExceeded);
        }
        let budget = SearchBudget {
            max_work: options.max_work,
            deadline: options.deadline,
            ..Default::default()
        };
        let mut ctx = Context::new(&budget, true);
        let topology =
            Topology::build(graph, &mut ctx, options.max_bytes.saturating_sub(workspace))?;
        let n = topology.states.len();
        if n > u32::MAX as usize {
            return Err(Error::BudgetExceeded);
        }
        let workspace = workspace + topology.bytes() + (n - original_n) as u64 * 160;
        if workspace > options.max_bytes {
            return Err(Error::BudgetExceeded);
        }
        let leaf = partition::partition(graph, &topology, options.leaf_nodes, &mut ctx)?;
        let mut index = Self {
            leaf,
            fanout: options.fanout as u32,
            boundary: vec![0; n],
            components: (0..n as u32).collect(),
            topology,
            levels: Vec::new(),
            reverse: Vec::new(),
        };
        for e in &index.topology.edges {
            ctx.tick()?;
            let a = root(&mut index.components, e.from);
            let b = root(&mut index.components, e.to);
            index.components[a] = b as u32;
            for level in 0..options.levels {
                if index.cell(level, e.from) != index.cell(level, e.to) {
                    index.boundary[e.from] = index.boundary[e.from].max(level as u8 + 1);
                    index.boundary[e.to] = index.boundary[e.to].max(level as u8 + 1);
                }
            }
        }
        for node in 0..n {
            index.components[node] = root(&mut index.components, node) as u32;
        }
        let mut distances = vec![u64::MAX; n];
        // u64::MAX is a valid edge/path cost, not an unreachable sentinel.
        let mut reached = vec![false; n];
        let mut touched = Vec::new();
        let mut heap = BinaryHeap::new();
        let mut total_shortcuts = 0;
        for level in 0..options.levels {
            let cells = (*index.leaf.iter().max().unwrap() as usize
                / options.fanout.pow(level as u32)) as usize
                + 1;
            let mut counts = vec![0; cells];
            for node in 0..n {
                if index.boundary[node] as usize > level {
                    counts[index.cell(level, node)] += 1;
                }
            }
            let mut metric = Level {
                offsets: Vec::with_capacity(n + 1),
                arcs: Vec::new(),
                usable: counts
                    .iter()
                    .map(|&c| c > 0 && c <= options.max_boundary)
                    .collect(),
            };
            for source in 0..n {
                ctx.tick()?;
                metric.offsets.push(metric.arcs.len() as u32);
                let cell = index.cell(level, source);
                if index.boundary[source] as usize <= level || !metric.usable[cell] {
                    continue;
                }
                distances[source] = 0;
                reached[source] = true;
                touched.push(source);
                heap.push(Reverse((0_u64, source)));
                while let Some(Reverse((cost, node))) = heap.pop() {
                    ctx.tick()?;
                    if distances[node] != cost {
                        continue;
                    }
                    if node != source && index.boundary[node] as usize > level {
                        total_shortcuts += 1;
                        if total_shortcuts > options.max_shortcuts
                            || workspace + total_shortcuts as u64 * 32 > options.max_bytes
                        {
                            return Err(Error::BudgetExceeded);
                        }
                        metric.arcs.push(Shortcut {
                            to: node as u32,
                            cost,
                        });
                    }
                    index.visit(node, index.lower_level(node, level), false, false, |arc| {
                        ctx.tick()?;
                        if index.cell(level, arc.to) != cell {
                            return Ok(());
                        }
                        let next = cost.checked_add(arc.cost).ok_or(Error::CostOverflow)?;
                        if !reached[arc.to] || next < distances[arc.to] {
                            // Duplicate heap entries are possible with parallel
                            // edges. Bound their vector growth separately from
                            // the node-sized dense customization workspace.
                            if workspace
                                + total_shortcuts as u64 * 32
                                + (heap.len() as u64 + 1) * 32
                                > options.max_bytes
                            {
                                return Err(Error::BudgetExceeded);
                            }
                            if !reached[arc.to] {
                                touched.push(arc.to);
                                reached[arc.to] = true;
                            }
                            distances[arc.to] = next;
                            heap.push(Reverse((next, arc.to)));
                        }
                        Ok(())
                    })?;
                }
                for node in touched.drain(..) {
                    distances[node] = u64::MAX;
                    reached[node] = false;
                }
            }
            metric.offsets.push(metric.arcs.len() as u32);
            index.levels.push(metric);
        }
        index.rebuild_reverse();
        Ok(index)
    }

    /// Reconstruct only the chosen clique arcs, recursively through lower levels.
    /// No original geometry is discarded or duplicated in the accelerator file.
    pub(crate) fn unpack(
        &self,
        from: usize,
        arc: Link,
        ctx: &mut Context<'_, '_>,
        limit: usize,
    ) -> Result<Vec<usize>, Error> {
        let mut pending = vec![(from, arc)];
        let mut result = Vec::new();
        while let Some((source, arc)) = pending.pop() {
            ctx.tick()?;
            let Some(level) = arc.level else {
                if result.len() >= limit {
                    return Err(ctx.fail(
                        "geometry",
                        ctx.budget.max_geometry.saturating_add(1),
                        ctx.budget.max_geometry,
                    ));
                }
                result.push(self.original(arc.edge));
                continue;
            };
            let cell = self.cell(level, source);
            let mut distances = HashMap::from([(source, 0_u64)]);
            let mut previous = HashMap::new();
            let mut heap = BinaryHeap::from([Reverse((0_u64, source))]);
            while let Some(Reverse((cost, node))) = heap.pop() {
                ctx.tick()?;
                if distances.get(&node) != Some(&cost) {
                    continue;
                }
                if node == arc.to {
                    break;
                }
                self.visit(node, self.lower_level(node, level), false, false, |next| {
                    ctx.tick()?;
                    if self.cell(level, next.to) != cell {
                        return Ok(());
                    }
                    let cost = cost.checked_add(next.cost).ok_or(Error::CostOverflow)?;
                    if distances.get(&next.to).is_none_or(|&old| cost < old) {
                        ctx.capacity(
                            distances.len() + usize::from(!distances.contains_key(&next.to)),
                            heap.len() + 1,
                        )?;
                        distances.insert(next.to, cost);
                        previous.insert(next.to, (node, next));
                        heap.push(Reverse((cost, next.to)));
                    }
                    Ok(())
                })?;
            }
            if distances.get(&arc.to) != Some(&arc.cost) {
                return Err(Error::InvalidDataset);
            }
            let mut cursor = arc.to;
            while cursor != source {
                ctx.tick()?;
                let &(parent, next) = previous.get(&cursor).ok_or(Error::InvalidDataset)?;
                pending.push((parent, next));
                cursor = parent;
            }
        }
        Ok(result)
    }
}

/// Explicit offline partition/customization step; graph format v1/v2 is unchanged.
/// Existing queries keep working without the optional accelerator.
pub fn customize(
    path: &Path,
    limits: LoadOptions,
    mut options: CustomizationOptions,
) -> Result<AccelerationInfo, Error> {
    let manifest =
        routeweft_dataset::inspect_manifest(path, limits).map_err(super::roads::dataset_error)?;
    let base = routeweft_dataset::check_resident_budget(&manifest, limits, true)
        .map_err(super::roads::dataset_error)?;
    options.max_bytes = options
        .max_bytes
        .min(limits.max_resident_bytes.saturating_sub(base));
    let (manifest, graph) =
        routeweft_dataset::open(path, limits).map_err(super::roads::dataset_error)?;
    let outgoing = Adjacency::with_limits(graph.nodes.len(), &graph.edges, limits)
        .map_err(super::roads::dataset_error)?;
    let index = Index::build(&graph, &outgoing, options)?;
    index.save(path, &manifest)?;
    Ok(index.info())
}

pub(crate) fn load(
    path: &Path,
    manifest: &Manifest,
    graph: &Graph,
    limits: LoadOptions,
) -> Result<Option<Index>, Error> {
    Index::load(path, manifest, graph, limits)
}
