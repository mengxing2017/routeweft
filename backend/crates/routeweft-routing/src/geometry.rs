// SPDX-License-Identifier: BSD-2-Clause
// Copyright (c) 2017, Project OSRM contributors
// All rights reserved.
// Rust adaptation of a planar point-to-segment projection routine.
// Full copyright, conditions and disclaimer: LICENSES/BSD-2-Clause-geometry.txt

/// Projects a point onto a planar segment. Returns (fraction, projected point).
/// Inputs must already share a planar coordinate system; this is not a WGS84
/// nearest-road operation. Degenerate segments return the source endpoint.
/// Rejects non-finite inputs and intermediate overflow.
pub fn project_point_on_segment(
    source: [f64; 2],
    target: [f64; 2],
    point: [f64; 2],
) -> Option<(f64, [f64; 2])> {
    if source
        .into_iter()
        .chain(target)
        .chain(point)
        .any(|v| !v.is_finite())
    {
        return None;
    }
    let slope = [target[0] - source[0], target[1] - source[1]];
    let relative = [point[0] - source[0], point[1] - source[1]];
    let dot = slope[0] * relative[0] + slope[1] * relative[1];
    let squared = slope[0] * slope[0] + slope[1] * slope[1];
    if !squared.is_finite() || !dot.is_finite() {
        return None;
    }
    if squared < f64::EPSILON {
        return Some((0.0, source));
    }
    let ratio = (dot / squared).clamp(0.0, 1.0);
    Some((
        ratio,
        [
            (1.0 - ratio) * source[0] + ratio * target[0],
            (1.0 - ratio) * source[1] + ratio * target[1],
        ],
    ))
}
