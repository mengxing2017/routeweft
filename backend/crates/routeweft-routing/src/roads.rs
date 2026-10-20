use crate::{Error, SearchBudget};
use routeweft_dataset::{Adjacency, Graph, LoadOptions, Manifest, Point};
use routeweft_domain::model::TravelProfile;
use rstar::{RTree, RTreeObject, AABB};
use serde::Serialize;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, VecDeque};
use std::path::Path;
const R: f64 = 6_371_008.8;
fn distance(a: Point, b: Point) -> f64 {
    let h = ((b.lat - a.lat).to_radians() / 2.).sin().powi(2)
        + a.lat.to_radians().cos()
            * b.lat.to_radians().cos()
            * ((b.lon - a.lon).to_radians() / 2.).sin().powi(2);
    R * 2. * h.clamp(0., 1.).sqrt().asin()
}
#[derive(Clone)]
struct Indexed {
    id: usize,
    envelope: AABB<[f64; 2]>,
}
impl RTreeObject for Indexed {
    type Envelope = AABB<[f64; 2]>;
    fn envelope(&self) -> Self::Envelope {
        self.envelope
    }
}
#[derive(Default)]
struct Trie {
    next: BTreeMap<usize, usize>,
    fail: usize,
    terminal: bool,
}
struct Restrictions {
    nodes: Vec<Trie>,
}
impl Restrictions {
    fn new(sequences: &[Vec<usize>]) -> Self {
        let mut nodes = vec![Trie::default()];
        for seq in sequences {
            let mut state = 0;
            for &edge in seq {
                state = if let Some(&next) = nodes[state].next.get(&edge) {
                    next
                } else {
                    let next = nodes.len();
                    nodes.push(Trie::default());
                    nodes[state].next.insert(edge, next);
                    next
                };
            }
            nodes[state].terminal = true;
        }
        let mut queue: VecDeque<_> = nodes[0].next.values().copied().collect();
        while let Some(v) = queue.pop_front() {
            let children: Vec<_> = nodes[v].next.iter().map(|(&e, &n)| (e, n)).collect();
            for (edge, next) in children {
                let mut f = nodes[v].fail;
                while f != 0 && !nodes[f].next.contains_key(&edge) {
                    f = nodes[f].fail;
                }
                nodes[next].fail = nodes[f].next.get(&edge).copied().unwrap_or(0);
                nodes[next].terminal |= nodes[nodes[next].fail].terminal;
                queue.push_back(next);
            }
        }
        Self { nodes }
    }
    fn step(&self, mut state: usize, edge: usize) -> Option<usize> {
        while state != 0 && !self.nodes[state].next.contains_key(&edge) {
            state = self.nodes[state].fail;
        }
        state = self.nodes[state].next.get(&edge).copied().unwrap_or(0);
        (!self.nodes[state].terminal).then_some(state)
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct Nearest {
    pub edge: usize,
    pub point: Point,
    pub fraction: f64,
    pub distance_m: f64,
    pub from: usize,
    pub to: usize,
}
#[derive(Clone, Copy)]
pub struct QueryOptions {
    pub profile: TravelProfile,
    pub radius_m: f64,
    pub candidates: usize,
}
impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            profile: TravelProfile::Driving,
            radius_m: 1000.,
            candidates: 8,
        }
    }
}
#[derive(Debug, Serialize)]
pub struct Leg {
    pub distance_m: f64,
    pub duration_s: f64,
    pub geometry_start: usize,
    pub geometry_end: usize,
}
#[derive(Debug, Serialize)]
pub struct Step {
    pub maneuver: String,
    pub name: String,
    pub distance_m: f64,
    pub duration_s: f64,
    pub geometry_start: usize,
    pub geometry_end: usize,
    pub exit: Option<usize>,
}
#[derive(Debug, Serialize)]
pub struct RouteResult {
    pub dataset_id: String,
    pub profile: String,
    pub format: u32,
    pub distance_m: f64,
    pub duration_s: f64,
    pub geometry: Vec<Point>,
    pub legs: Vec<Leg>,
    pub steps: Vec<Step>,
    pub snapped: Vec<Point>,
}
/// Immutable versioned dataset, spatial index and restriction automaton.
/// Each query owns its queue, overlay and cancellation/working-state budget.
pub struct Engine {
    pub(crate) graph: Graph,
    manifest: Manifest,
    outgoing: Adjacency,
    index: RTree<Indexed>,
    cos_lat: f64,
    restrictions: Restrictions,
}
fn dataset_error(e: routeweft_dataset::Error) -> Error {
    match e {
        routeweft_dataset::Error::Io(_) => Error::Io,
        routeweft_dataset::Error::IncompatibleFormat => Error::IncompatibleFormat,
        _ => Error::InvalidDataset,
    }
}
impl Engine {
    pub fn open(path: &Path, options: LoadOptions) -> Result<Self, Error> {
        let (m, g) = routeweft_dataset::open(path, options).map_err(dataset_error)?;
        Self::from_graph(m, g)
    }
    pub fn from_graph(manifest: Manifest, graph: Graph) -> Result<Self, Error> {
        graph
            .validate(LoadOptions::default())
            .map_err(dataset_error)?;
        let min_lon = graph
            .nodes
            .iter()
            .map(|p| p.lon)
            .fold(f64::INFINITY, f64::min);
        let max_lon = graph
            .nodes
            .iter()
            .map(|p| p.lon)
            .fold(f64::NEG_INFINITY, f64::max);
        let min_lat = graph
            .nodes
            .iter()
            .map(|p| p.lat)
            .fold(f64::INFINITY, f64::min);
        let max_lat = graph
            .nodes
            .iter()
            .map(|p| p.lat)
            .fold(f64::NEG_INFINITY, f64::max);
        if max_lon - min_lon > 5. || max_lat - min_lat > 5. {
            return Err(Error::InvalidDataset);
        }
        let cos_lat = ((min_lat + max_lat) / 2.).to_radians().cos();
        let project = |p: Point| [p.lon.to_radians() * R * cos_lat, p.lat.to_radians() * R];
        let outgoing = Adjacency::new(graph.nodes.len(), &graph.edges).map_err(dataset_error)?;
        let mut entries = Vec::new();
        for (id, e) in graph.edges.iter().enumerate() {
            entries.push(Indexed {
                id,
                envelope: AABB::from_corners(
                    project(graph.nodes[e.from]),
                    project(graph.nodes[e.to]),
                ),
            });
        }
        let restrictions = Restrictions::new(&graph.forbidden);
        Ok(Self {
            graph,
            manifest,
            outgoing,
            index: RTree::bulk_load(entries),
            cos_lat,
            restrictions,
        })
    }
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    fn projected(&self, p: Point) -> [f64; 2] {
        [
            p.lon.to_radians() * R * self.cos_lat,
            p.lat.to_radians() * R,
        ]
    }
    fn nearest_work(
        &self,
        point: Point,
        options: QueryOptions,
        budget: &SearchBudget<'_>,
        work: &mut usize,
    ) -> Result<Vec<Nearest>, Error> {
        point.coordinate().map_err(|_| Error::InvalidInput)?;
        if point.lat.abs() > 85.
            || !options.radius_m.is_finite()
            || options.radius_m <= 0.
            || options.radius_m > 10_000.
            || options.candidates == 0
            || options.candidates > 16
        {
            return Err(Error::InvalidInput);
        }
        if options.profile != TravelProfile::Driving {
            return Err(Error::UnsupportedProfile);
        }
        budget.tick(work)?;
        let p = self.projected(point);
        let lat_margin = options.radius_m / R;
        let min_cos = (point.lat.to_radians().abs() + lat_margin).cos().max(0.01);
        let dx = options.radius_m * self.cos_lat / min_cos * 1.01;
        let dy = options.radius_m * 1.01;
        let envelope = AABB::from_corners([p[0] - dx, p[1] - dy], [p[0] + dx, p[1] + dy]);
        let mut matches = Vec::new();
        for item in self.index.locate_in_envelope_intersecting(&envelope) {
            budget.tick(work)?;
            let e = &self.graph.edges[item.id];
            let a = self.graph.nodes[e.from];
            let b = self.graph.nodes[e.to];
            let (fraction, _) =
                crate::geometry::project_point_on_segment(self.projected(a), self.projected(b), p)
                    .ok_or(Error::InvalidInput)?;
            let projected = Point {
                lon: a.lon + (b.lon - a.lon) * fraction,
                lat: a.lat + (b.lat - a.lat) * fraction,
            };
            let d = distance(point, projected);
            if d <= options.radius_m {
                matches.push(Nearest {
                    edge: item.id,
                    point: projected,
                    fraction,
                    distance_m: d,
                    from: e.from,
                    to: e.to,
                });
                matches.sort_by(|a, b| {
                    a.distance_m
                        .total_cmp(&b.distance_m)
                        .then(a.edge.cmp(&b.edge))
                });
                matches.truncate(options.candidates);
            }
        }
        if matches.is_empty() {
            Err(Error::NoSegment)
        } else {
            Ok(matches)
        }
    }
    pub fn nearest(
        &self,
        point: Point,
        options: QueryOptions,
        budget: &SearchBudget<'_>,
    ) -> Result<Vec<Nearest>, Error> {
        self.nearest_work(point, options, budget, &mut 0)
    }
    pub fn route(
        &self,
        points: &[Point],
        options: QueryOptions,
        budget: &SearchBudget<'_>,
    ) -> Result<RouteResult, Error> {
        if !(2..=10).contains(&points.len()) {
            return Err(Error::InvalidInput);
        }
        if budget.max_states == 0 || budget.max_queue == 0 || budget.max_geometry == 0 {
            return Err(Error::BudgetExceeded);
        }
        let mut work = 0;
        let mut matches = Vec::new();
        for &p in points {
            let mut near = self.nearest_work(p, options, budget, &mut work)?;
            let best = near[0].distance_m;
            near.retain(|v| v.distance_m <= best + 0.01);
            matches.push(near);
        }
        let overlay = Overlay::new(&self.graph, &matches);
        let mut distances: HashMap<State, u64> = HashMap::new();
        let mut previous: HashMap<State, (State, Traversal)> = HashMap::new();
        let mut queue = BinaryHeap::new();
        for m in &matches[0] {
            let vertex = overlay.vertex(&self.graph, m);
            let automaton = if m.fraction > 1e-10 && m.fraction < 1. - 1e-10 {
                self.restrictions
                    .step(0, m.edge)
                    .ok_or(Error::InvalidDataset)?
            } else {
                0
            };
            let mut state = State {
                vertex,
                automaton,
                stage: 1,
            };
            state.stage = overlay.advance(vertex, state.stage);
            budget.tick(&mut work)?;
            if !distances.contains_key(&state) {
                if distances.len() >= budget.max_states || queue.len() >= budget.max_queue {
                    return Err(Error::BudgetExceeded);
                }
                distances.insert(state, 0);
                queue.push(Reverse((0, state)));
            }
        }
        while let Some(Reverse((cost, state))) = queue.pop() {
            budget.tick(&mut work)?;
            if distances.get(&state) != Some(&cost) {
                continue;
            }
            if state.stage == points.len() {
                return self.recover(
                    state, cost, &previous, &overlay, &matches, budget, &mut work,
                );
            }
            let arcs = overlay.arcs(&self.graph, &self.outgoing, state.vertex);
            for arc in arcs {
                budget.tick(&mut work)?;
                let automaton = if arc.enter {
                    let Some(next) = self.restrictions.step(state.automaton, arc.edge) else {
                        continue;
                    };
                    next
                } else {
                    state.automaton
                };
                let next = State {
                    vertex: arc.to,
                    automaton,
                    stage: overlay.advance(arc.to, state.stage),
                };
                let duration = scaled(self.graph.edges[arc.edge].duration_ms, arc.end - arc.start)?;
                let next_cost = cost.checked_add(duration).ok_or(Error::CostOverflow)?;
                if distances.get(&next).is_none_or(|&old| next_cost < old) {
                    if (!distances.contains_key(&next) && distances.len() >= budget.max_states)
                        || queue.len() >= budget.max_queue
                    {
                        return Err(Error::BudgetExceeded);
                    }
                    distances.insert(next, next_cost);
                    previous.insert(
                        next,
                        (
                            state,
                            Traversal {
                                edge: arc.edge,
                                start: arc.start,
                                end: arc.end,
                            },
                        ),
                    );
                    queue.push(Reverse((next_cost, next)));
                }
            }
        }
        Err(Error::NoRoute)
    }
    #[allow(clippy::too_many_arguments)]
    fn recover(
        &self,
        mut cursor: State,
        cost: u64,
        previous: &HashMap<State, (State, Traversal)>,
        overlay: &Overlay,
        matches: &[Vec<Nearest>],
        budget: &SearchBudget<'_>,
        work: &mut usize,
    ) -> Result<RouteResult, Error> {
        let mut traversals = Vec::new();
        while let Some((parent, arc)) = previous.get(&cursor) {
            budget.tick(work)?;
            if traversals.len().saturating_add(1) >= budget.max_geometry {
                return Err(Error::BudgetExceeded);
            }
            traversals.push((*arc, parent.stage, cursor.stage));
            cursor = *parent;
        }
        traversals.reverse();
        let mut geometry = vec![overlay.point(&self.graph, cursor.vertex)];
        let mut snapped = vec![geometry[0]];
        let mut legs = Vec::new();
        let mut steps: Vec<Step> = Vec::new();
        let mut total_distance = 0_u64;
        let mut leg_distance = 0_u64;
        let mut leg_time = 0_u64;
        let mut leg_start = 0;
        let mut prev_edge = None;
        let mut roundabout_exit = 0;
        // Coincident waypoints consumed at the source produce explicit zero legs.
        for _ in 1..cursor.stage {
            legs.push(Leg {
                distance_m: 0.,
                duration_s: 0.,
                geometry_start: 0,
                geometry_end: 0,
            });
            snapped.push(geometry[0]);
        }
        for (arc, before, after) in traversals {
            budget.tick(work)?;
            let e = &self.graph.edges[arc.edge];
            let d = scaled(e.distance_mm, arc.end - arc.start)?;
            let t = scaled(e.duration_ms, arc.end - arc.start)?;
            total_distance = total_distance.checked_add(d).ok_or(Error::CostOverflow)?;
            leg_distance = leg_distance.checked_add(d).ok_or(Error::CostOverflow)?;
            leg_time = leg_time.checked_add(t).ok_or(Error::CostOverflow)?;
            let a = geometry.len() - 1;
            let end = interpolate(self.graph.nodes[e.from], self.graph.nodes[e.to], arc.end);
            geometry.push(end);
            let b = geometry.len() - 1;
            // Account for the outgoing-edge inspections performed by guidance.
            for _ in &self.outgoing[e.from] {
                budget.tick(work)?;
            }
            let maneuver = if let Some(prev) = prev_edge {
                self.maneuver(prev, arc.edge, &mut roundabout_exit)
            } else {
                "depart".into()
            };
            if let Some(last) = steps
                .last_mut()
                .filter(|last| maneuver == "straight" && last.name == e.name && !e.roundabout)
            {
                last.distance_m += d as f64 / 1000.;
                last.duration_s += t as f64 / 1000.;
                last.geometry_end = b;
            } else {
                steps.push(Step {
                    exit: if maneuver == "roundabout_exit" {
                        Some(roundabout_exit.max(1))
                    } else {
                        None
                    },
                    maneuver,
                    name: e.name.clone(),
                    distance_m: d as f64 / 1000.,
                    duration_s: t as f64 / 1000.,
                    geometry_start: a,
                    geometry_end: b,
                });
            }
            prev_edge = Some(arc.edge);
            for stage in before..after {
                legs.push(Leg {
                    distance_m: leg_distance as f64 / 1000.,
                    duration_s: leg_time as f64 / 1000.,
                    geometry_start: leg_start,
                    geometry_end: b,
                });
                snapped.push(end);
                leg_start = b;
                leg_distance = 0;
                leg_time = 0;
                let _ = stage;
            }
        }
        let last = geometry.len() - 1;
        if steps.is_empty() {
            steps.push(Step {
                maneuver: "depart".into(),
                name: String::new(),
                distance_m: 0.,
                duration_s: 0.,
                geometry_start: 0,
                geometry_end: 0,
                exit: None,
            });
        }
        steps.push(Step {
            maneuver: "arrive".into(),
            name: String::new(),
            distance_m: 0.,
            duration_s: 0.,
            geometry_start: last,
            geometry_end: last,
            exit: None,
        });
        if legs.len() != matches.len() - 1 {
            return Err(Error::InvalidDataset);
        }
        Ok(RouteResult {
            dataset_id: self.manifest.dataset_id.clone(),
            profile: self.manifest.profile.clone(),
            format: self.manifest.format,
            distance_m: total_distance as f64 / 1000.,
            duration_s: cost as f64 / 1000.,
            geometry,
            legs,
            steps,
            snapped,
        })
    }
    fn maneuver(&self, prev: usize, next: usize, exit: &mut usize) -> String {
        let a = &self.graph.edges[prev];
        let b = &self.graph.edges[next];
        if prev == next {
            return if a.roundabout {
                "roundabout_continue"
            } else {
                "straight"
            }
            .into();
        }
        if !a.roundabout && b.roundabout {
            *exit = 0;
            return "roundabout_enter".into();
        }
        if a.roundabout {
            let has_exit = self.outgoing[a.to]
                .iter()
                .any(|&id| !self.graph.edges[id].roundabout && self.graph.edges[id].to != a.from);
            if has_exit {
                *exit += 1;
            }
            if !b.roundabout {
                return "roundabout_exit".into();
            }
            return "roundabout_continue".into();
        }
        if a.to == b.to && a.from == b.from {
            return "straight".into();
        }
        if a.from == b.to {
            return "uturn".into();
        }
        if a.way == b.way
            && self.outgoing[a.to]
                .iter()
                .filter(|&&id| self.graph.edges[id].to != a.from)
                .count()
                <= 1
        {
            return "straight".into();
        }
        if a.class.ends_with("_link") && !b.class.ends_with("_link") {
            return "merge".into();
        }
        if !a.class.ends_with("_link") && b.class.ends_with("_link") {
            return "exit".into();
        }
        let p = self.projected(self.graph.nodes[a.from]);
        let q = self.projected(self.graph.nodes[a.to]);
        let r = self.projected(self.graph.nodes[b.to]);
        let incoming = (q[1] - p[1]).atan2(q[0] - p[0]);
        let outgoing = (r[1] - q[1]).atan2(r[0] - q[0]);
        let angle = (outgoing - incoming + std::f64::consts::PI)
            .rem_euclid(2. * std::f64::consts::PI)
            - std::f64::consts::PI;
        if angle.abs() > 150_f64.to_radians() {
            "uturn"
        } else if angle > 30_f64.to_radians() {
            "left"
        } else if angle < -30_f64.to_radians() {
            "right"
        } else {
            "straight"
        }
        .into()
    }
}
fn scaled(value: u64, fraction: f64) -> Result<u64, Error> {
    let v = (value as f64 * fraction).round();
    if !v.is_finite() || v < 0. || v >= u64::MAX as f64 {
        return Err(Error::CostOverflow);
    }
    Ok(v as u64)
}
fn interpolate(a: Point, b: Point, t: f64) -> Point {
    Point {
        lon: a.lon + (b.lon - a.lon) * t,
        lat: a.lat + (b.lat - a.lat) * t,
    }
}
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct State {
    vertex: usize,
    automaton: usize,
    stage: usize,
}
#[derive(Clone, Copy)]
struct Traversal {
    edge: usize,
    start: f64,
    end: f64,
}
struct Arc {
    to: usize,
    edge: usize,
    start: f64,
    end: f64,
    enter: bool,
}
struct Split {
    edge: usize,
    fraction: f64,
}
struct Overlay {
    base: usize,
    splits: Vec<Split>,
    by_edge: BTreeMap<usize, Vec<usize>>,
    marks: BTreeMap<usize, u16>,
}
impl Overlay {
    fn new(graph: &Graph, matches: &[Vec<Nearest>]) -> Self {
        let mut out = Self {
            base: graph.nodes.len(),
            splits: Vec::new(),
            by_edge: BTreeMap::new(),
            marks: BTreeMap::new(),
        };
        for (stage, list) in matches.iter().enumerate() {
            for m in list {
                let vertex = if m.fraction <= 1e-10 {
                    m.from
                } else if m.fraction >= 1. - 1e-10 {
                    m.to
                } else if let Some(index) = out
                    .splits
                    .iter()
                    .position(|s| s.edge == m.edge && (s.fraction - m.fraction).abs() < 1e-10)
                {
                    out.base + index
                } else {
                    let id = out.base + out.splits.len();
                    out.splits.push(Split {
                        edge: m.edge,
                        fraction: m.fraction,
                    });
                    out.by_edge.entry(m.edge).or_default().push(id);
                    id
                };
                *out.marks.entry(vertex).or_default() |= 1 << stage;
            }
        }
        for ids in out.by_edge.values_mut() {
            ids.sort_by(|&a, &b| {
                out.splits[a - out.base]
                    .fraction
                    .total_cmp(&out.splits[b - out.base].fraction)
            });
        }
        out
    }
    fn vertex(&self, _graph: &Graph, m: &Nearest) -> usize {
        if m.fraction <= 1e-10 {
            m.from
        } else if m.fraction >= 1. - 1e-10 {
            m.to
        } else {
            self.base
                + self
                    .splits
                    .iter()
                    .position(|s| s.edge == m.edge && (s.fraction - m.fraction).abs() < 1e-10)
                    .expect("overlay built from nearest candidates")
        }
    }
    fn advance(&self, vertex: usize, mut stage: usize) -> usize {
        let mask = self.marks.get(&vertex).copied().unwrap_or(0);
        while stage < 10 && mask & (1 << stage) != 0 {
            stage += 1;
        }
        stage
    }
    fn point(&self, g: &Graph, vertex: usize) -> Point {
        if vertex < self.base {
            g.nodes[vertex]
        } else {
            let s = &self.splits[vertex - self.base];
            let e = &g.edges[s.edge];
            interpolate(g.nodes[e.from], g.nodes[e.to], s.fraction)
        }
    }
    fn arcs(&self, g: &Graph, outgoing: &Adjacency, vertex: usize) -> Vec<Arc> {
        if vertex < self.base {
            outgoing[vertex]
                .iter()
                .map(|&edge| {
                    let first = self.by_edge.get(&edge).and_then(|v| v.first()).copied();
                    Arc {
                        to: first.unwrap_or(g.edges[edge].to),
                        edge,
                        start: 0.,
                        end: first
                            .map(|v| self.splits[v - self.base].fraction)
                            .unwrap_or(1.),
                        enter: true,
                    }
                })
                .collect()
        } else {
            let s = &self.splits[vertex - self.base];
            let ids = &self.by_edge[&s.edge];
            let next = ids
                .iter()
                .position(|&id| id == vertex)
                .and_then(|i| ids.get(i + 1))
                .copied();
            vec![Arc {
                to: next.unwrap_or(g.edges[s.edge].to),
                edge: s.edge,
                start: s.fraction,
                end: next
                    .map(|v| self.splits[v - self.base].fraction)
                    .unwrap_or(1.),
                enter: false,
            }]
        }
    }
}
