//! Degree-two topology chains with lossless original-edge witnesses. Restriction
//! endpoints AND their neighbours remain explicit, retaining cheap U-turns that
//! can legally clear a restriction prefix. Query cuts expose all snapped points.
use super::*;
#[derive(Clone, Copy)]
pub(super) struct Chain {
    pub offset: usize,
    pub len: usize,
}
pub(super) struct Compression {
    chains: Vec<Chain>,
    edges: Vec<usize>,
    prefix: Vec<u128>,
    location: Vec<(u32, u32)>,
    boundaries: HashMap<u32, Vec<usize>>,
}
pub(super) type Cuts = HashMap<u32, Vec<usize>>;
impl Compression {
    pub fn new(g: &Graph, out: &Adjacency, incoming: &crate::topology::Rows) -> Self {
        let n = g.nodes.len();
        let mut protected = vec![false; n];
        for sequence in &g.forbidden {
            for &id in sequence {
                let e = &g.edges[id];
                protected[e.from] = true;
                protected[e.to] = true;
            }
        }
        let restrictions = protected.clone();
        for e in &g.edges {
            if restrictions[e.from] {
                protected[e.to] = true;
            }
            if restrictions[e.to] {
                protected[e.from] = true;
            }
        }
        let compatible = |a: usize, b: usize| {
            let a = &g.edges[a];
            let b = &g.edges[b];
            a.way == b.way && a.name == b.name && a.class == b.class && a.roundabout == b.roundabout
        };
        let mut pass = vec![false; n];
        for v in 0..n {
            if protected[v]
                || out[v].is_empty()
                || out[v].len() > 2
                || incoming[v].is_empty()
                || incoming[v].len() > 2
            {
                continue;
            }
            let mut neighbours: Vec<_> = out[v]
                .iter()
                .map(|&e| g.edges[e].to)
                .chain(incoming[v].iter().map(|&e| g.edges[e].from))
                .collect();
            neighbours.sort_unstable();
            neighbours.dedup();
            if neighbours.len() != 2 || neighbours.contains(&v) {
                continue;
            }
            pass[v] = incoming[v].iter().all(|&a| {
                out[v]
                    .iter()
                    .filter(|&&b| g.edges[b].to != g.edges[a].from && compatible(a, b))
                    .count()
                    == 1
            }) && out[v].iter().all(|&b| {
                incoming[v]
                    .iter()
                    .filter(|&&a| g.edges[a].from != g.edges[b].to && compatible(a, b))
                    .count()
                    == 1
            });
        }
        // Break anchorless rings deterministically. Each original edge is visited
        // at most once by this temporary successor traversal.
        let mut visited = vec![false; g.edges.len()];
        for first in 0..g.edges.len() {
            if visited[first] {
                continue;
            }
            let mut edge = first;
            let mut cycle = false;
            while !visited[edge] {
                visited[edge] = true;
                let e = &g.edges[edge];
                if !pass[e.to] {
                    break;
                }
                edge = *out[e.to]
                    .iter()
                    .find(|&&id| g.edges[id].to != e.from && compatible(edge, id))
                    .unwrap();
                if edge == first {
                    cycle = true;
                    break;
                }
            }
            if cycle && pass[g.edges[first].from] {
                pass[g.edges[first].from] = false;
            }
        }
        let mut result = Self {
            chains: Vec::new(),
            edges: Vec::with_capacity(g.edges.len()),
            prefix: Vec::with_capacity(g.edges.len()),
            location: vec![(u32::MAX, 0); g.edges.len()],
            boundaries: HashMap::new(),
        };
        for v in 0..n {
            if pass[v] {
                continue;
            }
            for &first in &out[v] {
                if result.location[first].0 != u32::MAX {
                    continue;
                }
                let chain = result.chains.len() as u32;
                let offset = result.edges.len();
                let mut edge = first;
                let mut cost = 0_u128;
                loop {
                    let e = &g.edges[edge];
                    let pos = result.edges.len() - offset;
                    result.location[edge] = (chain, pos as u32);
                    result.edges.push(edge);
                    result.prefix.push(cost);
                    cost += e.duration_ms as u128;
                    if !pass[e.to] {
                        break;
                    }
                    edge = *out[e.to]
                        .iter()
                        .find(|&&id| g.edges[id].to != e.from && compatible(edge, id))
                        .unwrap();
                }
                result.chains.push(Chain {
                    offset,
                    len: result.edges.len() - offset,
                });
            }
        }
        debug_assert!(result.location.iter().all(|p| p.0 != u32::MAX));
        result
    }
    pub fn set_boundaries(&mut self, g: &Graph, index: &Index) {
        self.boundaries.clear();
        for (chain, c) in self.chains.iter().enumerate() {
            for pos in 1..c.len {
                let vertex = g.edges[self.edges[c.offset + pos]].from;
                if index.is_boundary(vertex) {
                    self.boundaries.entry(chain as u32).or_default().push(pos);
                }
            }
        }
    }
    pub fn cuts(
        &self,
        g: &Graph,
        out: &Adjacency,
        incoming: &crate::topology::Rows,
        overlay: &Overlay,
    ) -> Cuts {
        let mut cuts: Cuts = HashMap::new();
        for &v in overlay.marks.keys() {
            if v < overlay.base {
                for &e in &out[v] {
                    let (c, p) = self.location[e];
                    cuts.entry(c).or_default().push(p as usize);
                }
                for &e in &incoming[v] {
                    let (c, p) = self.location[e];
                    cuts.entry(c).or_default().push(p as usize + 1);
                }
            }
        }
        for s in &overlay.splits {
            let (c, p) = self.location[s.edge];
            let list = cuts.entry(c).or_default();
            list.push(p as usize);
            list.push(p as usize + 1);
        }
        let _ = g;
        for list in cuts.values_mut() {
            list.sort_unstable();
            list.dedup();
        }
        cuts
    }
    pub fn jump(&self, edge: usize, reverse: bool, cuts: &Cuts) -> Option<(u32, usize, usize)> {
        let (chain, p) = self.location[edge];
        let p = p as usize;
        let c = self.chains[chain as usize];
        let mut first = 0;
        let mut last = c.len;
        for list in [self.boundaries.get(&chain), cuts.get(&chain)]
            .into_iter()
            .flatten()
        {
            if reverse {
                let i = list.partition_point(|&v| v <= p);
                if i > 0 {
                    first = first.max(list[i - 1]);
                }
            } else {
                let i = list.partition_point(|&v| v <= p);
                if i < list.len() {
                    last = last.min(list[i]);
                }
            }
        }
        let (start, end) = if reverse { (first, p + 1) } else { (p, last) };
        (end - start > 1).then_some((chain, start, end))
    }
    pub fn range(&self, chain: u32, start: usize, end: usize) -> &[usize] {
        let c = self.chains[chain as usize];
        &self.edges[c.offset + start..c.offset + end]
    }
    pub fn cost(&self, g: &Graph, chain: u32, start: usize, end: usize) -> Result<u64, Error> {
        let c = self.chains[chain as usize];
        let end_edge = self.edges[c.offset + end - 1];
        let value = self.prefix[c.offset + end - 1] + g.edges[end_edge].duration_ms as u128
            - self.prefix[c.offset + start];
        u64::try_from(value).map_err(|_| Error::CostOverflow)
    }
}
