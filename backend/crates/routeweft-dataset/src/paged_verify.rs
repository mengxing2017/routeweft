//! Offline semantic checks against the authenticated compact graph, with charged scratch.
use super::*;
#[derive(Debug, Serialize)]
pub struct QueryIndexVerification {
    pub scope: &'static str,
    pub nodes: u64,
    pub edges: u64,
    pub spatial_nodes: u64,
    pub workspace_bytes: u64,
}
impl PagedGraph {
    pub fn verify_indexes(&self, workspace: u64) -> Result<QueryIndexVerification> {
        let m = &self.metadata;
        let required = m
            .nodes
            .checked_mul(8)
            .and_then(|v| v.checked_add(m.edges.div_ceil(8)))
            .and_then(|v| v.checked_add(m.spatial_count.div_ceil(8)))
            .and_then(|v| v.checked_add(131072))
            .ok_or(Error::LimitExceeded)?;
        if required > workspace {
            return Err(resource_limit(
                "query_index_validation_workspace",
                workspace,
                required,
                "verify_query",
            ));
        }
        let header = self.read::<64>(0, 0)?;
        if m.dictionary_count != u64_at(&header, 40) || m.graph_nodes != 64 + u64_at(&header, 56) {
            return Err(invalid("dictionary layout binding"));
        }
        let mut position = 64;
        for id in 0..m.dictionary_count {
            let length = u32_at(&self.read::<4>(0, position)?, 0);
            position += 4;
            let entry = self.read::<12>(1, m.dictionary + id * 12)?;
            if u64_at(&entry, 0) != position || u32_at(&entry, 8) != length {
                return Err(invalid("dictionary offset binding"));
            }
            position += length as u64;
        }
        if position != m.graph_nodes {
            return Err(invalid("dictionary extent"));
        }
        let mut parent: Vec<u32> = (0..m.nodes as u32).collect();
        let mut sizes = vec![0u32; m.nodes as usize];
        let mut previous_end = 0;
        let mut min = [f64::INFINITY; 2];
        let mut max = [f64::NEG_INFINITY; 2];
        for node in 0..m.nodes as usize {
            let p = self.node(node)?;
            min[0] = min[0].min(p.lon);
            min[1] = min[1].min(p.lat);
            max[0] = max[0].max(p.lon);
            max[1] = max[1].max(p.lat);
            let range = self.outgoing_range(node)?;
            if range.start != previous_end {
                return Err(invalid("CSR continuity"));
            }
            previous_end = range.end;
            let mut last = None;
            for offset in range {
                let id = self.outgoing_edge(offset)?;
                if last.is_some_and(|last| last >= id) {
                    return Err(invalid("CSR duplicate/order"));
                }
                last = Some(id);
                let e = self.edge_record(id)?;
                let from = u64_at(&e, 0) as usize;
                let to = u64_at(&e, 8) as usize;
                if from != node || to >= parent.len() {
                    return Err(invalid("CSR source binding"));
                }
                if m.schema == 2 {
                    let cost = u64_at(&e, 32);
                    if self.outgoing_record(node, offset)? != (id, to, cost) {
                        return Err(invalid("compact CSR binding"));
                    }
                    let a = self.landmark_potentials(from)?;
                    let b = self.landmark_potentials(to)?;
                    for i in 0..4 {
                        if (a[i] == u64::MAX) != (b[i] == u64::MAX)
                            || (a[i] != u64::MAX && a[i].abs_diff(b[i]) > cost)
                        {
                            return Err(invalid("landmark potential feasibility"));
                        }
                    }
                }
                let a = find(&mut parent, from);
                let b = find(&mut parent, to);
                parent[a.max(b)] = a.min(b) as u32;
            }
        }
        if previous_end != m.edges
            || (min[1] + max[1]) / 2. != m.latitude_midpoint
            || (max[0] - min[0]).max(max[1] - min[1]) != m.span_degrees
        {
            return Err(invalid("query coverage binding"));
        }
        for i in 0..parent.len() {
            parent[i] = find(&mut parent, i) as u32;
            sizes[parent[i] as usize] += 1;
        }
        for (i, &root) in parent.iter().enumerate() {
            if self.component(i)? != (root, sizes[root as usize]) {
                return Err(invalid("component binding"));
            }
        }
        let largest = sizes
            .iter()
            .enumerate()
            .max_by_key(|&(i, s)| (*s, std::cmp::Reverse(i)))
            .map(|(i, _)| i as u32)
            .unwrap_or(0);
        if largest != m.largest_component {
            return Err(invalid("largest component binding"));
        }
        drop(parent);
        drop(sizes);
        let mut seen_edges = vec![0u8; m.edges.div_ceil(8) as usize];
        let mut seen_nodes = vec![0u8; m.spatial_count.div_ceil(8) as usize];
        let mut stack = Vec::new();
        if m.spatial_count > 0 {
            stack.push((0, [-180., -85., 180., 85.]));
        }
        let mut nodes = 0;
        let mut edges = 0;
        while let Some((id, outer)) = stack.pop() {
            if id >= m.spatial_count || stack.len() > 64 {
                return Err(invalid("spatial index shape"));
            }
            let mask = 1u8 << (id % 8);
            let entry = &mut seen_nodes[id as usize / 8];
            if *entry & mask != 0 {
                return Err(invalid("duplicate spatial node"));
            }
            *entry |= mask;
            nodes += 1;
            let b = self.read::<64>(1, m.spatial + id * 64)?;
            let mut bounds = [0.; 4];
            for i in 0..4 {
                bounds[i] = f64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
            }
            if bounds.iter().any(|v| !v.is_finite())
                || bounds[0] > bounds[2]
                || bounds[1] > bounds[3]
                || bounds[0] < outer[0]
                || bounds[1] < outer[1]
                || bounds[2] > outer[2]
                || bounds[3] > outer[3]
            {
                return Err(invalid("spatial bounding box"));
            }
            let count = u64_at(&b, 56);
            if count == 0 {
                let left = u64_at(&b, 32);
                let right = u64_at(&b, 40);
                if left <= id || right <= id {
                    return Err(invalid("spatial topology"));
                }
                stack.push((right, bounds));
                stack.push((left, bounds));
            } else {
                let start = u64_at(&b, 48);
                if count > 32 || start.checked_add(count).is_none_or(|v| v > m.edges) {
                    return Err(invalid("spatial leaf layout"));
                }
                for offset in start..start + count {
                    let edge = u64_at(&self.read::<8>(1, m.spatial_ids + offset * 8)?, 0);
                    if edge >= m.edges {
                        return Err(invalid("spatial edge id"));
                    }
                    let mask = 1u8 << (edge % 8);
                    let entry = &mut seen_edges[edge as usize / 8];
                    if *entry & mask != 0 {
                        return Err(invalid("duplicate spatial edge"));
                    }
                    *entry |= mask;
                    edges += 1;
                    let e = self.edge_record(edge as usize)?;
                    for node in [u64_at(&e, 0), u64_at(&e, 8)] {
                        let p = self.node(node as usize)?;
                        if p.lon < bounds[0]
                            || p.lat < bounds[1]
                            || p.lon > bounds[2]
                            || p.lat > bounds[3]
                        {
                            return Err(invalid("spatial edge bounds"));
                        }
                    }
                }
            }
        }
        if nodes != m.spatial_count || edges != m.edges {
            return Err(invalid("spatial coverage"));
        }
        Ok(QueryIndexVerification {
            scope: if m.schema == 2 {
                "semantic_CSR_dictionary_components_spatial_compact_landmark_feasibility_against_authenticated_v2"
            } else {
                "semantic_CSR_dictionary_components_spatial_against_authenticated_v2"
            },
            nodes: m.nodes,
            edges: m.edges,
            spatial_nodes: m.spatial_count,
            workspace_bytes: required,
        })
    }
}
