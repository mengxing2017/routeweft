//! Recursive balanced graph cuts. Spatial sweeps supply inertial seeds; actual
//! directed edge cuts choose the seed and balanced boundary swaps refine it.
//! Components are a separate seed so disconnected interleaved roads stay apart.
use super::*;
use crate::topology::{Rows, Topology};

pub(super) fn partition(
    g: &Graph,
    topology: &Topology,
    leaf_size: usize,
    ctx: &mut Context<'_, '_>,
) -> Result<Vec<u32>, Error> {
    let n = g.nodes.len();
    let out = Rows::new(n, g.edges.iter().map(|e| e.from));
    let incoming = Rows::new(n, g.edges.iter().map(|e| e.to));
    let mut components: Vec<_> = (0..n as u32).collect();
    for e in &g.edges {
        ctx.tick()?;
        let a = root(&mut components, e.from);
        let b = root(&mut components, e.to);
        components[a] = b as u32;
    }
    for v in 0..n {
        components[v] = root(&mut components, v) as u32;
    }
    let leaves = n.div_ceil(leaf_size).max(1);
    let mut leaf = vec![0; n];
    let mut rank = vec![usize::MAX; n];
    let mut pending = vec![((0..n).collect::<Vec<_>>(), 0, leaves)];
    while let Some((nodes, first, cells)) = pending.pop() {
        ctx.tick()?;
        if cells == 1 {
            for v in nodes {
                leaf[v] = first as u32;
            }
            continue;
        }
        let left_cells = cells / 2;
        let split = (nodes.len() * left_cells / cells).clamp(1, nodes.len() - 1);
        let mut best = Vec::new();
        let mut best_cut = usize::MAX;
        for axis in 0..5 {
            let mut candidate = nodes.clone();
            candidate.sort_unstable_by(|&a, &b| {
                if axis == 4 {
                    return components[a].cmp(&components[b]).then(a.cmp(&b));
                }
                let project = |v: usize| {
                    let p = g.nodes[v];
                    match axis {
                        0 => p.lon,
                        1 => p.lat,
                        2 => p.lon + p.lat,
                        _ => p.lon - p.lat,
                    }
                };
                project(a).total_cmp(&project(b)).then(a.cmp(&b))
            });
            for (i, &v) in candidate.iter().enumerate() {
                rank[v] = i;
            }
            let mut cut = 0;
            for &v in &nodes {
                ctx.tick()?;
                for &id in &out[v] {
                    ctx.tick()?;
                    let to = g.edges[id].to;
                    if rank[to] != usize::MAX && (rank[v] < split) != (rank[to] < split) {
                        cut += 1;
                    }
                }
            }
            if cut < best_cut {
                best_cut = cut;
                best = candidate;
            }
            for &v in &nodes {
                rank[v] = usize::MAX;
            }
        }
        for (i, &v) in best.iter().enumerate() {
            rank[v] = i;
        }
        // One balanced refinement pass, with exact gain recomputation at each
        // accepted pair. Never increases the selected seed's cut or changes size.
        let gain = |v: usize, rank: &[usize]| -> i64 {
            out[v]
                .iter()
                .chain(incoming[v].iter())
                .map(|&id| {
                    let e = &g.edges[id];
                    let w = if e.from == v { e.to } else { e.from };
                    if w == v || rank[w] == usize::MAX {
                        0
                    } else if (rank[w] < split) == (rank[v] < split) {
                        -1
                    } else {
                        1
                    }
                })
                .sum()
        };
        let mut left: Vec<_> = best[..split].iter().map(|&v| (gain(v, &rank), v)).collect();
        let mut right: Vec<_> = best[split..].iter().map(|&v| (gain(v, &rank), v)).collect();
        left.sort_unstable_by(|a, b| b.cmp(a));
        right.sort_unstable_by(|a, b| b.cmp(a));
        for ((_, a), (_, b)) in left.into_iter().zip(right) {
            ctx.tick()?;
            let correction = out[a].iter().filter(|&&id| g.edges[id].to == b).count()
                + out[b].iter().filter(|&&id| g.edges[id].to == a).count();
            if gain(a, &rank) + gain(b, &rank) > 2 * correction as i64 {
                let ia = rank[a];
                let ib = rank[b];
                best.swap(ia, ib);
                rank.swap(a, b);
            }
        }
        for &v in &nodes {
            rank[v] = usize::MAX;
        }
        let right = best.split_off(split);
        pending.push((right, first + left_cells, cells - left_cells));
        pending.push((best, first, left_cells));
    }
    Ok(topology.states.iter().map(|&(v, _)| leaf[v]).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interleaved_disconnected_roads_are_cut_by_topology_not_coordinate_order() {
        let nodes: Vec<_> = (0..64)
            .map(|i| routeweft_dataset::Point {
                lon: (i / 2) as f64 * 0.00001,
                lat: 0.,
            })
            .collect();
        let edges: Vec<_> = (0..62)
            .map(|i| routeweft_dataset::Edge {
                from: i,
                to: i + 2,
                way: 1,
                distance_mm: 1,
                duration_ms: 1,
                name: String::new(),
                class: "road".into(),
                roundabout: false,
            })
            .collect();
        let graph = Graph {
            nodes,
            edges,
            forbidden: vec![],
        };
        let budget = SearchBudget::default();
        let mut ctx = Context::new(&budget, true);
        let topology = Topology::build(&graph, &mut ctx, 10_000_000).unwrap();
        let leaf = partition(&graph, &topology, 32, &mut ctx).unwrap();
        assert_eq!(leaf.iter().filter(|&&c| c == 0).count(), 32);
        assert!(graph.edges.iter().all(|e| leaf[e.from] == leaf[e.to]));
        assert_ne!(leaf[0], leaf[1]);
    }
}
