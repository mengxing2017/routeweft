/** Request coordination adapted from Project OSRM (c) 2014.
 * See LICENSE.osrm-frontend and THIRD_PARTY_NOTICES.md.
 */
export interface Point { lon: number; lat: number }
export interface QueryOptions { profile?: 'driving'; radius_m?: number; candidates?: number }
export interface ResolvedQueryOptions { profile: 'driving'; radius_m: number; candidates: number }
export interface Leg {
  distance_m: number; duration_s: number; geometry_start: number; geometry_end: number;
}
export interface Step extends Leg { maneuver: string; name: string; exit: number | null }
export interface RouteResult {
  dataset_id: string; profile: string; format: number;
  distance_m: number; duration_s: number; geometry: Point[];
  legs: Leg[]; steps: Step[]; snapped: Point[];
}
export interface Nearest {
  edge: number; point: Point; fraction: number; distance_m: number; from: number; to: number;
}
export interface RouteRequest { coordinates: Point[]; options: ResolvedQueryOptions }
export interface NearestRequest { coordinate: Point; options: ResolvedQueryOptions }
/** Host adapters own authentication, wire protocol, response validation and backend error mapping. */
export interface RouteTransport {
  route(request: RouteRequest, signal?: AbortSignal): Promise<RouteResult>;
}
export interface RoutingTransport extends RouteTransport {
  route(request: RouteRequest, signal?: AbortSignal): Promise<RouteResult>;
  nearest(request: NearestRequest, signal?: AbortSignal): Promise<Nearest[]>;
}
export type ErrorCode = 'InvalidInput' | 'UnsupportedProfile' | 'NoSegment' | 'NoRoute'
  | 'InvalidDataset' | 'IncompatibleFormat' | 'BudgetExceeded' | 'Cancelled'
  | 'CostOverflow' | 'Io' | 'Network' | 'RateLimited';
export class RoutingError extends Error {
  constructor(public readonly code: ErrorCode, message: string, options?: ErrorOptions) {
    super(message, options); this.name = 'RoutingError';
  }
}
/** Wrap map world copies, retaining already valid coordinates (including +180). */
export function normalizePoint(point: Point): Point {
  if (!Number.isFinite(point.lon) || !Number.isFinite(point.lat) || Math.abs(point.lat) > 85)
    throw new RoutingError('InvalidInput', 'Coordinates must be finite and latitude within [-85, 85]');
  return { lon: point.lon >= -180 && point.lon <= 180 ? point.lon :
    ((point.lon % 360 + 540) % 360) - 180, lat: point.lat };
}
function resolveOptions(options: QueryOptions = {}): ResolvedQueryOptions {
  const { profile = 'driving', radius_m = 1000, candidates = 8 } = options;
  if (profile !== 'driving') throw new RoutingError('UnsupportedProfile', 'Only driving is supported');
  if (!Number.isFinite(radius_m) || radius_m <= 0 || radius_m > 10000 ||
      !Number.isInteger(candidates) || candidates < 1 || candidates > 16)
    throw new RoutingError('InvalidInput', 'radius_m must be in (0, 10000]; candidates in [1, 16]');
  return { profile, radius_m, candidates };
}
export function createRouteClient(transport: RouteTransport) {
  return {
    async route(coordinates: readonly Point[], options?: QueryOptions, signal?: AbortSignal) {
      if (coordinates.length < 2 || coordinates.length > 10)
        throw new RoutingError('InvalidInput', 'Route requires 2–10 waypoints');
      signal?.throwIfAborted();
      return transport.route({ coordinates: coordinates.map(normalizePoint), options: resolveOptions(options) }, signal);
    },

  };
}
export function createRoutingClient(transport: RoutingTransport) {
  return { ...createRouteClient(transport),
    async nearest(coordinate: Point, options?: QueryOptions, signal?: AbortSignal) {
      signal?.throwIfAborted();
      return transport.nearest({ coordinate: normalizePoint(coordinate), options: resolveOptions(options) }, signal);
    },
  };
}
export type RoutingClient = ReturnType<typeof createRoutingClient>;
/** Display-only projection. Never pass these coordinates back as backend results. */
export function projectRouteToWorld(route: RouteResult, original: readonly Point[]): RouteResult {
  if (original.length !== route.snapped.length)
    throw new RoutingError('InvalidInput', 'Waypoint count differs from snapped result');
  const offsets = original.map(p => p.lon - normalizePoint(p).lon);
  const offset = offsets.find(value => value !== 0) ?? 0;
  return { ...route, geometry: route.geometry.map(p => ({ ...p, lon: p.lon + offset })),
    snapped: route.snapped.map((p, i) => ({ ...p, lon: p.lon + offsets[i] })) };
}
export type RouteOutcome = { status: 'ok'; route: RouteResult } | { status: 'skipped' | 'superseded' };
/** One instance per interactive routing control. Direct client calls remain independent. */
export function createRouteController(client: Pick<RoutingClient, 'route'>, config: { now?: () => number } = {}) {
  const now = config.now ?? Date.now;
  let generation = 0;
  let active: AbortController | undefined;
  let failures = 0;
  let coolOff = 4000;
  let quietUntil = 0;
  return {
    cancel() { generation++; active?.abort(); active = undefined; },
    async route(coordinates: readonly Point[], options?: QueryOptions,
      request: { preview?: boolean } = {}): Promise<RouteOutcome> {
      // Even a dropped preview invalidates a response for an earlier pointer position.
      const id = ++generation;
      active?.abort(); active = undefined;
      if (request.preview && now() < quietUntil) return { status: 'skipped' };
      const controller = new AbortController();
      active = controller;
      try {
        const route = await client.route(coordinates, options, controller.signal);
        if (id !== generation) return { status: 'superseded' };
        failures = 0; coolOff = 4000; quietUntil = 0;
        return { status: 'ok', route };
      } catch (error) {
        if (id !== generation) return { status: 'superseded' };
        // Semantic failures (NoRoute, bad input, etc.) do not indicate overload.
        if (request.preview && error instanceof RoutingError &&
            (error.code === 'Network' || error.code === 'RateLimited')) {
          if (++failures >= 2) { quietUntil = now() + coolOff; coolOff = Math.min(coolOff * 2, 30000); }
        }
        throw error;
      } finally { if (id === generation) active = undefined; }
    },
  };
}
