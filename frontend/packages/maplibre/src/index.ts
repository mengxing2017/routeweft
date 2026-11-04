import type { Feature, FeatureCollection, LineString } from 'geojson';
import maplibregl, { type Marker, type Map, type MapMouseEvent, type GeoJSONSource } from 'maplibre-gl';
import { getRouteAccess, getRouteAccessNotice, projectRouteToWorld, type Point, type RouteResult } from '@routeweft/routing-client';
let serial = 0;
export interface RouteLayerOptions {
  id?: string; color?: string; waypointColors?: readonly string[];
  fit?: false | { padding?: number; maxZoom?: number };
  onPick?: (point: Point) => void;
}
/** Owns only its sources, layers, markers and event listeners. Host owns the map. */
export function createRouteLayer(map: Map, options: RouteLayerOptions = {}) {
  const id = options.id ?? `routeweft-route-${++serial}`;
  const accessId = `${id}-access`;
  let route: RouteResult | undefined;
  let waypoints: readonly (Point | undefined)[] = [];
  let markers: Marker[] = [];
  let accessMarkers: Marker[] = [];
  let ownsAccessSource = false;
  let ownsAccessLayer = false;
  let dead = false;
  let disposed = false;
  let picking = false;
  let previousCursor = '';
  let ownsSource = false;
  let ownsLayer = false;
  const original = () => waypoints.length ? waypoints.filter((p): p is Point => p !== undefined) : undefined;
  const display = () => route ? projectRouteToWorld(route, original() ?? route.snap_details?.map(d => d.requested_coordinate) ?? route.snapped) : undefined;
  const removeAccess = () => {
    accessMarkers.forEach(marker => marker.remove()); accessMarkers = [];
    if (dead) return;
    if (ownsAccessLayer && map.getLayer(accessId)) map.removeLayer(accessId);
    if (ownsAccessSource && map.getSource(accessId)) map.removeSource(accessId);
    ownsAccessLayer = false; ownsAccessSource = false;
  };
  const removeRoad = () => {
    if (dead) return;
    if (ownsLayer && map.getLayer(id)) map.removeLayer(id);
    if (ownsSource && map.getSource(id)) map.removeSource(id);
    ownsLayer = false; ownsSource = false;
  };
  const removeLine = () => { removeAccess(); removeRoad(); };
  const draw = () => {
    if (dead || disposed || !map.isStyleLoaded()) return;
    const current = display();
    if (!current || !current.geometry.length) { removeLine(); return; }
    if (current.geometry.length >= 2) {
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
    } else removeRoad();
    const access = getRouteAccess(current, original()).map((d, index) => ({ ...d, index }))
      .filter(d => d.access_status === 'unknown');
    if (!access.length) { removeAccess(); return; }
    const connections: FeatureCollection<LineString> = { type: 'FeatureCollection', features: access.map(d => ({
      type: 'Feature', properties: { access_status: 'unknown', distance_m: d.snap_distance_m, waypoint_index: d.index },
      geometry: { type: 'LineString', coordinates: [d.requested_coordinate, d.snapped_coordinate].map(p => [p.lon, p.lat]) }
    })) };
    if (!map.getSource(accessId)) { map.addSource(accessId, { type: 'geojson', data: connections }); ownsAccessSource = true; }
    else if (ownsAccessSource) (map.getSource(accessId) as GeoJSONSource).setData(connections);
    else throw new Error(`Route source ID already exists: ${accessId}`);
    if (!map.getLayer(accessId)) {
      map.addLayer({ id: accessId, type: 'line', source: accessId,
        paint: { 'line-color': '#666666', 'line-width': 2, 'line-dasharray': [2, 2] } }); ownsAccessLayer = true;
    } else if (!ownsAccessLayer) throw new Error(`Route layer ID already exists: ${accessId}`);
    accessMarkers.forEach(marker => marker.remove());
    accessMarkers = access.flatMap(d => {
      const label = d.index === 0 ? '道路导航起点' : d.index === current.snapped.length - 1 ? '目的地附近终点' : '途经点道路位置';
      const snapped = new maplibregl.Marker({ color: '#666666' })
        .setLngLat([d.snapped_coordinate.lon, d.snapped_coordinate.lat])
        .setPopup(new maplibregl.Popup({ closeOnClick: false }).setText(
          `${label}，距标记约 ${Math.ceil(d.snap_distance_m)} 米。连接示意，通行未验证。`))
        .addTo(map).togglePopup();
      return original() ? [snapped] : [snapped, new maplibregl.Marker({ color: d.index === 0 ? '#16803d' : '#d43131' })
        .setLngLat([d.requested_coordinate.lon, d.requested_coordinate.lat]).addTo(map)];
    });
  };
  const clearMarkers = () => { markers.forEach(marker => marker.remove()); markers = []; };
  const click = (event: MapMouseEvent) => { if (picking && !dead && !disposed) options.onPick?.({ lon: event.lngLat.lng, lat: event.lngLat.lat }); };
  const removed = () => { dead = true; clearMarkers(); removeAccess(); };
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
        for (const p of [...current.geometry, ...getRouteAccess(current, original()).flatMap(d => [d.requested_coordinate, d.snapped_coordinate])]) { west = Math.min(west, p.lon); east = Math.max(east, p.lon); south = Math.min(south, p.lat); north = Math.max(north, p.lat); }
        map.fitBounds([[west, south], [east, north]],
          { padding: options.fit?.padding ?? 60, maxZoom: options.fit?.maxZoom ?? 16 });
      }
    },
    getAccessNotice() { return route ? getRouteAccessNotice(route, original()) : ''; },
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
