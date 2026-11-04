mod components;
mod compression;
mod paged;
mod recovery;
mod sampling;
pub use paged::{PagedEngine, PagedOptions, PagedResourcePlan};
mod search;
use crate::acceleration::{AccelerationInfo, CustomizationOptions, Index, Link};
use crate::diagnostics::Context;
use crate::{Error, QueryOutcome, RoutingAlgorithm, SearchBudget};
use routeweft_dataset::{Adjacency, Graph, LoadOptions, Manifest, Point};
use routeweft_domain::model::TravelProfile;
use rstar::{RTree, RTreeObject, AABB};
pub use sampling::{RouteSample, RouteSamples, SamplingOptions, SAMPLER_VERSION};
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
pub(crate) struct Trie {
    pub(crate) next: BTreeMap<usize, usize>,
    fail: usize,
    pub(crate) terminal: bool,
}
pub(crate) struct Restrictions {
    pub(crate) nodes: Vec<Trie>,
}
impl Restrictions {
    pub(crate) fn new(sequences: &[Vec<usize>]) -> Self {
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
    pub(crate) fn step(&self, mut state: usize, edge: usize) -> Option<usize> {
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
/// Physical directed connectivity; turn-state reachability still needs queries.
#[derive(Debug, Serialize)]
pub struct CoverageInfo {
    pub nodes: usize,
    pub edges: usize,
    pub directed_distance_m: f64,
    pub weak_components: usize,
    pub largest_weak_nodes: usize,
    pub strong_components: usize,
    pub largest_strong_nodes: usize,
    pub excluded_way_edges: usize,
}
/// Immutable versioned dataset, spatial index and restriction automaton.
/// Each query owns its queue, overlay and cancellation/working-state budget.
pub struct Engine {
    pub(crate) graph: Graph,
    manifest: Manifest,
    outgoing: Adjacency,
    incoming: crate::topology::Rows,
    automata: HashMap<usize, Vec<usize>>,
    components: components::Components,
    compression: compression::Compression,
    index: RTree<Indexed>,
    cos_lat: f64,
    local_projection: bool,
    restrictions: Restrictions,
    acceleration: Option<Index>,
    load_diagnostics: LoadDiagnostics,
}
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct LoadDiagnostics {
    pub paged: Option<routeweft_dataset::paged::PagedOpenDiagnostics>,
    pub dataset_ms: f64,
    pub acceleration_ms: f64,
    pub index_ms: f64,
    pub adjacency_ms: f64,
    pub restrictions_ms: f64,
    pub spatial_index_ms: f64,
    pub components_ms: f64,
    pub compression_ms: f64,
    pub weak_components: usize,
    pub strong_components: usize,
}
pub(crate) fn dataset_error(e: routeweft_dataset::Error) -> Error {
    match e {
        routeweft_dataset::Error::Io(_) => Error::Io,
        routeweft_dataset::Error::IncompatibleFormat => Error::IncompatibleFormat,
        routeweft_dataset::Error::ResourceLimit(_) | routeweft_dataset::Error::LimitExceeded => {
            Error::BudgetExceeded
        }
        _ => Error::InvalidDataset,
    }
}
impl Engine {
    pub fn open(path: &Path, options: LoadOptions) -> Result<Self, Error> {
        Self::open_with_capacity(path, options, routeweft_dataset::GraphCapacity::City)
    }
    pub fn open_with_capacity(
        path: &Path,
        mut options: LoadOptions,
        capacity: routeweft_dataset::GraphCapacity,
    ) -> Result<Self, Error> {
        let started = std::time::Instant::now();
        let caps = capacity.limits();
        options.max_nodes = options.max_nodes.min(caps.max_nodes);
        options.max_edges = options.max_edges.min(caps.max_edges);
        options.max_bytes = options.max_bytes.min(caps.max_bytes);
        let manifest = routeweft_dataset::inspect_manifest(path, options).map_err(dataset_error)?;
        routeweft_dataset::check_resident_budget(&manifest, options, true)
            .map_err(dataset_error)?;
        let (m, g) = routeweft_dataset::open(path, options).map_err(dataset_error)?;
        let dataset_ms = started.elapsed().as_secs_f64() * 1000.;
        let started = std::time::Instant::now();
        let acceleration = crate::acceleration::load(path, &m, &g, options)?;
        let acceleration_ms = started.elapsed().as_secs_f64() * 1000.;
        let started = std::time::Instant::now();
        // Dataset::open has already authenticated and validated this graph.
        let mut engine = Self::from_validated_graph(m, g, options, capacity)?;
        if let Some(index) = acceleration {
            engine.load_diagnostics.compression_ms = engine.install_acceleration(index);
        }
        engine.load_diagnostics.dataset_ms = dataset_ms;
        engine.load_diagnostics.acceleration_ms = acceleration_ms;
        engine.load_diagnostics.index_ms = started.elapsed().as_secs_f64() * 1000.;
        Ok(engine)
    }
    pub fn from_graph(manifest: Manifest, graph: Graph) -> Result<Self, Error> {
        graph
            .validate(LoadOptions::default())
            .map_err(dataset_error)?;
        Self::from_validated_graph(
            manifest,
            graph,
            LoadOptions::default(),
            routeweft_dataset::GraphCapacity::City,
        )
    }
    /// Private entry point: callers must validate graph semantics first.
    fn from_validated_graph(
        manifest: Manifest,
        graph: Graph,
        options: LoadOptions,
        capacity: routeweft_dataset::GraphCapacity,
    ) -> Result<Self, Error> {
        let index_started = std::time::Instant::now();
        let mut actual = manifest.clone();
        actual.nodes = graph.nodes.len();
        actual.edges = graph.edges.len();
        actual.restrictions = graph.forbidden.len();
        routeweft_dataset::check_resident_budget(&actual, options, true).map_err(dataset_error)?;
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
        if max_lon - min_lon > capacity.max_span_degrees()
            || max_lat - min_lat > capacity.max_span_degrees()
        {
            return Err(Error::InvalidDataset);
        }
        let cos_lat = ((min_lat + max_lat) / 2.).to_radians().cos();
        let project = |p: Point| [p.lon.to_radians() * R * cos_lat, p.lat.to_radians() * R];
        let started = std::time::Instant::now();
        let incoming =
            crate::topology::Rows::new(graph.nodes.len(), graph.edges.iter().map(|e| e.to));
        let outgoing = Adjacency::with_limits(graph.nodes.len(), &graph.edges, options)
            .map_err(dataset_error)?;
        let adjacency_ms = started.elapsed().as_secs_f64() * 1000.;
        let started = std::time::Instant::now();
        let restrictions = Restrictions::new(&graph.forbidden);
        let mut automata: HashMap<usize, Vec<usize>> = HashMap::new();
        for node in &restrictions.nodes {
            for (&e, &a) in &node.next {
                if !restrictions.nodes[a].terminal {
                    automata
                        .entry(graph.edges[e].to)
                        .or_insert_with(|| vec![0])
                        .push(a);
                }
            }
        }
        let restrictions_ms = started.elapsed().as_secs_f64() * 1000.;
        let started = std::time::Instant::now();
        let mut entries = Vec::with_capacity(graph.edges.len());
        for (id, e) in graph.edges.iter().enumerate() {
            entries.push(Indexed {
                id,
                envelope: AABB::from_corners(
                    project(graph.nodes[e.from]),
                    project(graph.nodes[e.to]),
                ),
            });
        }
        let index = RTree::bulk_load(entries);
        let spatial_index_ms = started.elapsed().as_secs_f64() * 1000.;
        let started = std::time::Instant::now();
        let components = components::Components::new(&graph, &outgoing, &incoming);
        let components_ms = started.elapsed().as_secs_f64() * 1000.;
        let load_diagnostics = LoadDiagnostics {
            index_ms: index_started.elapsed().as_secs_f64() * 1000.,
            adjacency_ms,
            restrictions_ms,
            spatial_index_ms,
            components_ms,
            weak_components: components.size.len(),
            strong_components: components.strong_count,
            ..Default::default()
        };
        Ok(Self {
            graph,
            manifest,
            outgoing,
            incoming,
            automata,
            components,
            // Only MLD search emits chain arcs. Dijkstra needs no compression.
            compression: compression::Compression::default(),
            index,
            cos_lat,
            local_projection: capacity == routeweft_dataset::GraphCapacity::Regional,
            restrictions,
            acceleration: None,
            load_diagnostics,
        })
    }
    /// Build an in-memory metric for embedded use and differential tests.
    /// Persistent deployments should run the offline customize command once.
    pub fn customize(&mut self, options: CustomizationOptions) -> Result<AccelerationInfo, Error> {
        let index = Index::build(&self.graph, &self.outgoing, options)?;
        let info = index.info();
        self.install_acceleration(index);
        Ok(info)
    }
    fn install_acceleration(&mut self, index: Index) -> f64 {
        let started = std::time::Instant::now();
        if self.acceleration.is_none() {
            self.compression =
                compression::Compression::new(&self.graph, &self.outgoing, &self.incoming);
        }
        self.compression.set_boundaries(&self.graph, &index);
        self.acceleration = Some(index);
        started.elapsed().as_secs_f64() * 1000.
    }
    /// `excluded` must be strictly sorted. Coverage inspection never changes routing.
    pub fn coverage(&self, excluded: &[i64]) -> Result<CoverageInfo, Error> {
        if excluded.windows(2).any(|w| w[0] >= w[1]) {
            return Err(Error::InvalidInput);
        }
        let mut distance = 0u64;
        let mut remaining = 0;
        for edge in &self.graph.edges {
            distance = distance
                .checked_add(edge.distance_mm)
                .ok_or(Error::CostOverflow)?;
            remaining += excluded.binary_search(&edge.way).is_ok() as usize;
        }
        Ok(CoverageInfo {
            nodes: self.graph.nodes.len(),
            edges: self.graph.edges.len(),
            directed_distance_m: distance as f64 / 1000.,
            weak_components: self.components.size.len(),
            largest_weak_nodes: self
                .components
                .size
                .get(self.components.largest as usize)
                .copied()
                .unwrap_or(0),
            strong_components: self.components.strong_count,
            largest_strong_nodes: self.components.largest_strong_size,
            excluded_way_edges: remaining,
        })
    }
    pub fn acceleration_info(&self) -> Option<AccelerationInfo> {
        self.acceleration.as_ref().map(Index::info)
    }
    pub fn load_diagnostics(&self) -> LoadDiagnostics {
        self.load_diagnostics
    }
    /// SHA-256 of the complete loaded routing.mld file, including its checksum.
    /// None for an absent artifact or a metric customized only in memory.
    pub fn acceleration_sha256(&self) -> Option<&str> {
        self.acceleration.as_ref()?.artifact_sha256.as_deref()
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
        ctx: &mut Context<'_, '_>,
    ) -> Result<Vec<Nearest>, Error> {
        self.nearest_component(point, options, None, ctx)
    }
    fn nearest_component(
        &self,
        point: Point,
        options: QueryOptions,
        component: Option<u32>,
        ctx: &mut Context<'_, '_>,
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
        ctx.tick()?;
        let p = self.projected(point);
        let lat_margin = options.radius_m / R;
        let min_cos = (point.lat.to_radians().abs() + lat_margin).cos().max(0.01);
        let dx = options.radius_m * self.cos_lat / min_cos * 1.01;
        let dy = options.radius_m * 1.01;
        let envelope = AABB::from_corners([p[0] - dx, p[1] - dy], [p[0] + dx, p[1] + dy]);
        let mut matches = Vec::new();
        for item in self.index.locate_in_envelope_intersecting(&envelope) {
            ctx.tick()?;
            let e = &self.graph.edges[item.id];
            if component.is_some_and(|c| self.components.weak[e.from] != c) {
                continue;
            }
            let a = self.graph.nodes[e.from];
            let b = self.graph.nodes[e.to];
            // Regional snapping uses the query latitude, avoiding distortion
            // from a single map-centre latitude across the larger extent. The
            // R-tree still uses the common projection and conservative envelope.
            let snap = |q: Point| {
                if self.local_projection {
                    [
                        q.lon.to_radians() * R * point.lat.to_radians().cos(),
                        q.lat.to_radians() * R,
                    ]
                } else {
                    self.projected(q)
                }
            };
            let (fraction, _) =
                crate::geometry::project_point_on_segment(snap(a), snap(b), snap(point))
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
        self.nearest_work(point, options, &mut Context::new(budget, false))
    }
    pub fn nearest_detailed(
        &self,
        point: Point,
        options: QueryOptions,
        budget: &SearchBudget<'_>,
    ) -> (Result<Vec<Nearest>, Error>, crate::QueryDiagnostics) {
        let mut ctx = Context::new(budget, false);
        let result = self.nearest_work(point, options, &mut ctx);
        (result, ctx.finish())
    }
    pub fn route(
        &self,
        points: &[Point],
        options: QueryOptions,
        budget: &SearchBudget<'_>,
    ) -> Result<RouteResult, Error> {
        self.route_detailed(points, options, budget, RoutingAlgorithm::Auto)
            .result
    }
    pub fn route_detailed(
        &self,
        points: &[Point],
        options: QueryOptions,
        budget: &SearchBudget<'_>,
        algorithm: RoutingAlgorithm,
    ) -> QueryOutcome {
        let acceleration = if algorithm == RoutingAlgorithm::Dijkstra {
            None
        } else {
            self.acceleration.as_ref()
        };
        let mut ctx = Context::new(budget, acceleration.is_some());
        let result = self.route_inner(points, options, &mut ctx, acceleration);
        QueryOutcome {
            result,
            diagnostics: ctx.finish(),
        }
    }
    fn route_inner(
        &self,
        points: &[Point],
        options: QueryOptions,
        ctx: &mut Context<'_, '_>,
        acceleration: Option<&Index>,
    ) -> Result<RouteResult, Error> {
        if !(2..=10).contains(&points.len()) {
            return Err(Error::InvalidInput);
        }
        ctx.capacity(1, 1)?;
        if ctx.budget.max_geometry == 0 {
            return Err(ctx.fail("geometry", 1, 0));
        }
        let mut matches = Vec::new();
        for &p in points {
            let mut near = self.nearest_work(p, options, ctx)?;
            let best = near[0].distance_m;
            near.retain(|v| v.distance_m <= best + 0.01);
            matches.push(near);
        }
        // Keep a nearby closed component when ALL waypoints can use it;
        // otherwise seek a main-component alternative independently of top-k.
        let common = matches[0]
            .iter()
            .map(|m| self.components.weak[m.from])
            .any(|c| {
                matches
                    .iter()
                    .all(|list| list.iter().any(|m| self.components.weak[m.from] == c))
            });
        if !common {
            for (i, list) in matches.iter_mut().enumerate() {
                let c = self.components.weak[list[0].from];
                if c != self.components.largest && self.components.size[c as usize] < 1000 {
                    match self.nearest_component(
                        points[i],
                        options,
                        Some(self.components.largest),
                        ctx,
                    ) {
                        Ok(mut alternatives) => {
                            let best = alternatives[0].distance_m;
                            alternatives.retain(|m| m.distance_m <= best + 0.01);
                            *list = alternatives;
                            ctx.diagnostics.snap_fallbacks += 1;
                        }
                        Err(Error::NoSegment) => {}
                        Err(e) => return Err(e),
                    }
                }
            }
        }
        self.route_matched(&matches, ctx, acceleration)
    }
    fn route_matched(
        &self,
        matches: &[Vec<Nearest>],
        ctx: &mut Context<'_, '_>,
        acceleration: Option<&Index>,
    ) -> Result<RouteResult, Error> {
        for pair in matches.windows(2) {
            if !pair[0].iter().any(|a| {
                pair[1]
                    .iter()
                    .any(|b| self.components.weak[a.from] == self.components.weak[b.from])
            }) {
                return Err(Error::NoRoute);
            }
        }
        let open = acceleration.map(|index| {
            index.open_cells(
                matches
                    .iter()
                    .flatten()
                    .flat_map(|m| [m.from, m.to].into_iter()),
            )
        });
        let overlay = Overlay::new(&self.graph, matches);
        ctx.phase("search");
        if let Some(index) = acceleration {
            return self.search_accelerated(matches, &overlay, index, open.as_ref().unwrap(), ctx);
        }
        let mut distances: HashMap<State, u64> = HashMap::new();
        let mut previous: HashMap<State, (State, PathArc)> = HashMap::new();
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
            ctx.tick()?;
            if !distances.contains_key(&state) {
                ctx.capacity(distances.len() + 1, queue.len() + 1)?;
                distances.insert(state, 0);
                queue.push(Reverse((0, state)));
            }
        }
        while let Some(Reverse((cost, state))) = queue.pop() {
            ctx.tick()?;
            if distances.get(&state) != Some(&cost) {
                continue;
            }
            if state.stage == matches.len() {
                ctx.phase("recover");
                return self.recover(state, cost, &previous, &overlay, matches, ctx);
            }
            ctx.diagnostics.settled_states += 1;
            let mut relax = |to: usize,
                             duration: u64,
                             path_arc: PathArc,
                             automaton: Option<usize>|
             -> Result<(), Error> {
                ctx.tick()?;
                match path_arc {
                    PathArc::Base(_) => ctx.diagnostics.scanned_edges += 1,
                    PathArc::Shortcut { .. } => ctx.diagnostics.relaxed_shortcuts += 1,
                    PathArc::Chain { .. } => unreachable!("baseline uses original edges"),
                }
                let Some(automaton) = automaton else {
                    return Ok(());
                };
                let next = State {
                    vertex: to,
                    automaton,
                    stage: overlay.advance(to, state.stage),
                };
                let next_cost = cost.checked_add(duration).ok_or(Error::CostOverflow)?;
                if distances.get(&next).is_none_or(|&old| next_cost < old) {
                    ctx.capacity(
                        distances.len() + usize::from(!distances.contains_key(&next)),
                        queue.len() + 1,
                    )?;
                    distances.insert(next, next_cost);
                    previous.insert(next, (state, path_arc));
                    queue.push(Reverse((next_cost, next)));
                }
                Ok(())
            };
            overlay.visit_arcs(&self.graph, &self.outgoing, state.vertex, |arc| {
                let automaton = if arc.enter {
                    self.restrictions.step(state.automaton, arc.edge)
                } else {
                    Some(state.automaton)
                };
                let duration = if automaton.is_some() {
                    scaled(self.graph.edges[arc.edge].duration_ms, arc.end - arc.start)?
                } else {
                    0
                };
                relax(
                    arc.to,
                    duration,
                    PathArc::Base(Traversal {
                        edge: arc.edge,
                        start: arc.start,
                        end: arc.end,
                    }),
                    automaton,
                )
            })?;
        }
        Err(Error::NoRoute)
    }
    #[allow(clippy::too_many_arguments)]
    fn recover(
        &self,
        mut cursor: State,
        cost: u64,
        previous: &HashMap<State, (State, PathArc)>,
        overlay: &Overlay,
        matches: &[Vec<Nearest>],
        ctx: &mut Context<'_, '_>,
    ) -> Result<RouteResult, Error> {
        let mut traversals = Vec::new();
        while let Some((parent, arc)) = previous.get(&cursor) {
            ctx.tick()?;
            match *arc {
                PathArc::Base(arc) => traversals.push((arc, parent.stage, cursor.stage)),
                PathArc::Chain { chain, start, end } => {
                    for (i, &edge) in self
                        .compression
                        .range(chain, start, end)
                        .iter()
                        .rev()
                        .enumerate()
                    {
                        ctx.tick()?;
                        if traversals.len() + 1 >= ctx.budget.max_geometry {
                            return Err(ctx.fail(
                                "geometry",
                                traversals.len() + 2,
                                ctx.budget.max_geometry,
                            ));
                        }
                        traversals.push((
                            Traversal {
                                edge,
                                start: 0.,
                                end: 1.,
                            },
                            parent.stage,
                            if i == 0 { cursor.stage } else { parent.stage },
                        ));
                    }
                }
                PathArc::Shortcut {
                    from,
                    to,
                    level,
                    cost,
                } => {
                    let index = self.acceleration.as_ref().ok_or(Error::InvalidDataset)?;
                    let edges = index.unpack(
                        from,
                        Link {
                            to,
                            cost,
                            edge: 0,
                            level: Some(level),
                        },
                        ctx,
                        ctx.budget.max_geometry.saturating_sub(traversals.len() + 1),
                    )?;
                    // Parent-chain recovery runs backwards. Preserve the order
                    // when the entire original-edge sequence is reversed below.
                    for (i, edge) in edges.into_iter().rev().enumerate() {
                        traversals.push((
                            Traversal {
                                edge,
                                start: 0.,
                                end: 1.,
                            },
                            parent.stage,
                            if i == 0 { cursor.stage } else { parent.stage },
                        ));
                    }
                }
            }
            if traversals.len().saturating_add(1) > ctx.budget.max_geometry {
                return Err(ctx.fail("geometry", traversals.len() + 1, ctx.budget.max_geometry));
            }
            cursor = *parent;
        }
        traversals.reverse();
        recovery::recover_path(
            self,
            &self.restrictions,
            cursor.vertex,
            cursor.stage,
            cost,
            traversals,
            matches.len(),
            overlay,
            ctx,
        )
    }
}

fn scaled(value: u64, fraction: f64) -> Result<u64, Error> {
    // Preserve full-edge integer weights (including values above 2^53) so
    // customized metrics and the baseline search use exactly the same costs.
    if fraction == 1. {
        return Ok(value);
    }
    if fraction == 0. {
        return Ok(0);
    }
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
enum PathArc {
    Base(Traversal),
    Chain {
        chain: u32,
        start: usize,
        end: usize,
    },
    Shortcut {
        from: usize,
        to: usize,
        level: usize,
        cost: u64,
    },
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
        Self::with_base(graph.nodes.len(), matches)
    }
    fn with_base(base: usize, matches: &[Vec<Nearest>]) -> Self {
        let mut out = Self {
            base,
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
        self.vertex_for(m)
    }
    fn vertex_for(&self, m: &Nearest) -> usize {
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
    fn visit_incoming(
        &self,
        g: &Graph,
        incoming: &crate::topology::Rows,
        vertex: usize,
        mut visit: impl FnMut(Arc) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let mut emit = |edge: usize, end: f64| {
            let prev = self.by_edge.get(&edge).and_then(|ids| {
                ids.iter()
                    .rev()
                    .copied()
                    .find(|&v| self.splits[v - self.base].fraction < end - 1e-12)
            });
            visit(Arc {
                to: prev.unwrap_or(g.edges[edge].from),
                edge,
                start: prev
                    .map(|v| self.splits[v - self.base].fraction)
                    .unwrap_or(0.),
                end,
                enter: prev.is_none(),
            })
        };
        if vertex < self.base {
            for &edge in &incoming[vertex] {
                emit(edge, 1.)?;
            }
        } else {
            let split = &self.splits[vertex - self.base];
            emit(split.edge, split.fraction)?;
        }
        Ok(())
    }
    fn visit_arcs(
        &self,
        g: &Graph,
        outgoing: &Adjacency,
        vertex: usize,
        mut visit: impl FnMut(Arc) -> Result<(), Error>,
    ) -> Result<(), Error> {
        if vertex < self.base {
            for &edge in &outgoing[vertex] {
                let first = self.by_edge.get(&edge).and_then(|v| v.first()).copied();
                visit(Arc {
                    to: first.unwrap_or(g.edges[edge].to),
                    edge,
                    start: 0.,
                    end: first
                        .map(|v| self.splits[v - self.base].fraction)
                        .unwrap_or(1.),
                    enter: true,
                })?;
            }
        } else {
            let s = &self.splits[vertex - self.base];
            let ids = &self.by_edge[&s.edge];
            let next = ids
                .iter()
                .position(|&id| id == vertex)
                .and_then(|i| ids.get(i + 1))
                .copied();
            visit(Arc {
                to: next.unwrap_or(g.edges[s.edge].to),
                edge: s.edge,
                start: s.fraction,
                end: next
                    .map(|v| self.splits[v - self.base].fraction)
                    .unwrap_or(1.),
                enter: false,
            })?;
        }
        Ok(())
    }
}
