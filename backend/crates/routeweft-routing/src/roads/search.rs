//! Bidirectional search of the SAME directed query graph in both directions.
//! Reverse traversal enumerates automaton preimages, including at split edges;
//! stages are inverted through Overlay::advance rather than resetting at vias.
use super::*;
use std::collections::HashSet;

type Distances = HashMap<State, u64>;
type Parents = HashMap<State, (State, PathArc)>;
type Queue = BinaryHeap<Reverse<(u64, State)>>;

impl Engine {
    fn automata_at(&self, vertex: usize, overlay: &Overlay) -> std::borrow::Cow<'_, [usize]> {
        if vertex < overlay.base {
            return std::borrow::Cow::Borrowed(
                self.automata
                    .get(&vertex)
                    .map(Vec::as_slice)
                    .unwrap_or(&[0]),
            );
        }
        let edge = overlay.splits[vertex - overlay.base].edge;
        let mut states: Vec<_> = self
            .automata
            .get(&self.graph.edges[edge].from)
            .map(Vec::as_slice)
            .unwrap_or(&[0])
            .iter()
            .filter_map(|&a| self.restrictions.step(a, edge))
            .collect();
        states.sort_unstable();
        states.dedup();
        std::borrow::Cow::Owned(states)
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) fn search_accelerated(
        &self,
        matches: &[Vec<Nearest>],
        overlay: &Overlay,
        index: &Index,
        open: &[HashSet<usize>],
        ctx: &mut Context<'_, '_>,
    ) -> Result<RouteResult, Error> {
        let cuts = self
            .compression
            .cuts(&self.graph, &self.outgoing, &self.incoming, overlay);
        let mut d: [Distances; 2] = [HashMap::new(), HashMap::new()];
        let mut p: [Parents; 2] = [HashMap::new(), HashMap::new()];
        let mut q: [Queue; 2] = [BinaryHeap::new(), BinaryHeap::new()];
        for m in &matches[0] {
            let vertex = overlay.vertex(&self.graph, m);
            let automaton = if vertex >= overlay.base {
                self.restrictions
                    .step(0, m.edge)
                    .ok_or(Error::InvalidDataset)?
            } else {
                0
            };
            let s = State {
                vertex,
                automaton,
                stage: overlay.advance(vertex, 1),
            };
            if d[0].insert(s, 0).is_none() {
                q[0].push(Reverse((0, s)));
            }
        }
        for m in matches.last().unwrap() {
            let vertex = overlay.vertex(&self.graph, m);
            for automaton in self.automata_at(vertex, overlay).iter().copied() {
                let s = State {
                    vertex,
                    automaton,
                    stage: matches.len(),
                };
                if d[1].insert(s, 0).is_none() {
                    q[1].push(Reverse((0, s)));
                }
            }
        }
        ctx.capacity(d[0].len() + d[1].len(), q[0].len() + q[1].len())?;
        let mut best: Option<(u64, State)> = None;
        let mut turn = 0;
        loop {
            for side in 0..2 {
                while q[side]
                    .peek()
                    .is_some_and(|Reverse((c, s))| d[side].get(s) != Some(c))
                {
                    ctx.tick()?;
                    q[side].pop();
                }
            }
            let minima = [
                q[0].peek().map(|Reverse((c, _))| *c),
                q[1].peek().map(|Reverse((c, _))| *c),
            ];
            let (Some(a), Some(b)) = (minima[0], minima[1]) else {
                break;
            };
            if best.is_some_and(|(upper, _)| a as u128 + b as u128 >= upper as u128) {
                break;
            }
            // Expand the smaller frontier. Cost minima alone over-expand dense
            // urban cells when the other endpoint is in a sparse road network.
            // Each side still pops its own minimum; the lower-bound stopping
            // rule above is independent of this scheduling choice.
            let side = if q[0].len() == q[1].len() {
                turn ^= 1;
                turn
            } else {
                usize::from(q[1].len() < q[0].len())
            };
            let Reverse((cost, s)) = q[side].pop().unwrap();
            ctx.tick()?;
            ctx.diagnostics.settled_states += 1;
            if side == 0 {
                ctx.diagnostics.forward_settled += 1;
            } else {
                ctx.diagnostics.reverse_settled += 1;
            }
            if let Some(&other) = d[1 - side].get(&s) {
                if let Some(sum) = cost.checked_add(other) {
                    if best.is_none_or(|(old, _)| sum < old) {
                        best = Some((sum, s));
                    }
                }
            }
            if best.is_some_and(|(upper, _)| cost >= upper)
                || (side == 0 && s.stage == matches.len())
            {
                continue;
            }
            let arrival = p[side].get(&s).and_then(|(_, arc)| match arc {
                PathArc::Shortcut { level, from, .. } => Some((*level, index.cell(*level, *from))),
                _ => None,
            });
            self.query_arcs(
                s,
                side == 1,
                overlay,
                index,
                open,
                arrival,
                &cuts,
                ctx,
                |to, automaton, duration, arc, ctx| {
                    let next_stage = overlay.advance(to, s.stage);
                    let stages = if side == 0 {
                        next_stage..next_stage + 1
                    } else {
                        1..matches.len()
                    };
                    for stage in stages {
                        if side == 1 && overlay.advance(s.vertex, stage) != s.stage {
                            continue;
                        }
                        ctx.tick()?;
                        let next = State {
                            vertex: to,
                            automaton,
                            stage,
                        };
                        let cost = cost.checked_add(duration).ok_or(Error::CostOverflow)?;
                        if d[side].get(&next).is_none_or(|&old| cost < old) {
                            ctx.capacity(
                                d[0].len() + d[1].len() + usize::from(!d[side].contains_key(&next)),
                                q[0].len() + q[1].len() + 1,
                            )?;
                            d[side].insert(next, cost);
                            p[side].insert(next, (s, arc));
                            q[side].push(Reverse((cost, next)));
                            if let Some(&other) = d[1 - side].get(&next) {
                                if let Some(sum) = cost.checked_add(other) {
                                    if best.is_none_or(|(old, _)| sum < old) {
                                        best = Some((sum, next));
                                    }
                                }
                            }
                        }
                    }
                    Ok(())
                },
            )?;
        }
        let (cost, meeting) = best.ok_or(Error::NoRoute)?;
        // Build one forward predecessor chain for the existing geometry/guidance
        // recovery, retaining restriction history across every waypoint.
        let mut previous = HashMap::new();
        let mut cursor = meeting;
        while let Some(&(parent, arc)) = p[0].get(&cursor) {
            ctx.tick()?;
            previous.insert(cursor, (parent, arc));
            cursor = parent;
        }
        cursor = meeting;
        while let Some(&(next, arc)) = p[1].get(&cursor) {
            ctx.tick()?;
            previous.insert(next, (cursor, arc));
            cursor = next;
        }
        drop(d);
        drop(p);
        drop(q);
        ctx.phase("recover");
        self.recover(cursor, cost, &previous, overlay, matches, ctx)
    }
    #[allow(clippy::too_many_arguments)]
    fn query_arcs(
        &self,
        s: State,
        reverse: bool,
        overlay: &Overlay,
        index: &Index,
        open: &[HashSet<usize>],
        arrival: Option<(usize, usize)>,
        cuts: &compression::Cuts,
        ctx: &mut Context<'_, '_>,
        mut visit: impl FnMut(usize, usize, u64, PathArc, &mut Context<'_, '_>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let node = if s.vertex < overlay.base {
            index.node(s.vertex, s.automaton)
        } else {
            None
        };
        if let Some(node) = node {
            let level = index.query_level(node, open);
            if !reverse {
                if let Some(level) = level {
                    let skip = arrival == Some((level, index.cell(level, node)));
                    if skip {
                        ctx.diagnostics.clique_skips += 1;
                    }
                    return index.visit(node, Some(level), false, skip, |link| {
                        ctx.tick()?;
                        let (v, a) = index.state(link.to);
                        let arc = if let Some(level) = link.level {
                            ctx.diagnostics.relaxed_shortcuts += 1;
                            PathArc::Shortcut {
                                from: node,
                                to: link.to,
                                level,
                                cost: link.cost,
                            }
                        } else {
                            ctx.diagnostics.scanned_edges += 1;
                            PathArc::Base(Traversal {
                                edge: index.original(link.edge),
                                start: 0.,
                                end: 1.,
                            })
                        };
                        visit(v, a, link.cost, arc, ctx)
                    });
                }
            } else {
                // The transpose cannot just use query_level(node): incoming
                // shortcuts may originate at a different query level.
                for l in 0..open.len() {
                    if open[l].contains(&index.cell(l, node)) {
                        continue;
                    }
                    if arrival == Some((l, index.cell(l, node))) {
                        ctx.diagnostics.clique_skips += 1;
                        continue;
                    }
                    index.visit(node, Some(l), true, false, |link| {
                        ctx.tick()?;
                        if link.level.is_none()
                            || !index.active(link.to, open)
                            || index.query_level(link.to, open) != Some(l)
                        {
                            return Ok(());
                        }
                        let (v, a) = index.state(link.to);
                        ctx.diagnostics.relaxed_shortcuts += 1;
                        visit(
                            v,
                            a,
                            link.cost,
                            PathArc::Shortcut {
                                from: link.to,
                                to: node,
                                level: l,
                                cost: link.cost,
                            },
                            ctx,
                        )
                    })?;
                }
            }
        }
        let mut base = |arc: Arc| -> Result<(), Error> {
            ctx.tick()?;
            ctx.diagnostics.scanned_edges += 1;
            let jump = if s.vertex < overlay.base
                && arc.to < overlay.base
                && arc.start == 0.
                && arc.end == 1.
                && s.automaton == 0
            {
                self.compression.jump(arc.edge, reverse, cuts)
            } else {
                None
            };
            let (to, duration, path) = if let Some((chain, start, end)) = jump {
                let ids = self.compression.range(chain, start, end);
                let to = if reverse {
                    self.graph.edges[ids[0]].from
                } else {
                    self.graph.edges[*ids.last().unwrap()].to
                };
                ctx.diagnostics.compressed_edges += end - start - 1;
                (
                    to,
                    self.compression.cost(&self.graph, chain, start, end)?,
                    PathArc::Chain { chain, start, end },
                )
            } else {
                (
                    arc.to,
                    scaled(self.graph.edges[arc.edge].duration_ms, arc.end - arc.start)?,
                    PathArc::Base(Traversal {
                        edge: arc.edge,
                        start: arc.start,
                        end: arc.end,
                    }),
                )
            };
            if !reverse {
                if let Some(a) = if arc.enter {
                    self.restrictions.step(s.automaton, arc.edge)
                } else {
                    Some(s.automaton)
                } {
                    visit(to, a, duration, path, ctx)?;
                }
            } else {
                for a in self.automata_at(to, overlay).iter().copied() {
                    ctx.tick()?;
                    if jump.is_some() && a != 0 {
                        continue;
                    }
                    if (if arc.enter {
                        self.restrictions.step(a, arc.edge)
                    } else {
                        Some(a)
                    }) != Some(s.automaton)
                    {
                        continue;
                    }
                    if let Some(from) = (to < overlay.base).then(|| index.node(to, a)).flatten() {
                        if !index.active(from, open) {
                            continue;
                        }
                        if index.query_level(from, open).is_some_and(|l| {
                            node.is_some_and(|target| index.cell(l, from) == index.cell(l, target))
                        }) {
                            continue;
                        }
                    }
                    visit(to, a, duration, path, ctx)?;
                }
            }
            Ok(())
        };
        if reverse {
            overlay.visit_incoming(&self.graph, &self.incoming, s.vertex, &mut base)
        } else {
            overlay.visit_arcs(&self.graph, &self.outgoing, s.vertex, &mut base)
        }
    }
}
