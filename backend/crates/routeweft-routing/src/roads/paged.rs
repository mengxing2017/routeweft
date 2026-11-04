//! Compact direct A*/Dijkstra over persisted CSR, with path-only name decoding.
use super::*;
use routeweft_dataset::paged::{PageStats, PagedGraph};
use std::{collections::HashSet, sync::Mutex};
#[derive(Clone, Copy, Debug, Serialize)]
pub struct PagedOptions {
    pub cache_bytes: u64,
    pub planning_workspace_bytes: u64,
    pub query_workspace_bytes: u64,
}
impl Default for PagedOptions {
    fn default() -> Self {
        Self {
            cache_bytes: 16 * 1024 * 1024,
            planning_workspace_bytes: 64 * 1024 * 1024,
            query_workspace_bytes: 256 * 1024 * 1024,
        }
    }
}
#[derive(Debug, Serialize)]
pub struct PagedResourcePlan {
    pub schema: u32,
    pub estimator_version: u32,
    pub access_model: &'static str,
    pub dataset_id: String,
    pub graph_sha256: String,
    pub query_index_sha256: String,
    pub profile: String,
    pub options: PagedOptions,
    pub tool_build: serde_json::Value,
    pub persistent_graph_bytes: u64,
    pub persistent_index_bytes: u64,
    pub resident_bytes: u64,
    pub load_peak_bytes: u64,
    pub query_workspace_bytes: u64,
    pub shared_cache_bytes: u64,
    pub verification_scope: &'static str,
    pub excluded_unknown: Vec<&'static str>,
}
pub struct PagedEngine {
    graph: PagedGraph,
    options: PagedOptions,
    capacity: routeweft_dataset::GraphCapacity,
    // Bounded graph construction is serialized per engine. Runtime additionally
    // reserves each executing query's complete workspace across engine versions.
    query: Mutex<()>,
    load_diagnostics: LoadDiagnostics,
}
fn integer(bytes: &[u8], offset: usize) -> usize {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap()) as usize
}
impl PagedEngine {
    pub fn verify_indexes(
        &self,
        workspace: u64,
    ) -> Result<routeweft_dataset::paged::QueryIndexVerification, Error> {
        self.graph.verify_indexes(workspace).map_err(dataset_error)
    }
    pub fn open(
        path: &Path,
        mut limits: LoadOptions,
        capacity: routeweft_dataset::GraphCapacity,
        options: PagedOptions,
    ) -> Result<Self, Error> {
        let started = std::time::Instant::now();
        let caps = capacity.limits();
        limits.max_nodes = limits.max_nodes.min(caps.max_nodes);
        limits.max_edges = limits.max_edges.min(caps.max_edges);
        limits.max_bytes = limits.max_bytes.min(caps.max_bytes);
        let resident = options
            .cache_bytes
            .checked_add(131072)
            .ok_or(Error::BudgetExceeded)?;
        let peak = resident
            .checked_add(options.planning_workspace_bytes)
            .ok_or(Error::BudgetExceeded)?;
        if peak > limits.max_resident_bytes || options.query_workspace_bytes < 131072 {
            return Err(Error::BudgetExceeded);
        }
        let graph = PagedGraph::open(
            path,
            limits,
            options.cache_bytes,
            options.planning_workspace_bytes,
        )
        .map_err(dataset_error)?;
        if graph.metadata.span_degrees > capacity.max_span_degrees() {
            return Err(Error::InvalidDataset);
        }
        let paged = Some(graph.open_diagnostics);
        Ok(Self {
            graph,
            options,
            capacity,
            query: Mutex::new(()),
            load_diagnostics: LoadDiagnostics {
                paged,
                dataset_ms: started.elapsed().as_secs_f64() * 1000.,
                ..Default::default()
            },
        })
    }
    pub fn resource_plan(&self) -> PagedResourcePlan {
        PagedResourcePlan {
            schema: 1,
            estimator_version: 2,
            access_model: "paged_compact_direct_astar_alt_or_dijkstra",
            dataset_id: self.manifest().dataset_id.clone(),
            graph_sha256: self.manifest().graph_sha256.clone(),
            query_index_sha256: self.graph.index_sha256.clone(),
            profile: self.manifest().profile.clone(),
            options: self.options,
            tool_build: self.graph.metadata.tool_build.clone(),
            persistent_graph_bytes: self.manifest().graph_bytes,
            persistent_index_bytes: self.graph.metadata.bytes,
            resident_bytes: self.options.cache_bytes + 131072,
            load_peak_bytes: self.options.cache_bytes
                + 131072
                + self.options.planning_workspace_bytes,
            query_workspace_bytes: self.options.query_workspace_bytes,
            shared_cache_bytes: self.options.cache_bytes,
            verification_scope:
                "streamed_base_and_index_authentication_access_bounds_not_full_index_semantics",
            excluded_unknown: vec![
                "os_file_cache_and_process_overhead",
                "host_results_retained_after_query",
                "offline_preparation_peak",
                "mld_not_loaded_by_paged_backend",
            ],
        }
    }
    pub fn manifest(&self) -> &Manifest {
        &self.graph.manifest
    }
    pub fn page_stats(&self) -> PageStats {
        self.graph.stats()
    }
    pub fn query_index_sha256(&self) -> &str {
        &self.graph.index_sha256
    }
    pub fn load_diagnostics(&self) -> LoadDiagnostics {
        self.load_diagnostics
    }
    pub fn acceleration_info(&self) -> Option<AccelerationInfo> {
        None
    }
    pub fn acceleration_sha256(&self) -> Option<&str> {
        None
    }
    fn nearest_component(
        &self,
        point: Point,
        options: QueryOptions,
        component: Option<u32>,
        routing: bool,
        ctx: &mut Context<'_, '_>,
    ) -> Result<Vec<Nearest>, Error> {
        point.coordinate().map_err(|_| Error::InvalidInput)?;
        if point.lat.abs() > 85.
            || !options.radius_m.is_finite()
            || options.radius_m <= 0.
            || options.radius_m > 10000.
            || options.candidates == 0
            || options.candidates > 16
        {
            return Err(Error::InvalidInput);
        }
        if options.profile != TravelProfile::Driving {
            return Err(Error::UnsupportedProfile);
        }
        let margin = (options.radius_m / R).to_degrees() * 1.01;
        let cosine = (point.lat.to_radians().abs() + margin.to_radians())
            .cos()
            .max(0.01);
        let bounds = [
            point.lon - margin / cosine,
            point.lat - margin,
            point.lon + margin / cosine,
            point.lat + margin,
        ];
        let cos = if self.capacity == routeweft_dataset::GraphCapacity::Regional {
            point.lat.to_radians().cos()
        } else {
            self.graph.metadata.latitude_midpoint.to_radians().cos()
        };
        let project = |p: Point| [p.lon.to_radians() * R * cos, p.lat.to_radians() * R];
        let mut found = Vec::new();
        let mut stopped = None;
        let scan = self.graph.spatial_candidates(bounds, |id| {
            ctx.tick().map_err(|e| {
                stopped = Some(e);
                routeweft_dataset::invalid("query stopped")
            })?;
            let Some(id) = id else {
                return Ok(());
            };
            let raw = self.graph.edge_record(id)?;
            let from = integer(&raw, 0);
            let to = integer(&raw, 8);
            if component.is_some_and(|c| self.graph.component(from).map_or(true, |v| v.0 != c)) {
                return Ok(());
            }
            let a = self.graph.node(from)?;
            let b = self.graph.node(to)?;
            let (fraction, _) =
                crate::geometry::project_point_on_segment(project(a), project(b), project(point))
                    .ok_or_else(|| routeweft_dataset::invalid("projection"))?;
            let snapped = Point {
                lon: a.lon + (b.lon - a.lon) * fraction,
                lat: a.lat + (b.lat - a.lat) * fraction,
            };
            let d = distance(point, snapped);
            if d <= options.radius_m {
                snapping::insert(
                    &mut found,
                    Nearest {
                        edge: id,
                        point: snapped,
                        fraction,
                        distance_m: d,
                        from,
                        to,
                    },
                    options.candidates,
                    routing,
                );
            }
            Ok(())
        });
        scan.map_err(|e| stopped.unwrap_or_else(|| dataset_error(e)))?;
        if found.is_empty() {
            Err(Error::NoSegment)
        } else {
            Ok(found)
        }
    }
    pub fn nearest_detailed(
        &self,
        point: Point,
        options: QueryOptions,
        budget: &SearchBudget<'_>,
    ) -> (Result<Vec<Nearest>, Error>, crate::QueryDiagnostics) {
        let mut ctx = Context::new(budget, false);
        let result = match self.query.try_lock() {
            Ok(_guard) => {
                ctx.attach_pages(&self.graph);
                self.nearest_component(point, options, None, false, &mut ctx)
            }
            Err(_) => Err(ctx.fail("paged_query_busy", 1, 0)),
        };
        (result, ctx.finish())
    }
    pub fn route_detailed(
        &self,
        points: &[Point],
        options: QueryOptions,
        budget: &SearchBudget<'_>,
        algorithm: RoutingAlgorithm,
    ) -> crate::QueryOutcome {
        let mut ctx = Context::new(budget, false);
        if algorithm == RoutingAlgorithm::Auto && self.graph.metadata.schema == 2 {
            ctx.diagnostics.algorithm = "astar";
        }
        let result = match self.query.try_lock() {
            Ok(_guard) => {
                ctx.attach_pages(&self.graph);
                self.route_inner(points, options, &mut ctx)
            }
            Err(_) => Err(ctx.fail("paged_query_busy", 1, 0)),
        };
        ctx.diagnostics.page_cache = Some(self.page_stats());
        crate::QueryOutcome {
            result,
            diagnostics: ctx.finish(),
        }
    }
    fn route_inner(
        &self,
        points: &[Point],
        options: QueryOptions,
        ctx: &mut Context<'_, '_>,
    ) -> Result<RouteResult, Error> {
        if !(2..=10).contains(&points.len()) {
            return Err(Error::InvalidInput);
        }
        let mut candidates = Vec::new();
        for &point in points {
            candidates.push(self.nearest_component(point, options, None, true, ctx)?);
        }
        let mut matches: Vec<_> = candidates.iter().map(|v| snapping::closest(v)).collect();
        let mut components = Vec::new();
        for list in &matches {
            let mut ids = HashSet::new();
            for m in list {
                ids.insert(self.graph.component(m.from).map_err(dataset_error)?.0);
            }
            components.push(ids);
        }
        let common = components[0]
            .iter()
            .any(|c| components.iter().all(|s| s.contains(c)));
        if !common {
            for (i, list) in matches.iter_mut().enumerate() {
                if i > 0 && i + 1 < points.len() {
                    continue;
                }
                let (component, size) =
                    self.graph.component(list[0].from).map_err(dataset_error)?;
                if component != self.graph.metadata.largest_component && size < 1000 {
                    match self.nearest_component(
                        points[i],
                        options,
                        Some(self.graph.metadata.largest_component),
                        true,
                        ctx,
                    ) {
                        Ok(mut near) => {
                            let best = near[0].distance_m;
                            near.retain(|m| m.distance_m <= best + 0.01);
                            *list = near;
                            ctx.diagnostics.snap_fallbacks += 1;
                        }
                        Err(Error::NoSegment) => {}
                        Err(e) => return Err(e),
                    }
                }
            }
        }
        snapping::route(
            points,
            options,
            &candidates,
            &matches,
            ctx,
            |selected, ctx| self.route_matched(selected, ctx),
        )
    }
    fn route_matched(
        &self,
        matches: &[Vec<Nearest>],
        ctx: &mut Context<'_, '_>,
    ) -> Result<RouteResult, Error> {
        for pair in matches.windows(2) {
            let mut a = HashSet::new();
            for m in &pair[0] {
                a.insert(self.graph.component(m.from).map_err(dataset_error)?.0);
            }
            let mut connected = false;
            for m in &pair[1] {
                connected |= a.contains(&self.graph.component(m.from).map_err(dataset_error)?.0);
            }
            if !connected {
                return Err(Error::NoRoute);
            }
        }
        ctx.phase("prepare");
        let sequences = self
            .graph
            .restrictions(self.options.query_workspace_bytes / 4)
            .map_err(dataset_error)?;
        let restriction_charge = sequences
            .iter()
            .map(|v| 128 + v.len() as u64 * 512)
            .sum::<u64>()
            + 65536;
        ctx.phase("search");
        if restriction_charge.saturating_add(131072) > self.options.query_workspace_bytes {
            return Err(ctx.fail(
                "paged_restrictions_bytes",
                restriction_charge.saturating_add(131072) as usize,
                self.options.query_workspace_bytes as usize,
            ));
        }
        let restrictions = Restrictions::new(&sequences);
        drop(sequences);
        let overlay = Overlay::with_base(self.manifest().nodes, matches);
        self.search_compact(matches, &overlay, &restrictions, restriction_charge, ctx)
    }
}

impl recovery::RouteAccess for PagedEngine {
    fn route_node(&self, id: usize) -> Result<Point, Error> {
        self.graph.node(id).map_err(dataset_error)
    }
    fn route_edge(
        &self,
        id: usize,
    ) -> Result<std::borrow::Cow<'_, routeweft_dataset::Edge>, Error> {
        self.graph
            .edge(id)
            .map(std::borrow::Cow::Owned)
            .map_err(dataset_error)
    }
    fn route_outgoing(
        &self,
        node: usize,
        mut visit: impl FnMut(usize) -> Result<(), Error>,
    ) -> Result<(), Error> {
        for offset in self.graph.outgoing_range(node).map_err(dataset_error)? {
            visit(self.graph.outgoing_edge(offset).map_err(dataset_error)?)?;
        }
        Ok(())
    }
    fn route_manifest(&self) -> &Manifest {
        self.manifest()
    }
    fn route_projected(&self, p: Point) -> [f64; 2] {
        [
            p.lon.to_radians() * R * self.graph.metadata.latitude_midpoint.to_radians().cos(),
            p.lat.to_radians() * R,
        ]
    }
}

#[path = "paged_search.rs"]
mod search;
