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
pub struct QueryDiagnostics {
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
}
impl<'a, 'b> Context<'a, 'b> {
    pub fn new(budget: &'a SearchBudget<'b>, accelerated: bool) -> Self {
        let start = Instant::now();
        Self {
            budget,
            diagnostics: QueryDiagnostics {
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
                queue_peak: 0,
                failure: None,
            },
            phase: "snap",
            start,
            phase_start: start,
        }
    }
    pub fn phase(&mut self, next: &'static str) {
        let elapsed = self.phase_start.elapsed().as_secs_f64() * 1000.;
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
        if states > self.budget.max_states {
            return Err(self.fail("states", states, self.budget.max_states));
        }
        if queue > self.budget.max_queue {
            return Err(self.fail("queue", queue, self.budget.max_queue));
        }
        self.diagnostics.states = self.diagnostics.states.max(states);
        self.diagnostics.queue_peak = self.diagnostics.queue_peak.max(queue);
        Ok(())
    }
}
