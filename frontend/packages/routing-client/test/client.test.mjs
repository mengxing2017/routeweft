import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createRoutingClient, createRouteController, getRouteAccess, getRouteAccessNotice, normalizePoint, projectRouteToWorld, RoutingError } from '../dist/index.js';
const points = [{ lon: 361, lat: 0 }, { lon: 362, lat: 1 }];
const result = { dataset_id: 'fixture', profile: 'driving', format: 2, distance_m: 10,
  duration_s: 1, geometry: [{ lon: 1, lat: 0 }, { lon: 2, lat: 1 }],
  snapped: [{ lon: 1, lat: 0 }, { lon: 2, lat: 1 }], legs: [], steps: [] };
test('route and nearest use backend fields, defaults and copied normalized coordinates', async () => {
  const transport = {
    async route(request) {
      assert.deepEqual(request.coordinates, result.geometry);
      assert.deepEqual(request.options, { profile: 'driving', radius_m: 1000, candidates: 8 });
      return result;
    },
    async nearest(request) { assert.deepEqual(request.coordinate, { lon: 1, lat: 0 }); return []; }
  };
  const client = createRoutingClient(transport);
  assert.equal(await client.route(points), result);
  assert.deepEqual(await client.nearest(points[0]), []);
  assert.equal(points[0].lon, 361);
  assert.equal(normalizePoint({ lon: 180, lat: 85 }).lon, 180);
});
test('reject unsupported or invalid queries before transport', async () => {
  let calls = 0;
  const client = createRoutingClient({ route: async () => { calls++; return result; }, nearest: async () => { calls++; return []; } });
  for (const coordinates of [[], [points[0]], Array(11).fill(points[0]), [{ lon: Infinity, lat: 0 }, points[0]]])
    await assert.rejects(client.route(coordinates), { code: 'InvalidInput' });
  for (const options of [{ radius_m: 0 }, { radius_m: 10001 }, { candidates: 1.5 }, { candidates: 17 }])
    await assert.rejects(client.nearest(points[0], options), { code: 'InvalidInput' });
  await assert.rejects(client.route(points, { profile: 'walking' }), { code: 'UnsupportedProfile' });
  await assert.rejects(client.nearest({ lon: 0, lat: 91 }), { code: 'InvalidInput' });
  const abort = new AbortController(); abort.abort();
  await assert.rejects(client.route(points, undefined, abort.signal), { name: 'AbortError' });
  assert.equal(calls, 0);
});
test('world projection retains backend result and moves snapped points and geometry', () => {
  const display = projectRouteToWorld(result, points);
  assert.deepEqual(display.geometry, points);
  assert.deepEqual(display.snapped, points);
  assert.equal(result.geometry[0].lon, 1);
  assert.throws(() => projectRouteToWorld(result, []), { code: 'InvalidInput' });
});
test('access gaps retain raw destinations, exclude road ETA and support older transports', () => {
  const original = [{ lon: 1, lat: 0.001 }, { lon: 2, lat: 1.005 }];
  const access = getRouteAccess(result, original);
  assert.equal(access[0].access_status, 'unknown');
  assert.ok(Math.abs(access[0].snap_distance_m - 111.19508) < 0.001);
  assert.deepEqual(access[1].requested_coordinate, original[1]);
  assert.deepEqual(access[1].snapped_coordinate, result.snapped[1]);
  assert.match(getRouteAccessNotice(result, original), /道路导航从距起点/);
  assert.match(getRouteAccessNotice(result, original), /导航至目的地附近/);
  assert.match(getRouteAccessNotice(result, original), /不含连接段/);
  assert.equal(getRouteAccessNotice(result), '');
  assert.throws(() => getRouteAccess(result, []), { code: 'InvalidInput' });
  assert.equal(result.duration_s, 1);
});
test('world projection moves access metadata without mutating backend coordinates', () => {
  const nearby = { ...result, distance_scope: 'road_only', snap_details: result.snapped.map(p => ({
    requested_coordinate: { ...p, lat: p.lat + 0.001 }, snapped_coordinate: p,
    snap_distance_m: 111.195, search_radius_m: 1000, snap_method: 'road_projection',
    fallback_used: false, access_status: 'unknown', arrival_status: 'nearby'
  })) };
  const display = projectRouteToWorld(nearby, points);
  assert.equal(display.snap_details[0].requested_coordinate.lon, 361);
  assert.equal(display.snap_details[1].snapped_coordinate.lon, 362);
  assert.equal(nearby.snap_details[0].requested_coordinate.lon, 1);
  assert.match(getRouteAccessNotice(display), /附近/);
});
test('late responses are superseded even when transport ignores abort; cancellation is isolated', async () => {
  const pending = [];
  const client = createRoutingClient({ route: (_, signal) => new Promise(resolve => pending.push({ resolve, signal })) });
  const controller = createRouteController(client);
  const first = controller.route(points);
  const second = controller.route(points);
  assert.equal(pending[0].signal.aborted, true);
  pending[1].resolve(result);
  assert.equal((await second).status, 'ok');
  pending[0].resolve(result);
  assert.equal((await first).status, 'superseded');
  const third = controller.route(points);
  controller.cancel(); pending[2].resolve(result);
  assert.equal((await third).status, 'superseded');
});
test('preview failures back off, final request always runs, success resets', async () => {
  let calls = 0, time = 0, fail = true;
  const controller = createRouteController(createRoutingClient({ async route() {
    calls++; if (fail) throw new RoutingError('RateLimited', 'busy'); return result;
  } }), { now: () => time });
  for (let i = 0; i < 2; i++) await assert.rejects(controller.route(points, undefined, { preview: true }));
  assert.equal((await controller.route(points, undefined, { preview: true })).status, 'skipped');
  assert.equal(calls, 2);
  await assert.rejects(controller.route(points)); assert.equal(calls, 3);
  time = 4000; fail = false;
  assert.equal((await controller.route(points, undefined, { preview: true })).status, 'ok');
  assert.equal((await controller.route(points, undefined, { preview: true })).status, 'ok');
});
test('semantic errors never trigger preview backoff', async () => {
  let calls = 0;
  const controller = createRouteController(createRoutingClient({ async route() { calls++; throw new RoutingError('NoRoute', 'unreachable'); } }));
  for (let i = 0; i < 4; i++) await assert.rejects(controller.route(points, undefined, { preview: true }), { code: 'NoRoute' });
  assert.equal(calls, 4);
});
