//! Offline lower bounds over the undirected relaxation of the original graph.
//! Integer landmark potentials remain valid with directed turns/vias removed.
use super::*;
pub(super) fn landmarks(graph: &Graph, root: usize) -> Vec<[u64; 4]> {
    use std::{cmp::Reverse, collections::BinaryHeap};
    let mut offsets = vec![0usize; graph.nodes.len() + 1];
    for e in &graph.edges {
        offsets[e.from + 1] += 1;
        offsets[e.to + 1] += 1;
    }
    for i in 0..graph.nodes.len() {
        offsets[i + 1] += offsets[i];
    }
    let mut ids = vec![0usize; graph.edges.len() * 2];
    for (id, e) in graph.edges.iter().enumerate() {
        ids[offsets[e.from]] = id;
        offsets[e.from] += 1;
        ids[offsets[e.to]] = id;
        offsets[e.to] += 1;
    }
    offsets.copy_within(..graph.nodes.len(), 1);
    offsets[0] = 0;
    let mut result = vec![[u64::MAX; 4]; graph.nodes.len()];
    let mut nearest = vec![u64::MAX; graph.nodes.len()];
    let mut root = root;
    for landmark in 0..4 {
        let mut queue = BinaryHeap::new();
        result[root][landmark] = 0;
        queue.push(Reverse((0u64, root)));
        while let Some(Reverse((cost, node))) = queue.pop() {
            if result[node][landmark] != cost {
                continue;
            }
            for &id in &ids[offsets[node]..offsets[node + 1]] {
                let e = &graph.edges[id];
                let to = if e.from == node { e.to } else { e.from };
                // Clamping preserves feasibility even for legal huge edge weights.
                let next = cost.saturating_add(e.duration_ms).min(u64::MAX - 1);
                if next < result[to][landmark] {
                    result[to][landmark] = next;
                    queue.push(Reverse((next, to)));
                }
            }
        }
        for (i, row) in result.iter().enumerate() {
            nearest[i] = nearest[i].min(row[landmark]);
        }
        root = nearest
            .iter()
            .enumerate()
            .filter(|(_, d)| **d != u64::MAX)
            .max_by_key(|&(i, d)| (*d, std::cmp::Reverse(i)))
            .map_or(root, |(i, _)| i);
    }
    result
}
