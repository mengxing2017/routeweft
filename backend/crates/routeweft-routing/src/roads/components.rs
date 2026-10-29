//! Weak components for safe rejection/snapping; iterative Kosaraju SCC labels
//! distinguish directed connectivity without assuming different SCCs unreachable.
use super::*;
use crate::topology::Rows;

pub(super) struct Components {
    pub weak: Vec<u32>,
    pub size: Vec<usize>,
    pub largest: u32,
    pub strong_count: usize,
    pub largest_strong_size: usize,
}
impl Components {
    pub fn new(g: &Graph, out: &Adjacency, incoming: &Rows) -> Self {
        let n = g.nodes.len();
        let mut seen = vec![false; n];
        let mut order = Vec::with_capacity(n);
        for start in 0..n {
            if seen[start] {
                continue;
            }
            seen[start] = true;
            let mut stack = vec![(start, 0)];
            while let Some((v, i)) = stack.last_mut() {
                if *i < out[*v].len() {
                    let to = g.edges[out[*v][*i]].to;
                    *i += 1;
                    if !seen[to] {
                        seen[to] = true;
                        stack.push((to, 0));
                    }
                } else {
                    let (v, _) = stack.pop().unwrap();
                    order.push(v);
                }
            }
        }
        seen.fill(false);
        let mut strong_count = 0;
        let mut largest_strong_size = 0;
        for &start in order.iter().rev() {
            if seen[start] {
                continue;
            }
            strong_count += 1;
            seen[start] = true;
            let mut stack = vec![start];
            let mut strong_size = 0;
            while let Some(v) = stack.pop() {
                strong_size += 1;
                for &id in &incoming[v] {
                    let from = g.edges[id].from;
                    if !seen[from] {
                        seen[from] = true;
                        stack.push(from);
                    }
                }
            }
            largest_strong_size = largest_strong_size.max(strong_size);
        }
        let mut weak = vec![u32::MAX; n];
        let mut size = Vec::new();
        for start in 0..n {
            if weak[start] != u32::MAX {
                continue;
            }
            let id = size.len() as u32;
            weak[start] = id;
            let mut count = 0;
            let mut stack = vec![start];
            while let Some(v) = stack.pop() {
                count += 1;
                for to in out[v]
                    .iter()
                    .map(|&e| g.edges[e].to)
                    .chain(incoming[v].iter().map(|&e| g.edges[e].from))
                {
                    if weak[to] == u32::MAX {
                        weak[to] = id;
                        stack.push(to);
                    }
                }
            }
            size.push(count);
        }
        let largest = size
            .iter()
            .enumerate()
            .max_by_key(|&(id, size)| (*size, std::cmp::Reverse(id)))
            .map(|(i, _)| i as u32)
            .unwrap_or(0);
        Self {
            weak,
            size,
            largest,
            strong_count,
            largest_strong_size,
        }
    }
}
