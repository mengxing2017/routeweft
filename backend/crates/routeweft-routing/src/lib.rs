//! Immutable city/regional dataset routing and a separate synthetic NodeEngine.
//! The production Engine supports coordinate snapping, continuous waypoints,
//! restriction history, bounded work and cooperative cancellation.
//!
//! ```no_run
//! use routeweft_dataset::{LoadOptions, Point};
//! use routeweft_routing::{Engine, QueryOptions, SearchBudget};
//! use std::path::Path;
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let engine = Engine::open(Path::new("dataset"), LoadOptions::default())?;
//! let points = [Point { lon: 7.4206, lat: 43.7311 },
//!               Point { lon: 7.4269, lat: 43.7384 }];
//! let route = engine.route(&points, QueryOptions::default(), &SearchBudget::default())?;
//! let nearest = engine.nearest(points[0], QueryOptions::default(), &SearchBudget::default())?;
//! # Ok(())
//! # }
//! ```
pub mod acceleration;
mod diagnostics;
pub mod geometry;
pub use diagnostics::{BudgetFailure, QueryDiagnostics, QueryOutcome, RoutingAlgorithm};

use routeweft_domain::model::{Coordinate, TravelProfile};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

#[derive(Debug, Clone, Copy)]
pub struct Edge {
    pub from: usize,
    pub to: usize,
    pub duration_ms: u64,
    pub distance_mm: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidGraph,
    InvalidInput,
    UnsupportedProfile,
    NoRoute,
    BudgetExceeded,
    Cancelled,
    CostOverflow,
    NoSegment,
    InvalidDataset,
    IncompatibleFormat,
    Io,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

/// Shared across all work in a query, including edge scans and stale heap pops.
pub struct SearchBudget<'a> {
    pub max_work: usize,
    pub max_states: usize,
    pub max_queue: usize,
    pub max_geometry: usize,
    pub deadline: Option<Instant>,
    pub cancelled: Option<&'a AtomicBool>,
}
impl Default for SearchBudget<'_> {
    fn default() -> Self {
        Self {
            max_work: 2_000_000,
            max_states: 250_000,
            max_queue: 500_000,
            max_geometry: 100_000,
            deadline: None,
            cancelled: None,
        }
    }
}
impl SearchBudget<'_> {
    fn tick(&self, work: &mut usize) -> Result<(), Error> {
        if self.cancelled.is_some_and(|v| v.load(Ordering::Relaxed)) {
            return Err(Error::Cancelled);
        }
        if *work >= self.max_work || self.deadline.is_some_and(|v| Instant::now() >= v) {
            return Err(Error::BudgetExceeded);
        }
        *work += 1;
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Path {
    pub edges: Vec<usize>,
    pub duration_ms: u64,
    pub distance_mm: u64,
}

/// Immutable directed graph. Edge IDs are their input order. Forbidden sequences
/// encode no-turns (two edges) or via-way histories (three or more edges).
/// Only-turns must be expanded by the builder into forbidden alternatives.
pub struct NodeEngine {
    nodes: Vec<Coordinate>,
    edges: Vec<Edge>,
    outgoing: Vec<Vec<usize>>,
    forbidden: Vec<Vec<usize>>,
    history_len: usize,
}

impl NodeEngine {
    pub fn new(
        nodes: Vec<Coordinate>,
        edges: Vec<Edge>,
        forbidden: Vec<Vec<usize>>,
    ) -> Result<Self, Error> {
        if nodes.is_empty()
            || edges
                .iter()
                .any(|e| e.from >= nodes.len() || e.to >= nodes.len() || e.duration_ms == 0)
        {
            return Err(Error::InvalidGraph);
        }
        for sequence in &forbidden {
            if sequence.len() < 2
                || sequence.iter().any(|&id| id >= edges.len())
                || sequence
                    .windows(2)
                    .any(|w| edges[w[0]].to != edges[w[1]].from)
            {
                return Err(Error::InvalidGraph);
            }
        }
        let mut outgoing = vec![Vec::new(); nodes.len()];
        for (id, edge) in edges.iter().enumerate() {
            outgoing[edge.from].push(id);
        }
        let history_len = forbidden.iter().map(|v| v.len() - 1).max().unwrap_or(0);
        Ok(Self {
            nodes,
            edges,
            outgoing,
            forbidden,
            history_len,
        })
    }

    pub fn coordinate(&self, node: usize) -> Option<Coordinate> {
        self.nodes.get(node).copied()
    }

    /// Dijkstra on (node, restriction-history), not just node. Does not snap
    /// coordinates or maintain history across separate calls/waypoint legs.
    pub fn route(
        &self,
        source: usize,
        target: usize,
        profile: TravelProfile,
        budget: &SearchBudget<'_>,
    ) -> Result<Path, Error> {
        if profile != TravelProfile::Driving {
            return Err(Error::UnsupportedProfile);
        }
        if source >= self.nodes.len() || target >= self.nodes.len() {
            return Err(Error::InvalidInput);
        }
        type State = (usize, Vec<usize>);
        let start: State = (source, Vec::new());
        let mut distances = HashMap::from([(start.clone(), 0_u64)]);
        let mut previous: HashMap<State, (State, usize)> = HashMap::new();
        let mut queue = BinaryHeap::from([Reverse((0_u64, start.clone()))]);
        let mut work = 0;
        while let Some(Reverse((cost, state))) = queue.pop() {
            budget.tick(&mut work)?;
            if distances.get(&state) != Some(&cost) {
                continue;
            }
            if state.0 == target {
                let mut cursor = state;
                let mut ids = Vec::new();
                let mut distance_mm = 0_u64;
                while cursor != start {
                    let (parent, edge) = &previous[&cursor];
                    ids.push(*edge);
                    distance_mm = distance_mm
                        .checked_add(self.edges[*edge].distance_mm)
                        .ok_or(Error::CostOverflow)?;
                    cursor = parent.clone();
                }
                ids.reverse();
                return Ok(Path {
                    edges: ids,
                    duration_ms: cost,
                    distance_mm,
                });
            }
            for &id in &self.outgoing[state.0] {
                budget.tick(&mut work)?;
                let mut history = state.1.clone();
                history.push(id);
                let mut prohibited = false;
                for sequence in &self.forbidden {
                    budget.tick(&mut work)?;
                    if history.ends_with(sequence) {
                        prohibited = true;
                        break;
                    }
                }
                if prohibited {
                    continue;
                }
                if history.len() > self.history_len {
                    history.drain(..history.len() - self.history_len);
                }
                let edge = self.edges[id];
                let next = (edge.to, history);
                let next_cost = cost
                    .checked_add(edge.duration_ms)
                    .ok_or(Error::CostOverflow)?;
                if distances.get(&next).is_none_or(|&old| next_cost < old) {
                    distances.insert(next.clone(), next_cost);
                    previous.insert(next.clone(), (state.clone(), id));
                    queue.push(Reverse((next_cost, next)));
                }
            }
        }
        Err(Error::NoRoute)
    }
}

mod roads;
mod topology;
pub use roads::{
    CoverageInfo, Engine, Leg, LoadDiagnostics, Nearest, QueryOptions, RouteResult, Step,
};
