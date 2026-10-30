import type { Feature, LineString } from 'geojson';
import maplibregl, { type Marker, type Map, type MapMouseEvent, type GeoJSONSource } from 'maplibre-gl';
import { projectRouteToWorld, type Point, type RouteResult } from '@routeweft/routing-client';
let serial = 0;
export interface RouteLayerOptions {
  id?: string; color?: string; waypointColors?: readonly string[];
  fit?: false | { padding?: number; maxZoom?: number };
  onPick?: (point: Point) => void;
}
/** Owns only its sources, layers, markers and event listeners. Host owns the map. */
export function createRouteLayer(map: Map, options: RouteLayerOptions = {}) {
  const id = options.id ?? `routeweft-route-${++serial}`;
  let route: RouteResult | undefined;
  let waypoints: readonly (Point | undefined)[] = [];
  let markers: Marker[] = [];
  let dead = false;
  let disposed = false;
  let picking = false;
  let previousCursor = '';
  let ownsSource = false;
  let ownsLayer = false;
  const display = () => route ? projectRouteToWorld(route, (waypoints.length ? waypoints.filter((p): p is Point => p !== undefined) : route.snapped)) : undefined;
  const removeLine = () => {
    if (dead) return;
    if (ownsLayer && map.getLayer(id)) map.removeLayer(id);
    if (ownsSource && map.getSource(id)) map.removeSource(id);
    ownsLayer = false; ownsSource = false;
  };
  const draw = () => {
    if (dead || disposed || !map.isStyleLoaded()) return;
    const current = display();
    if (!current || current.geometry.length < 2) { removeLine(); return; }
    const data: Feature<LineString> = { type: 'Feature', properties: {},
      geometry: { type: 'LineString', coordinates: current.geometry.map(p => [p.lon, p.lat]) } };
    // A style replacement drops resources. Recreate them using retained route state.
    if (!map.getSource(id)) { map.addSource(id, { type: 'geojson', data }); ownsSource = true; }
    else if (ownsSource) (map.getSource(id) as GeoJSONSource).setData(data);
    else throw new Error(`Route source ID already exists: ${id}`);
    if (!map.getLayer(id)) {
      map.addLayer({ id, type: 'line', source: id, layout: { 'line-join': 'round', 'line-cap': 'round' },
        paint: { 'line-color': options.color ?? '#1677ff', 'line-width': 6, 'line-opacity': 0.85 } });
      ownsLayer = true;
    } else if (!ownsLayer) throw new Error(`Route layer ID already exists: ${id}`);
  };
  const clearMarkers = () => { markers.forEach(marker => marker.remove()); markers = []; };
  const click = (event: MapMouseEvent) => { if (picking && !dead && !disposed) options.onPick?.({ lon: event.lngLat.lng, lat: event.lngLat.lat }); };
  const removed = () => { dead = true; clearMarkers(); };
  map.on('style.load', draw); map.on('remove', removed); map.on('click', click);
  const setPicking = (value: boolean) => {
    if (dead || disposed || picking === value) return;
    const canvas = map.getCanvas();
    if (value) { previousCursor = canvas.style.cursor; canvas.style.cursor = 'crosshair'; }
    else canvas.style.cursor = previousCursor;
    picking = value;
  };
  return {
    setPicking,
    setWaypoints(points: readonly (Point | undefined)[]) {
      if (dead || disposed) return;
      waypoints = points.map(p => p ? { ...p } : undefined); clearMarkers();
      markers = waypoints.flatMap((point, index) => point ? [new maplibregl.Marker({ color: options.waypointColors?.[index] ?? (index === 0 ? '#16803d' : '#d43131') })
        .setLngLat([point.lon, point.lat]).addTo(map)] : []);
      draw();
    },
    setRoute(value?: RouteResult) {
      if (dead || disposed) return;
      route = value; draw();
      const current = display();
      if (current?.geometry.length && options.fit !== false) {
        let west = Infinity, south = Infinity, east = -Infinity, north = -Infinity;
        for (const p of current.geometry) { west = Math.min(west, p.lon); east = Math.max(east, p.lon); south = Math.min(south, p.lat); north = Math.max(north, p.lat); }
        map.fitBounds([[west, south], [east, north]],
          { padding: options.fit?.padding ?? 60, maxZoom: options.fit?.maxZoom ?? 16 });
      }
    },
    focusStep(index: number) {
      if (dead || disposed) return;
      const current = display(); const step = current?.steps[index];
      const point = step ? current?.geometry[step.geometry_start] : undefined;
      if (point) map.flyTo({ center: [point.lon, point.lat], zoom: 16 });
    },
    clear() { if (dead || disposed) return; route = undefined; waypoints = []; clearMarkers(); removeLine(); setPicking(false); },
    dispose() {
      if (disposed) return;
      setPicking(false); clearMarkers(); removeLine(); disposed = true;
      map.off('style.load', draw); map.off('remove', removed); map.off('click', click);
    },
  };
}
