//! Direct compact search. Labels contain IDs and integer costs, never Edge strings.
use super::*;
#[derive(Clone, Copy)]
struct Label {
    state: State,
    cost: u64,
    parent: usize,
    arc: Traversal,
}
struct Workspace {
    labels: Vec<Label>,
    ids: HashMap<State, usize>,
    queue: BinaryHeap<Reverse<(u64, u64, usize)>>,
}
impl Workspace {
    fn charge(
        &self,
        base: u64,
        growth: u64,
        limit: u64,
        ctx: &mut Context<'_, '_>,
    ) -> Result<(), Error> {
        let labels_bytes = self.labels.capacity() as u64 * std::mem::size_of::<Label>() as u64;
        let map_bytes = self.ids.capacity() as u64 * 64;
        let frontier_bytes = self.queue.capacity() as u64 * 24;
        let current = labels_bytes
            .saturating_add(map_bytes)
            .saturating_add(frontier_bytes);
        let bytes = current.saturating_add(base).saturating_add(growth);
        let previous_peak = ctx
            .diagnostics
            .paged_workspace
            .as_ref()
            .map_or(0, |v| v.planned_peak_bytes);
        ctx.diagnostics.paged_workspace = Some(crate::diagnostics::PagedWorkspaceUsage {
            labels: self.labels.len(),
            labels_capacity: self.labels.capacity(),
            labels_bytes,
            map_capacity: self.ids.capacity(),
            map_bytes,
            frontier: self.queue.len(),
            frontier_capacity: self.queue.capacity(),
            frontier_bytes,
            fixed_and_restrictions_bytes: base,
            current_bytes: current.saturating_add(base),
            allocation_growth_bytes: growth,
            planned_peak_bytes: previous_peak.max(bytes),
            limit_bytes: limit,
        });
        if bytes > limit {
            return Err(ctx.fail("paged_query_bytes", bytes as usize, limit as usize));
        }
        Ok(())
    }
    fn reserve(
        &mut self,
        new_state: bool,
        base: u64,
        limit: u64,
        ctx: &mut Context<'_, '_>,
    ) -> Result<(), Error> {
        ctx.capacity(
            self.labels.len() + usize::from(new_state),
            self.queue.len() + 1,
        )?;
        // Charge growth overlap before allocation. HashMap's capacity excludes
        // control bytes; 64/entry includes buckets, slack and allocator allowance.
        let mut growth = 0;
        let labels = new_state && self.labels.len() == self.labels.capacity();
        let ids = new_state && self.ids.len() == self.ids.capacity();
        let queue = self.queue.len() == self.queue.capacity();
        let target_labels = (self.labels.capacity() * 2).max(64);
        let target_ids = (self.ids.capacity() * 2).max(64);
        let target_queue = (self.queue.capacity() * 2).max(64);
        if labels {
            growth += target_labels as u64 * std::mem::size_of::<Label>() as u64;
        }
        if ids {
            growth += target_ids as u64 * 128;
        }
        if queue {
            growth += target_queue as u64 * 24;
        }
        self.charge(base, growth, limit, ctx)?;
        if labels {
            self.labels
                .try_reserve_exact(target_labels - self.labels.len())
                .map_err(|_| Error::BudgetExceeded)?;
        }
        if ids {
            self.ids
                .try_reserve(target_ids - self.ids.len())
                .map_err(|_| Error::BudgetExceeded)?;
        }
        if queue {
            self.queue
                .try_reserve_exact(target_queue - self.queue.len())
                .map_err(|_| Error::BudgetExceeded)?;
        }
        self.charge(base, 0, limit, ctx)
    }
}
impl PagedEngine {
    fn visit_compact_arcs(
        &self,
        overlay: &Overlay,
        vertex: usize,
        mut visit: impl FnMut(Arc, u64) -> Result<(), Error>,
    ) -> Result<(), Error> {
        if vertex < overlay.base {
            for offset in self.graph.outgoing_range(vertex).map_err(dataset_error)? {
                let (edge, to, duration) = self
                    .graph
                    .outgoing_record(vertex, offset)
                    .map_err(dataset_error)?;
                let first = overlay.by_edge.get(&edge).and_then(|v| v.first()).copied();
                visit(
                    Arc {
                        to: first.unwrap_or(to),
                        edge,
                        start: 0.,
                        end: first
                            .map(|v| overlay.splits[v - overlay.base].fraction)
                            .unwrap_or(1.),
                        enter: true,
                    },
                    duration,
                )?;
            }
        } else {
            let split = &overlay.splits[vertex - overlay.base];
            let raw = self.graph.edge_record(split.edge).map_err(dataset_error)?;
            let ids = &overlay.by_edge[&split.edge];
            let next = ids
                .iter()
                .position(|&v| v == vertex)
                .and_then(|i| ids.get(i + 1))
                .copied();
            visit(
                Arc {
                    to: next.unwrap_or(integer(&raw, 8)),
                    edge: split.edge,
                    start: split.fraction,
                    end: next
                        .map(|v| overlay.splits[v - overlay.base].fraction)
                        .unwrap_or(1.),
                    enter: false,
                },
                u64::from_le_bytes(raw[32..40].try_into().unwrap()),
            )?;
        }
        Ok(())
    }
    pub(super) fn search_compact(
        &self,
        matches: &[Vec<Nearest>],
        overlay: &Overlay,
        restrictions: &Restrictions,
        restrictions_bytes: u64,
        ctx: &mut Context<'_, '_>,
    ) -> Result<RouteResult, Error> {
        let limit = self.options.query_workspace_bytes;
        let base = restrictions_bytes.saturating_add(131072);
        let mut targets = Vec::new();
        for list in matches {
            let mut values = Vec::new();
            for m in list {
                let nodes: &[usize] = if m.fraction <= 1e-10 {
                    std::slice::from_ref(&m.from)
                } else if m.fraction >= 1. - 1e-10 {
                    std::slice::from_ref(&m.to)
                } else {
                    &[]
                };
                if nodes.is_empty() {
                    values.push(
                        self.graph
                            .landmark_potentials(m.from)
                            .map_err(dataset_error)?,
                    );
                    values.push(
                        self.graph
                            .landmark_potentials(m.to)
                            .map_err(dataset_error)?,
                    );
                } else {
                    values.push(
                        self.graph
                            .landmark_potentials(nodes[0])
                            .map_err(dataset_error)?,
                    );
                }
            }
            targets.push(values);
        }
        let accelerated = ctx.diagnostics.algorithm == "astar";
        let heuristic = |state: State| -> Result<u64, Error> {
            if !accelerated || state.stage == matches.len() || state.vertex >= overlay.base {
                return Ok(0);
            }
            let p = self
                .graph
                .landmark_potentials(state.vertex)
                .map_err(dataset_error)?;
            Ok(targets[state.stage]
                .iter()
                .map(|t| {
                    (0..4)
                        .filter(|&i| p[i] != u64::MAX && t[i] != u64::MAX)
                        .map(|i| p[i].abs_diff(t[i]))
                        .max()
                        .unwrap_or(0)
                })
                .min()
                .unwrap_or(0))
        };
        let mut work = Workspace {
            labels: Vec::new(),
            ids: HashMap::new(),
            queue: BinaryHeap::new(),
        };
        for m in &matches[0] {
            ctx.tick()?;
            let vertex = overlay.vertex_for(m);
            let automaton = if m.fraction > 1e-10 && m.fraction < 1. - 1e-10 {
                restrictions.step(0, m.edge).ok_or(Error::InvalidDataset)?
            } else {
                0
            };
            let state = State {
                vertex,
                automaton,
                stage: overlay.advance(vertex, 1),
            };
            if !work.ids.contains_key(&state) {
                work.reserve(true, base, limit, ctx)?;
                let id = work.labels.len();
                work.ids.insert(state, id);
                work.labels.push(Label {
                    state,
                    cost: 0,
                    parent: usize::MAX,
                    arc: Traversal {
                        edge: 0,
                        start: 0.,
                        end: 0.,
                    },
                });
                work.queue.push(Reverse((heuristic(state)?, 0, id)));
            }
        }
        while let Some(Reverse((_, cost, id))) = work.queue.pop() {
            ctx.tick()?;
            let label = work.labels[id];
            if label.cost != cost {
                continue;
            }
            if label.state.stage == matches.len() {
                ctx.phase("path_recovery");
                let mut path = Vec::new();
                let mut cursor = id;
                while work.labels[cursor].parent != usize::MAX {
                    ctx.tick()?;
                    if path.len() + 2 > ctx.budget.max_geometry {
                        return Err(ctx.fail("geometry", path.len() + 2, ctx.budget.max_geometry));
                    }
                    work.charge(
                        base,
                        (path.len() as u64 + 1).next_power_of_two() * 96,
                        limit,
                        ctx,
                    )?;
                    let label = work.labels[cursor];
                    let parent = &work.labels[label.parent];
                    path.push((label.arc, parent.state.stage, label.state.stage));
                    cursor = label.parent;
                }
                path.reverse();
                let source = work.labels[cursor].state;
                drop(work);
                ctx.memory = Some((limit, base + path.capacity() as u64 * 48));
                ctx.capacity(0, 0)?;
                ctx.phase("recover");
                return recovery::recover_path(
                    self,
                    restrictions,
                    source.vertex,
                    source.stage,
                    cost,
                    path,
                    matches.len(),
                    overlay,
                    ctx,
                );
            }
            ctx.diagnostics.settled_states += 1;
            self.visit_compact_arcs(overlay, label.state.vertex, |arc, duration| {
                ctx.tick()?;
                ctx.diagnostics.scanned_edges += 1;
                let automaton = if arc.enter {
                    restrictions.step(label.state.automaton, arc.edge)
                } else {
                    Some(label.state.automaton)
                };
                let Some(automaton) = automaton else {
                    return Ok(());
                };
                let state = State {
                    vertex: arc.to,
                    automaton,
                    stage: overlay.advance(arc.to, label.state.stage),
                };
                let cost = cost
                    .checked_add(scaled(duration, arc.end - arc.start)?)
                    .ok_or(Error::CostOverflow)?;
                let existing = work.ids.get(&state).copied();
                if existing.is_none_or(|id| cost < work.labels[id].cost) {
                    work.reserve(existing.is_none(), base, limit, ctx)?;
                    let next = Label {
                        state,
                        cost,
                        parent: id,
                        arc: Traversal {
                            edge: arc.edge,
                            start: arc.start,
                            end: arc.end,
                        },
                    };
                    let next_id = if let Some(id) = existing {
                        work.labels[id] = next;
                        id
                    } else {
                        let id = work.labels.len();
                        work.labels.push(next);
                        work.ids.insert(state, id);
                        id
                    };
                    work.queue.push(Reverse((
                        cost.saturating_add(heuristic(state)?),
                        cost,
                        next_id,
                    )));
                }
                Ok(())
            })?;
        }
        Err(Error::NoRoute)
    }
}
