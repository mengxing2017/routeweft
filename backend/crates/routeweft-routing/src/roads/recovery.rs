//! Shared path recovery; paged search decodes names only after finding the path.
use super::*;
use std::borrow::Cow;
pub(super) trait RouteAccess {
    fn route_node(&self, id: usize) -> Result<Point, Error>;
    fn route_edge(&self, id: usize) -> Result<Cow<'_, routeweft_dataset::Edge>, Error>;
    fn route_outgoing(
        &self,
        node: usize,
        visit: impl FnMut(usize) -> Result<(), Error>,
    ) -> Result<(), Error>;
    fn route_manifest(&self) -> &Manifest;
    fn route_projected(&self, point: Point) -> [f64; 2];
}
impl RouteAccess for Engine {
    fn route_node(&self, id: usize) -> Result<Point, Error> {
        Ok(self.graph.nodes[id])
    }
    fn route_edge(&self, id: usize) -> Result<Cow<'_, routeweft_dataset::Edge>, Error> {
        Ok(Cow::Borrowed(&self.graph.edges[id]))
    }
    fn route_outgoing(
        &self,
        node: usize,
        mut visit: impl FnMut(usize) -> Result<(), Error>,
    ) -> Result<(), Error> {
        for &id in &self.outgoing[node] {
            visit(id)?;
        }
        Ok(())
    }
    fn route_manifest(&self) -> &Manifest {
        &self.manifest
    }
    fn route_projected(&self, p: Point) -> [f64; 2] {
        self.projected(p)
    }
}
impl Overlay {
    fn route_point(&self, access: &impl RouteAccess, vertex: usize) -> Result<Point, Error> {
        if vertex < self.base {
            access.route_node(vertex)
        } else {
            let split = &self.splits[vertex - self.base];
            let e = access.route_edge(split.edge)?;
            Ok(interpolate(
                access.route_node(e.from)?,
                access.route_node(e.to)?,
                split.fraction,
            ))
        }
    }
}
#[allow(clippy::too_many_arguments)]
pub(super) fn recover_path(
    access: &impl RouteAccess,
    restrictions: &Restrictions,
    vertex: usize,
    initial_stage: usize,
    cost: u64,
    traversals: Vec<(Traversal, usize, usize)>,
    waypoints: usize,
    overlay: &Overlay,
    ctx: &mut Context<'_, '_>,
) -> Result<RouteResult, Error> {
    let mut geometry = vec![overlay.route_point(access, vertex)?];
    let mut snapped = vec![geometry[0]];
    let mut legs = Vec::new();
    let mut steps: Vec<Step> = Vec::new();
    let mut total_distance = 0_u64;
    let mut leg_distance = 0_u64;
    let mut leg_time = 0_u64;
    let mut leg_start = 0;
    let mut prev_edge = None;
    let mut roundabout_exit = 0;
    let mut validated_previous: Option<Traversal> = None;
    let mut validated_state = 0;
    let mut recovered_cost = 0u64;
    // Coincident waypoints consumed at the source produce explicit zero legs.
    for _ in 1..initial_stage {
        legs.push(Leg {
            distance_m: 0.,
            duration_s: 0.,
            geometry_start: 0,
            geometry_end: 0,
        });
        snapped.push(geometry[0]);
    }
    for (arc, before, after) in traversals {
        ctx.tick()?;
        let e = access.route_edge(arc.edge)?;
        ctx.diagnostics.decoded_path_edges += 1;
        ctx.charge_output(e.name.len() as u64 * 2 + 512)?;
        if arc.end > arc.start {
            let continuation =
                validated_previous.is_some_and(|p| p.edge == arc.edge && p.end == arc.start);
            if let Some(previous) = validated_previous {
                if !continuation
                    && (previous.end != 1.
                        || arc.start != 0.
                        || access.route_edge(previous.edge)?.to != e.from)
                {
                    return Err(Error::InvalidDataset);
                }
            }
            if !continuation {
                validated_state = restrictions
                    .step(validated_state, arc.edge)
                    .ok_or(Error::InvalidDataset)?;
                ctx.diagnostics.validated_route_edges += 1;
            }
            validated_previous = Some(arc);
        }
        let d = scaled(e.distance_mm, arc.end - arc.start)?;
        let t = scaled(e.duration_ms, arc.end - arc.start)?;
        recovered_cost = recovered_cost.checked_add(t).ok_or(Error::CostOverflow)?;
        total_distance = total_distance.checked_add(d).ok_or(Error::CostOverflow)?;
        leg_distance = leg_distance.checked_add(d).ok_or(Error::CostOverflow)?;
        leg_time = leg_time.checked_add(t).ok_or(Error::CostOverflow)?;
        let a = geometry.len() - 1;
        let end = interpolate(
            access.route_node(e.from)?,
            access.route_node(e.to)?,
            arc.end,
        );
        geometry.push(end);
        let b = geometry.len() - 1;
        // Account for the outgoing-edge inspections performed by guidance.
        access.route_outgoing(e.from, |_| ctx.tick())?;
        let maneuver = if let Some(prev) = prev_edge {
            maneuver(access, prev, arc.edge, &mut roundabout_exit, ctx)?
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
    if legs.len() != waypoints - 1 || recovered_cost != cost {
        return Err(Error::InvalidDataset);
    }
    ctx.diagnostics.route_cost_ms = Some(cost);
    Ok(RouteResult {
        dataset_id: access.route_manifest().dataset_id.clone(),
        profile: access.route_manifest().profile.clone(),
        format: access.route_manifest().format,
        distance_m: total_distance as f64 / 1000.,
        duration_s: cost as f64 / 1000.,
        geometry,
        legs,
        steps,
        snapped,
        distance_scope: "road_only",
        snap_details: Vec::new(), // Filled once endpoint selection has finished.
    })
}
fn maneuver(
    access: &impl RouteAccess,
    prev: usize,
    next: usize,
    exit: &mut usize,
    ctx: &mut Context<'_, '_>,
) -> Result<String, Error> {
    let a = access.route_edge(prev)?;
    let b = access.route_edge(next)?;
    if prev == next {
        return Ok(if a.roundabout {
            "roundabout_continue"
        } else {
            "straight"
        }
        .into());
    }
    if !a.roundabout && b.roundabout {
        *exit = 0;
        return Ok("roundabout_enter".into());
    }
    if a.roundabout {
        let mut has_exit = false;
        access.route_outgoing(a.to, |id| {
            ctx.tick()?;
            let e = access.route_edge(id)?;
            has_exit |= !e.roundabout && e.to != a.from;
            Ok(())
        })?;
        if has_exit {
            *exit += 1;
        }
        if !b.roundabout {
            return Ok("roundabout_exit".into());
        }
        return Ok("roundabout_continue".into());
    }
    if a.to == b.to && a.from == b.from {
        return Ok("straight".into());
    }
    if a.from == b.to {
        return Ok("uturn".into());
    }
    if a.way == b.way {
        let mut alternatives = 0;
        access.route_outgoing(a.to, |id| {
            ctx.tick()?;
            alternatives += usize::from(access.route_edge(id)?.to != a.from);
            Ok(())
        })?;
        if alternatives <= 1 {
            return Ok("straight".into());
        }
    }
    if a.class.ends_with("_link") && !b.class.ends_with("_link") {
        return Ok("merge".into());
    }
    if !a.class.ends_with("_link") && b.class.ends_with("_link") {
        return Ok("exit".into());
    }
    let p = access.route_projected(access.route_node(a.from)?);
    let q = access.route_projected(access.route_node(a.to)?);
    let r = access.route_projected(access.route_node(b.to)?);
    let incoming = (q[1] - p[1]).atan2(q[0] - p[0]);
    let outgoing = (r[1] - q[1]).atan2(r[0] - q[0]);
    let angle = (outgoing - incoming + std::f64::consts::PI).rem_euclid(2. * std::f64::consts::PI)
        - std::f64::consts::PI;
    Ok(if angle.abs() > 150_f64.to_radians() {
        "uturn"
    } else if angle > 30_f64.to_radians() {
        "left"
    } else if angle < -30_f64.to_radians() {
        "right"
    } else {
        "straight"
    }
    .into())
}
