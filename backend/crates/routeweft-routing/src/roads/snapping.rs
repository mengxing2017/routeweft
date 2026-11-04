//! Shared bounded endpoint selection. Never inserts off-network graph edges.
use super::*;

fn segment(m: &Nearest) -> (usize, usize) {
    (m.from.min(m.to), m.from.max(m.to))
}

/// Public nearest retains its directed-edge cap. Routing instead caps physical
/// segments, keeping both directions without spending two spatial choices.
pub(super) fn insert(matches: &mut Vec<Nearest>, candidate: Nearest, limit: usize, routing: bool) {
    matches.push(candidate);
    matches.sort_by(|a, b| {
        a.distance_m
            .total_cmp(&b.distance_m)
            .then(a.edge.cmp(&b.edge))
    });
    if !routing {
        matches.truncate(limit);
        return;
    }
    let mut groups = Vec::new();
    let mut directions = Vec::new();
    let mut keep = vec![false; matches.len()];
    let mut count = 0;
    // Reserve one variant per direction first, then use spare slots for
    // parallel edges (which can have different costs and turn restrictions).
    for (i, m) in matches.iter().enumerate() {
        let key = segment(m);
        if !groups.contains(&key) {
            if groups.len() == limit {
                continue;
            }
            groups.push(key);
        }
        let direction = (m.from, m.to);
        if !directions.contains(&direction) {
            directions.push(direction);
            keep[i] = true;
            count += 1;
        }
    }
    for (i, m) in matches.iter().enumerate() {
        if count < limit * 2 && !keep[i] && groups.contains(&segment(m)) {
            keep[i] = true;
            count += 1;
        }
    }
    let mut index = 0;
    matches.retain(|_| {
        let selected = keep[index];
        index += 1;
        selected
    });
}

pub(super) fn closest(matches: &[Nearest]) -> Vec<Nearest> {
    let best = matches[0].distance_m;
    matches
        .iter()
        .filter(|m| m.distance_m <= best + 0.01)
        .cloned()
        .collect()
}

fn alternatives(all: &[Nearest], primary: &[Nearest]) -> Vec<Vec<Nearest>> {
    let mut groups = vec![primary.to_vec()];
    for m in all {
        if primary.iter().any(|p| distance(p.point, m.point) <= 0.01) {
            continue;
        }
        if let Some(group) = groups
            .iter_mut()
            .skip(1)
            .find(|g| segment(&g[0]) == segment(m))
        {
            group.push(m.clone());
        } else {
            groups.push(vec![m.clone()]);
        }
    }
    groups
}

pub(super) fn route(
    points: &[Point],
    options: QueryOptions,
    all: &[Vec<Nearest>],
    primary: &[Vec<Nearest>],
    ctx: &mut Context<'_, '_>,
    mut solve: impl FnMut(&[Vec<Nearest>], &mut Context<'_, '_>) -> Result<RouteResult, Error>,
) -> Result<RouteResult, Error> {
    // Preserve exact/nearest routing and its cost whenever it is feasible.
    match solve(primary, ctx) {
        Ok(result) if !collapsed(&result, all) => {
            return describe(result, points, options, all, ctx)
        }
        Ok(_) | Err(Error::NoRoute) => {}
        Err(e) => return Err(e),
    }
    let last = points.len() - 1;
    let starts = alternatives(&all[0], &primary[0]);
    let ends = alternatives(&all[last], &primary[last]);
    // At most (16 + 1)^2 endpoint combinations, never a Cartesian product of
    // via points. Each solve retains the complete turn history and shared ctx.
    let mut trials = Vec::new();
    for (a, start) in starts.iter().enumerate() {
        for (b, end) in ends.iter().enumerate() {
            ctx.tick()?;
            if a != 0 || b != 0 {
                trials.push((start[0].distance_m + end[0].distance_m, a, b));
            }
        }
    }
    trials.sort_by(|a, b| a.0.total_cmp(&b.0).then((a.1, a.2).cmp(&(b.1, b.2))));
    for (_, a, b) in trials {
        ctx.tick()?;
        // A fallback must not collapse distinct road endpoints onto one point.
        if distance(starts[a][0].point, ends[b][0].point) <= 0.01
            && distance(primary[0][0].point, primary[last][0].point) > 0.01
        {
            continue;
        }
        let mut selected = primary.to_vec();
        selected[0] = starts[a].clone();
        selected[last] = ends[b].clone();
        match solve(&selected, ctx) {
            Ok(result) if !collapsed(&result, all) => {
                ctx.diagnostics.snap_fallbacks += usize::from(a != 0) + usize::from(b != 0);
                return describe(result, points, options, all, ctx);
            }
            Ok(_) | Err(Error::NoRoute) => {}
            Err(e) => return Err(e),
        }
    }
    Err(Error::NoRoute)
}

fn collapsed(result: &RouteResult, original: &[Vec<Nearest>]) -> bool {
    result.distance_m <= 0.001
        && distance(original[0][0].point, original[original.len() - 1][0].point) > 0.01
        && distance(result.snapped[0], result.snapped[result.snapped.len() - 1]) <= 0.01
}

fn describe(
    mut result: RouteResult,
    points: &[Point],
    options: QueryOptions,
    all: &[Vec<Nearest>],
    ctx: &mut Context<'_, '_>,
) -> Result<RouteResult, Error> {
    ctx.charge_output(points.len() as u64 * 512)?;
    result.snap_details = points
        .iter()
        .zip(&result.snapped)
        .zip(all)
        .map(|((&requested, &snapped), candidates)| {
            let offset = distance(requested, snapped);
            let fallback = offset > candidates[0].distance_m + 0.01;
            SnapDetail {
                requested_coordinate: requested,
                snapped_coordinate: snapped,
                snap_distance_m: offset,
                search_radius_m: options.radius_m,
                snap_method: if fallback {
                    "reachable_alternative"
                } else {
                    "road_projection"
                },
                fallback_used: fallback,
                access_status: if offset <= 0.01 {
                    "on_network"
                } else {
                    "unknown"
                },
                arrival_status: if offset <= 0.01 {
                    "on_network"
                } else {
                    "nearby"
                },
            }
        })
        .collect();
    Ok(result)
}
