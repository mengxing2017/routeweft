//! Compact in-memory adjacency; not a persisted dataset format.
use crate::{invalid, resource_limit, Edge, LoadOptions, Result};
use std::ops::Index;

/// Outgoing edge IDs in original edge order, including self-loops and parallels.
/// Uses two contiguous arrays instead of an allocation for each non-empty node.
pub struct Adjacency {
    offsets: Box<[usize]>,
    edge_ids: Box<[usize]>,
}

impl Adjacency {
    /// Checks source indices and default graph capacity, not full graph semantics.
    pub fn new(node_count: usize, edges: &[Edge]) -> Result<Self> {
        let limits = LoadOptions::default();
        for (resource, observed, limit) in [
            ("graph_nodes", node_count, limits.max_nodes),
            ("graph_edges", edges.len(), limits.max_edges),
        ] {
            if observed > limit {
                return Err(resource_limit(
                    resource,
                    limit as u64,
                    observed as u64,
                    "adjacency_build",
                ));
            }
        }
        let mut offsets = vec![0; node_count + 1];
        for edge in edges {
            if edge.from >= node_count {
                return Err(invalid("adjacency source"));
            }
            offsets[edge.from + 1] += 1;
        }
        for node in 0..node_count {
            offsets[node + 1] += offsets[node];
        }
        let mut edge_ids = vec![0; edges.len()];
        // Reuse starts as insertion cursors, then shift the resulting ends back
        // to offsets. No additional node-sized cursor array is needed.
        for (id, edge) in edges.iter().enumerate() {
            edge_ids[offsets[edge.from]] = id;
            offsets[edge.from] += 1;
        }
        offsets.copy_within(..node_count, 1);
        offsets[0] = 0;
        Ok(Self {
            offsets: offsets.into_boxed_slice(),
            edge_ids: edge_ids.into_boxed_slice(),
        })
    }
}

impl Index<usize> for Adjacency {
    type Output = [usize];

    fn index(&self, node: usize) -> &Self::Output {
        &self.edge_ids[self.offsets[node]..self.offsets[node + 1]]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(from: usize, to: usize) -> Edge {
        Edge {
            from,
            to,
            way: 1,
            distance_mm: 1,
            duration_ms: 1,
            name: String::new(),
            class: "residential".into(),
            roundabout: false,
        }
    }

    #[test]
    fn compact_index_matches_independent_scan_in_original_order() {
        let node_count = 257;
        // Deliberately interleave sources, with parallels, self-loops, a high
        // degree source, and isolated nodes near the end.
        let edges: Vec<_> = (0..5000)
            .map(|i| edge(if i % 3 == 0 { 0 } else { (i * 31) % 251 }, (i * 17) % 251))
            .collect();
        let index = Adjacency::new(node_count, &edges).unwrap();
        for node in 0..node_count {
            let expected: Vec<_> = edges
                .iter()
                .enumerate()
                .filter_map(|(id, edge)| (edge.from == node).then_some(id))
                .collect();
            assert_eq!(&index[node], expected);
        }
        assert_eq!(index.offsets.len(), node_count + 1);
        assert_eq!(index.edge_ids.len(), edges.len());
        assert_eq!(*index.offsets.last().unwrap(), edges.len());
    }

    #[test]
    fn empty_and_invalid_indices_do_not_require_per_node_allocations() {
        let empty = Adjacency::new(0, &[]).unwrap();
        assert_eq!(&*empty.offsets, &[0]);
        let isolated = Adjacency::new(1000, &[]).unwrap();
        assert!(isolated.edge_ids.is_empty());
        assert!(isolated.offsets.iter().all(|&offset| offset == 0));
        assert!(Adjacency::new(0, &[edge(0, 0)]).is_err());
        assert!(Adjacency::new(3, &[edge(3, 0)]).is_err());
        let error = Adjacency::new(LoadOptions::default().max_nodes + 1, &[]);
        assert!(matches!(error, Err(crate::Error::ResourceLimit(_))));
    }
}
