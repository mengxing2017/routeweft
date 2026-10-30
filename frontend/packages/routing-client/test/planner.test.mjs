import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createRouteClient, createRoutePlanner, RoutingError } from '../dist/index.js';
const points = [{lon:361,lat:0},{lon:362,lat:1},{lon:363,lat:2}];
const result = { dataset_id:'fixture',profile:'driving-v1',format:2,distance_m:3,duration_s:1,
  geometry:[],snapped:[],legs:[],steps:[] };
test('route-only transport validates engine latitude, normalizes all waypoints, keeps original world', async () => {
  const planner = createRoutePlanner(createRouteClient({ async route(request) {
    assert.equal(request.coordinates.length,3); assert.equal(request.coordinates[0].lon,1); return result;
  }}));
  planner.setPoints(points); await planner.calculate();
  assert.equal(planner.getSnapshot().points[0].lon,361);
  assert.equal(planner.getSnapshot().route,result);
  assert.throws(() => planner.setPoint(0,{lon:0,lat:85.01}),{code:'InvalidInput'});
  planner.dispose();
});
test('edits, clear, swap, cancellation and teardown suppress late successes and errors', async () => {
  for (const action of ['clear','swap','cancel','dispose','edit']) {
    for (const fail of [false,true]) {
      let resolve, reject;
      const planner = createRoutePlanner(createRouteClient({route:() => new Promise((yes,no) => {resolve=yes; reject=no;})}));
      planner.setPoints(points);
      const pending=planner.calculate();
      if(action==='edit') planner.setPoint(0,{lon:4,lat:4}); else planner[action]();
      const before=planner.getSnapshot();
      if(fail) reject(new RoutingError('NoRoute','unreachable')); else resolve(result);
      await pending;
      assert.equal(planner.getSnapshot(),before,action);
      assert.equal(planner.getSnapshot().route,undefined);
    }
  }
});
test('newer request wins when transport ignores abort; subscriptions unsubscribe',async()=>{
  const pending=[];let changes=0;
  const planner=createRoutePlanner(createRouteClient({route:()=>new Promise(resolve=>pending.push(resolve))}));
  const unsubscribe=planner.subscribe(()=>changes++);
  planner.setPoints(points); const first=planner.calculate(); const second=planner.calculate();
  pending[1](result);await second;pending[0]({...result,distance_m:99});await first;
  assert.equal(planner.getSnapshot().route,result);assert.equal(planner.getSnapshot().busy,false);
  unsubscribe();const before=changes;planner.clear();assert.equal(changes,before);
});
