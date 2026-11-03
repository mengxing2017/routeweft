//! Deterministic, bounded directed walks with continuous restriction history.
use super::*;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::time::Instant;

pub const SAMPLER_VERSION: u32 = 1;
#[derive(Clone, Debug, Serialize)]
pub struct SamplingOptions {
    pub count: usize,
    pub waypoints: usize,
    pub max_path_edges: usize,
    pub max_attempts: usize,
    pub max_workspace_bytes: u64,
    /// Strictly increasing OD great-circle distance boundaries, in meters.
    pub distance_bands_m: Vec<f64>,
}
impl Default for SamplingOptions {
    fn default() -> Self {
        Self {
            count: 12,
            waypoints: 2,
            max_path_edges: 1024,
            max_attempts: 10000,
            max_workspace_bytes: 16 * 1024 * 1024,
            distance_bands_m: vec![1000., 10000.],
        }
    }
}
#[derive(Debug, Serialize)]
pub struct RouteSample {
    pub coordinates: Vec<Point>,
    pub witness_edges: Vec<usize>,
    pub witness_end_automaton: usize,
    pub witness_distance_m: f64,
    pub witness_duration_ms: u64,
    pub od_distance_m: f64,
    pub distance_band: usize,
}
#[derive(Debug, Serialize)]
pub struct RouteSamples {
    pub sampler_version: u32,
    pub seed_sha256: String,
    pub dataset_id: String,
    pub graph_sha256: String,
    pub profile: String,
    pub options: SamplingOptions,
    pub max_work: usize,
    pub work: usize,
    pub attempts: usize,
    pub coverage_complete: bool,
    pub shortfall_reasons: Vec<&'static str>,
    pub requested_per_band: Vec<usize>,
    pub generated_per_band: Vec<usize>,
    pub pairs: Vec<Vec<Point>>,
    pub samples: Vec<RouteSample>,
}
fn next(seed: &mut u64) -> u64 {
    *seed = seed.wrapping_add(0x9e3779b97f4a7c15);
    let mut n = *seed;
    n = (n ^ (n >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    n = (n ^ (n >> 27)).wrapping_mul(0x94d049bb133111eb);
    n ^ (n >> 31)
}
impl Engine {
    /// Witnesses prove a legal directed multi-edge walk, not shortest paths.
    /// Consumers must still route/differentially verify the emitted coordinates.
    pub fn sample_routes(
        &self,
        options: SamplingOptions,
        budget: &SearchBudget<'_>,
    ) -> Result<RouteSamples, Error> {
        let bands = options.distance_bands_m.len() + 1;
        if !(1..=1000).contains(&options.count)
            || options.count < bands
            || !(2..=10).contains(&options.waypoints)
            || !(2..=4096).contains(&options.max_path_edges)
            || options.max_path_edges + 1 < options.waypoints
            || options.max_attempts == 0
            || bands > 8
            || options
                .distance_bands_m
                .iter()
                .any(|v| !v.is_finite() || *v <= 0.)
            || options.distance_bands_m.windows(2).any(|w| w[0] >= w[1])
        {
            return Err(Error::InvalidInput);
        }
        let per_sample = (options.max_path_edges as u64)
            .checked_mul(16)
            .and_then(|v| v.checked_add(1024))
            .ok_or(Error::BudgetExceeded)?;
        let workspace = per_sample
            .checked_mul(options.count as u64)
            .and_then(|v| v.checked_add(options.max_path_edges as u64 * 32 + 65536))
            .ok_or(Error::BudgetExceeded)?;
        crate::resources::check(workspace, options.max_workspace_bytes)?;
        let seed_bytes = Sha256::digest(
            format!(
                "routeweft-sampler-{SAMPLER_VERSION}:{}",
                self.manifest.graph_sha256
            )
            .as_bytes(),
        );
        let mut seed = u64::from_le_bytes(seed_bytes[..8].try_into().unwrap());
        let mut result = RouteSamples {
            sampler_version: SAMPLER_VERSION,
            seed_sha256: format!("{seed_bytes:x}"),
            dataset_id: self.manifest.dataset_id.clone(),
            graph_sha256: self.manifest.graph_sha256.clone(),
            profile: self.manifest.profile.clone(),
            requested_per_band: (0..bands)
                .map(|b| options.count / bands + usize::from(b < options.count % bands))
                .collect(),
            generated_per_band: vec![0; bands],
            max_work: budget.max_work,
            work: 0,
            attempts: 0,
            coverage_complete: false,
            shortfall_reasons: Vec::new(),
            samples: Vec::new(),
            pairs: Vec::new(),
            options,
        };
        let mut ctx = Context::new(budget, false);
        let mut seen = HashSet::new();
        let run = (|| -> Result<(), Error> {
            if self.graph.edges.len() < 2 {
                return Ok(());
            }
            while result.samples.len() < result.options.count
                && result.attempts < result.options.max_attempts
            {
                ctx.tick()?;
                result.attempts += 1;
                let target = (0..bands)
                    .map(|i| (result.attempts + i) % bands)
                    .find(|&b| result.generated_per_band[b] < result.requested_per_band[b])
                    .unwrap();
                let first = next(&mut seed) as usize % self.graph.edges.len();
                let mut edge = first;
                let mut automaton = 0;
                let start = self.graph.edges[first].from;
                let mut vertices = vec![start];
                let mut witness = Vec::new();
                let mut distance_mm = 0u64;
                let mut duration_ms = 0u64;
                for _ in 0..result.options.max_path_edges {
                    ctx.tick()?;
                    let Some(state) = self.restrictions.step(automaton, edge) else {
                        break;
                    };
                    automaton = state;
                    let e = &self.graph.edges[edge];
                    let Some(d) = distance_mm.checked_add(e.distance_mm) else {
                        break;
                    };
                    let Some(t) = duration_ms.checked_add(e.duration_ms) else {
                        break;
                    };
                    distance_mm = d;
                    duration_ms = t;
                    witness.push(edge);
                    vertices.push(e.to);
                    let od = super::distance(self.graph.nodes[start], self.graph.nodes[e.to]);
                    let band = result
                        .options
                        .distance_bands_m
                        .partition_point(|&v| od >= v);
                    if witness.len() >= 2
                        && vertices.len() >= result.options.waypoints
                        && witness.iter().any(|&id| id != first)
                        && od > 0.
                        && band == target
                    {
                        let ids: Vec<_> = (0..result.options.waypoints)
                            .map(|i| {
                                vertices[i * (vertices.len() - 1) / (result.options.waypoints - 1)]
                            })
                            .collect();
                        let coordinates: Vec<_> =
                            ids.iter().map(|&id| self.graph.nodes[id]).collect();
                        let bits = |v: f64| if v == 0. { 0 } else { v.to_bits() };
                        let key: Vec<_> = coordinates
                            .iter()
                            .map(|p| (bits(p.lon), bits(p.lat)))
                            .collect();
                        if !coordinates.windows(2).any(|w| w[0] == w[1]) && seen.insert(key) {
                            result.pairs.push(coordinates.clone());
                            result.samples.push(RouteSample {
                                coordinates,
                                witness_edges: witness,
                                witness_end_automaton: automaton,
                                witness_distance_m: distance_mm as f64 / 1000.,
                                witness_duration_ms: duration_ms,
                                od_distance_m: od,
                                distance_band: band,
                            });
                            result.generated_per_band[band] += 1;
                            break;
                        }
                    }
                    let outgoing = &self.outgoing[e.to];
                    if outgoing.is_empty() {
                        break;
                    }
                    let offset = next(&mut seed) as usize % outgoing.len();
                    let mut chosen = None;
                    let mut fallback = None;
                    for i in 0..outgoing.len() {
                        ctx.tick()?;
                        let id = outgoing[(offset + i) % outgoing.len()];
                        if self.restrictions.step(automaton, id).is_some() {
                            if self.graph.edges[id].to == e.from {
                                fallback = Some(id);
                                continue;
                            }
                            chosen = Some(id);
                            break;
                        }
                    }
                    let Some(id) = chosen.or(fallback) else {
                        break;
                    };
                    edge = id;
                }
            }
            Ok(())
        })();
        if let Err(error) = run {
            if error != Error::BudgetExceeded {
                return Err(error);
            }
            result.shortfall_reasons.push(
                if budget.deadline.is_some_and(|d| Instant::now() >= d) {
                    "deadline"
                } else {
                    "work"
                },
            );
        }
        result.work = ctx.diagnostics.work;
        result.coverage_complete = result.samples.len() == result.options.count;
        if !result.coverage_complete {
            result
                .shortfall_reasons
                .push(if self.graph.edges.len() < 2 {
                    "no_cross_edge_path"
                } else {
                    "distance_bands_or_unique_paths_exhausted"
                });
        }
        Ok(result)
    }
}
