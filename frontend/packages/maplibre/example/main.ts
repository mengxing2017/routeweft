import maplibregl from 'maplibre-gl';
import 'maplibre-gl/dist/maplibre-gl.css';
import { createRouteClient, createRoutePlanner, RoutingError, type RouteTransport, type Point } from '@routeweft/routing-client';
import { createRouteLayer } from '../src/index';
const points: Point[] = [{ lon: 108.3, lat: 22.8 }, { lon: 108.31, lat: 22.805 }];
// A host can replace this fixture transport with its authenticated HTTP/IPC adapter.
const transport: RouteTransport = { async route(request) {
  if (JSON.stringify(request.coordinates) !== JSON.stringify(points)) throw new RoutingError('NoRoute', 'Only the fixture endpoints are available');
  return { dataset_id: 'synthetic-example', profile: 'driving-v1', format: 2, distance_m: 1200, duration_s: 120,
    geometry: [points[0], { lon: 108.305, lat: 22.8 }, { lon: 108.305, lat: 22.805 }, points[1]],
    snapped: points, legs: [], steps: [] };
} };
const map = new maplibregl.Map({ container: 'map', center: [108.305, 22.8025], zoom: 14,
  style: { version: 8, sources: {}, layers: [{ id: 'background', type: 'background', paint: { 'background-color': '#eff4f0' } }] } });
const planner = createRoutePlanner(createRouteClient(transport));
const layer = createRouteLayer(map);
const unsubscribe = planner.subscribe(() => {
  const state = planner.getSnapshot();
  layer.setRoute(undefined); layer.setWaypoints(state.points); layer.setRoute(state.route);
  document.querySelector('#status')!.textContent = state.busy ? 'Calculating…' : state.error ? String(state.error) : state.route ? 'Fixture route displayed' : '';
});
const calculate = () => { planner.setPoints(points); void planner.calculate(); };
document.querySelector('#calculate')!.addEventListener('click', calculate);
document.querySelector('#clear')!.addEventListener('click', () => planner.clear());
map.once('load', calculate);
window.addEventListener('pagehide', () => { unsubscribe(); planner.dispose(); layer.dispose(); map.remove(); }, { once: true });
