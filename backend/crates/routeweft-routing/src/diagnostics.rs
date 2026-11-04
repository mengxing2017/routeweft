use crate::{Error, RouteResult, SearchBudget};
use serde::Serialize;
use std::time::Instant;

/// Dijkstra remains available as an independent search-space baseline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RoutingAlgorithm {
    #[default]
    Auto,
    Dijkstra,
}

#[derive(Debug, Serialize)]
pub struct BudgetFailure {
    pub reason: &'static str,
    pub phase: &'static str,
    pub observed: u64,
    pub limit: u64,
}

#[derive(Debug, Serialize)]
pub struct QueryPhase {
    pub phase: &'static str,
    pub elapsed_ms: f64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub cache_bytes_read: u64,
}
#[derive(Debug, Serialize)]
pub struct PagedWorkspaceUsage {
    pub labels: usize,
    pub labels_capacity: usize,
    pub labels_bytes: u64,
    pub map_capacity: usize,
    pub map_bytes: u64,
    pub frontier: usize,
    pub frontier_capacity: usize,
    pub frontier_bytes: u64,
    pub fixed_and_restrictions_bytes: u64,
    pub current_bytes: u64,
    pub allocation_growth_bytes: u64,
    pub planned_peak_bytes: u64,
    pub limit_bytes: u64,
}
#[derive(Debug, Serialize)]
pub struct QueryDiagnostics {
    pub phases: Vec<QueryPhase>,
    pub paged_workspace: Option<PagedWorkspaceUsage>,
    pub decoded_path_edges: usize,
    pub recovery_output_charge_bytes: u64,
    pub route_cost_ms: Option<u64>,
    pub page_cache: Option<routeweft_dataset::paged::PageStats>,
    pub working_graph_edges: usize,
    pub algorithm: &'static str,
    pub elapsed_ms: f64,
    pub snap_ms: f64,
    pub search_ms: f64,
    pub recover_ms: f64,
    pub work: usize,
    pub states: usize,
    pub settled_states: usize,
    pub forward_settled: usize,
    pub reverse_settled: usize,
    pub clique_skips: usize,
    pub compressed_edges: usize,
    pub snap_fallbacks: usize,
    pub scanned_edges: usize,
    pub relaxed_shortcuts: usize,
    /// Original edge sequence replayed through the restriction automaton after unpacking.
    pub validated_route_edges: usize,
    pub queue_peak: usize,
    pub failure: Option<BudgetFailure>,
}

pub struct QueryOutcome {
    pub result: Result<RouteResult, Error>,
    pub diagnostics: QueryDiagnostics,
}

pub(crate) struct Context<'a, 'b> {
    pub budget: &'a SearchBudget<'b>,
    pub diagnostics: QueryDiagnostics,
    phase: &'static str,
    start: Instant,
    phase_start: Instant,
    pages: Option<&'a routeweft_dataset::paged::PagedGraph>,
    last_pages: routeweft_dataset::paged::PageStats,
    pub memory: Option<(u64, u64)>,
    pub output_bytes: u64,
    live_states: usize,
    live_queue: usize,
}
impl<'a, 'b> Context<'a, 'b> {
    pub fn new(budget: &'a SearchBudget<'b>, accelerated: bool) -> Self {
        let start = Instant::now();
        Self {
            budget,
            diagnostics: QueryDiagnostics {
                phases: Vec::new(),
                paged_workspace: None,
                decoded_path_edges: 0,
                recovery_output_charge_bytes: 0,
                route_cost_ms: None,
                page_cache: None,
                working_graph_edges: 0,
                algorithm: if accelerated { "mld" } else { "dijkstra" },
                elapsed_ms: 0.,
                snap_ms: 0.,
                search_ms: 0.,
                recover_ms: 0.,
                work: 0,
                states: 0,
                settled_states: 0,
                forward_settled: 0,
                reverse_settled: 0,
                clique_skips: 0,
                compressed_edges: 0,
                snap_fallbacks: 0,
                scanned_edges: 0,
                relaxed_shortcuts: 0,
                validated_route_edges: 0,
                queue_peak: 0,
                failure: None,
            },
            phase: "snap",
            start,
            phase_start: start,
            pages: None,
            last_pages: Default::default(),
            memory: None,
            output_bytes: 0,
            live_states: 0,
            live_queue: 0,
        }
    }
    pub fn attach_pages(&mut self, graph: &'a routeweft_dataset::paged::PagedGraph) {
        self.last_pages = graph.stats();
        self.pages = Some(graph);
    }
    pub fn phase(&mut self, next: &'static str) {
        let elapsed = self.phase_start.elapsed().as_secs_f64() * 1000.;
        let pages = self.pages.map(|p| p.stats()).unwrap_or_default();
        if self.pages.is_some() {
            self.diagnostics.phases.push(QueryPhase {
                phase: self.phase,
                elapsed_ms: elapsed,
                cache_hits: pages.hits.saturating_sub(self.last_pages.hits),
                cache_misses: pages.misses.saturating_sub(self.last_pages.misses),
                cache_bytes_read: pages.bytes_read.saturating_sub(self.last_pages.bytes_read),
            });
        }
        self.last_pages = pages;
        match self.phase {
            "snap" => self.diagnostics.snap_ms += elapsed,
            "search" => self.diagnostics.search_ms += elapsed,
            "recover" => self.diagnostics.recover_ms += elapsed,
            _ => {}
        }
        self.phase = next;
        self.phase_start = Instant::now();
    }
    pub fn finish(mut self) -> QueryDiagnostics {
        self.phase("done");
        self.diagnostics.elapsed_ms = self.start.elapsed().as_secs_f64() * 1000.;
        self.diagnostics.recovery_output_charge_bytes = self.output_bytes;
        self.diagnostics
    }
    pub fn fail(&mut self, reason: &'static str, observed: usize, limit: usize) -> Error {
        self.diagnostics.failure = Some(BudgetFailure {
            reason,
            phase: self.phase,
            observed: observed as u64,
            limit: limit as u64,
        });
        Error::BudgetExceeded
    }
    pub fn tick(&mut self) -> Result<(), Error> {
        // Keep cancellation and deadline cooperative at every bounded operation.
        if self
            .budget
            .cancelled
            .is_some_and(|v| v.load(std::sync::atomic::Ordering::Relaxed))
        {
            return Err(Error::Cancelled);
        }
        if self.budget.deadline.is_some_and(|v| Instant::now() >= v) {
            return Err(self.fail(
                "deadline",
                self.start.elapsed().as_millis() as usize,
                self.budget
                    .deadline
                    .unwrap()
                    .saturating_duration_since(self.start)
                    .as_millis() as usize,
            ));
        }
        if self.diagnostics.work >= self.budget.max_work {
            return Err(self.fail("work", self.diagnostics.work, self.budget.max_work));
        }
        self.diagnostics.work += 1;
        Ok(())
    }
    pub fn capacity(&mut self, states: usize, queue: usize) -> Result<(), Error> {
        if let Some((limit, base)) = self.memory {
            let bytes = base
                .saturating_add((states as u64).saturating_mul(512))
                .saturating_add((queue as u64).saturating_mul(64))
                .saturating_add(self.output_bytes);
            if bytes > limit {
                return Err(self.fail(
                    "query_workspace_bytes",
                    bytes.min(usize::MAX as u64) as usize,
                    limit.min(usize::MAX as u64) as usize,
                ));
            }
        }
        if states > self.budget.max_states {
            return Err(self.fail("states", states, self.budget.max_states));
        }
        if queue > self.budget.max_queue {
            return Err(self.fail("queue", queue, self.budget.max_queue));
        }
        self.live_states = states;
        self.live_queue = queue;
        self.diagnostics.states = self.diagnostics.states.max(states);
        self.diagnostics.queue_peak = self.diagnostics.queue_peak.max(queue);
        Ok(())
    }
    pub fn charge_output(&mut self, bytes: u64) -> Result<(), Error> {
        self.output_bytes = self.output_bytes.saturating_add(bytes);
        self.capacity(self.live_states, self.live_queue)
    }
}
