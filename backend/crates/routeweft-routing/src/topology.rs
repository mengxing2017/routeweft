//! Sparse restriction product graph. A non-root automaton state has one physical
//! end vertex, so expansion is O(vertices + prefixes), not their Cartesian product.
use crate::{diagnostics::Context, roads::Restrictions, Error};
use routeweft_dataset::Graph;
use std::ops::Index;

pub(crate) struct Rows {
    offsets: Vec<usize>,
    ids: Vec<usize>,
}
impl Rows {
    pub fn new(n: usize, sources: impl Iterator<Item = usize> + Clone) -> Self {
        let mut offsets = vec![0; n + 1];
        for v in sources.clone() {
            offsets[v + 1] += 1;
        }
        for v in 0..n {
            offsets[v + 1] += offsets[v];
        }
        let mut ids = vec![0; offsets[n]];
        for (id, v) in sources.enumerate() {
            ids[offsets[v]] = id;
            offsets[v] += 1;
        }
        offsets.copy_within(..n, 1);
        offsets[0] = 0;
        Self { offsets, ids }
    }
}
impl Index<usize> for Rows {
    type Output = [usize];
    fn index(&self, v: usize) -> &[usize] {
        &self.ids[self.offsets[v]..self.offsets[v + 1]]
    }
}
#[derive(Clone, Copy)]
pub(crate) struct Edge {
    pub from: usize,
    pub to: usize,
    pub original: usize,
    pub cost: u64,
}
pub(crate) struct Topology {
    pub states: Vec<(usize, usize)>,
    pub extras: Vec<Option<usize>>,
    pub edges: Vec<Edge>,
    pub outgoing: Rows,
    pub incoming: Rows,
}
impl Topology {
    pub fn build(g: &Graph, ctx: &mut Context<'_, '_>, max_bytes: u64) -> Result<Self, Error> {
        ctx.tick()?;
        let restrictions = Restrictions::new(&g.forbidden);
        let mut states: Vec<_> = (0..g.nodes.len()).map(|v| (v, 0)).collect();
        let mut extras = vec![None; restrictions.nodes.len()];
        for node in &restrictions.nodes {
            for (&edge, &a) in &node.next {
                ctx.tick()?;
                if !restrictions.nodes[a].terminal {
                    extras[a] = Some(states.len());
                    states.push((g.edges[edge].to, a));
                }
            }
        }
        let base = Rows::new(g.nodes.len(), g.edges.iter().map(|e| e.from));
        let mut edges = Vec::with_capacity(g.edges.len());
        for (from, &(v, a)) in states.iter().enumerate() {
            ctx.tick()?;
            for &id in &base[v] {
                ctx.tick()?;
                if let Some(b) = restrictions.step(a, id) {
                    let e = &g.edges[id];
                    let to = if b == 0 {
                        e.to
                    } else {
                        extras[b].ok_or(Error::InvalidGraph)?
                    };
                    if states.len() as u64 * 64 + (edges.len() as u64 + 1) * 96 > max_bytes {
                        return Err(Error::BudgetExceeded);
                    }
                    edges.push(Edge {
                        from,
                        to,
                        original: id,
                        cost: e.duration_ms,
                    });
                }
            }
        }
        let outgoing = Rows::new(states.len(), edges.iter().map(|e| e.from));
        let incoming = Rows::new(states.len(), edges.iter().map(|e| e.to));
        Ok(Self {
            states,
            extras,
            edges,
            outgoing,
            incoming,
        })
    }
    pub fn node(&self, vertex: usize, automaton: usize) -> Option<usize> {
        if automaton == 0 {
            Some(vertex)
        } else {
            self.extras
                .get(automaton)
                .copied()
                .flatten()
                .filter(|&id| self.states[id].0 == vertex)
        }
    }
    pub fn bytes(&self) -> u64 {
        self.states.len() as u64 * 32 + self.extras.len() as u64 * 16 + self.edges.len() as u64 * 48
    }
}
