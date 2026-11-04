import { describe, it, expect, vi } from 'vitest';
import type { Map } from 'maplibre-gl';
import type { RouteResult } from '@routeweft/routing-client';
const markers = vi.hoisted(() => [] as { removed: boolean; point?: number[]; popup?: { text: string } }[]);
vi.mock('maplibre-gl', () => ({ default: { Marker: class {
  removed = false;
  point?: number[]; popup?: { text: string };
  constructor() { markers.push(this); }
  setLngLat(point: number[]) { this.point = point; return this; }
  setPopup(popup: { text: string }) { this.popup = popup; return this; }
  togglePopup() { return this; }
  addTo() { return this; } remove() { this.removed = true; }
}, Popup: class { text = ''; setText(text: string) { this.text = text; return this; } } } }));
import { createRouteLayer } from '../src/index';
class FakeMap {
  sources = new Map<string, { data: unknown; setData(data: unknown): void }>();
  layers = new Map<string, unknown>();
  handlers = new Map<string, Set<(...args: any[]) => void>>();
  canvas = { style: { cursor: 'grab' } };
  loaded = true; dead = false;
  fitBounds = vi.fn(); flyTo = vi.fn();
  check() { if (this.dead) throw new Error('style used after removal'); }
  on(event: string, fn: (...args: any[]) => void) { const set = this.handlers.get(event) ?? new Set(); set.add(fn); this.handlers.set(event,set); }
  off(event: string, fn: (...args: any[]) => void) { this.handlers.get(event)?.delete(fn); }
  emit(event: string, value?: unknown) { this.handlers.get(event)?.forEach(fn => fn(value)); }
  getCanvas() { this.check(); return this.canvas; }
  isStyleLoaded() { this.check(); return this.loaded; }
  getSource(id: string) { this.check(); return this.sources.get(id); }
  getLayer(id: string) { this.check(); return this.layers.get(id); }
  addSource(id: string, source: { data: unknown }) { this.check(); this.sources.set(id,{data:source.data,setData(data){this.data=data;}}); }
  addLayer(layer: {id:string}) { this.check(); this.layers.set(layer.id,layer); }
  removeLayer(id:string) { this.check(); this.layers.delete(id); }
  removeSource(id:string) { this.check(); this.sources.delete(id); }
  reload() { this.sources.clear(); this.layers.clear(); this.emit('style.load'); }
  remove() { this.dead=true; this.emit('remove'); }
  map() { return this as unknown as Map; }
}
const route: RouteResult = {dataset_id:'fixture',profile:'driving-v1',format:2,distance_m:10,duration_s:1,
  geometry:[{lon:1,lat:0},{lon:2,lat:1}],snapped:[{lon:1,lat:0},{lon:2,lat:1}],legs:[],
  steps:[{maneuver:'depart',name:'road',exit:null,distance_m:10,duration_s:1,geometry_start:0,geometry_end:1}]};
describe('route layer lifecycle', () => {
  it('restores changed styles, isolates controls and removes only owned resources', () => {
    const map=new FakeMap();
    const one=createRouteLayer(map.map());const two=createRouteLayer(map.map(),{color:'red',fit:false});
    one.setRoute(route);two.setRoute(route);
    expect(map.layers.size).toBe(2);expect(map.fitBounds).toHaveBeenCalledTimes(1);
    map.reload();expect(map.layers.size).toBe(2);
    one.dispose();expect(map.layers.size).toBe(1);two.dispose();expect(map.sources.size).toBe(0);
    expect([...map.handlers.values()].every(set=>set.size===0)).toBe(true);
  });
  it('handles style not yet loaded, selection, world copies, step focus and cursor restoration', () => {
    const map=new FakeMap();map.loaded=false;const picked=vi.fn();
    const layer=createRouteLayer(map.map(),{onPick:picked});
    layer.setWaypoints([{lon:361,lat:0},{lon:362,lat:1}]);layer.setRoute(route);
    expect(map.layers.size).toBe(0);map.loaded=true;map.emit('style.load');
    const source=[...map.sources.values()][0];
    expect((source.data as any).geometry.coordinates[0]).toEqual([361,0]);
    expect(route.geometry[0].lon).toBe(1);
    layer.focusStep(0);expect(map.flyTo).toHaveBeenCalledWith({center:[361,0],zoom:16});
    layer.setPicking(true);expect(map.canvas.style.cursor).toBe('crosshair');
    map.emit('click',{lngLat:{lng:363,lat:2}});expect(picked).toHaveBeenCalledWith({lon:363,lat:2});
    layer.clear();expect(map.layers.size).toBe(0);expect(map.canvas.style.cursor).toBe('grab');
    expect(markers.every(marker=>marker.removed)).toBe(true);layer.dispose();
  });
  it('never touches destroyed style and tolerates repeated disposal', () => {
    const map=new FakeMap();const layer=createRouteLayer(map.map());layer.setRoute(route);map.remove();
    expect(()=>{layer.clear();layer.setRoute(route);layer.setWaypoints([]);layer.focusStep(0);layer.dispose();layer.dispose();}).not.toThrow();
    expect([...map.handlers.values()].every(set=>set.size===0)).toBe(true);
  });
  it('does not draw fallback geometry on no route; rejects colliding explicit IDs', () => {
    const map=new FakeMap();const layer=createRouteLayer(map.map(),{id:'shared'});
    layer.setWaypoints(route.snapped);expect(map.layers.size).toBe(0);
    layer.setRoute(route);const other=createRouteLayer(map.map(),{id:'shared'});
    expect(()=>other.setRoute(route)).toThrow('already exists');other.dispose();expect(map.layers.size).toBe(1);
    layer.setRoute(undefined);map.reload();expect(map.layers.size).toBe(0);layer.dispose();
  });
  it('separates unverified access gaps, keeps world copies and fits the original destination', () => {
    const map = new FakeMap();
    const layer = createRouteLayer(map.map(), { id: 'nearby' });
    const requested = [{ lon: 361, lat: 0 }, { lon: 362, lat: 1.001 }];
    layer.setWaypoints(requested); layer.setRoute(route);
    expect(map.layers.size).toBe(2);
    const gap = map.sources.get('nearby-access')!.data as any;
    expect(gap.features[0].geometry.coordinates).toEqual([[362, 1.001], [362, 1]]);
    expect(gap.features[0].properties.access_status).toBe('unknown');
    expect(map.layers.get('nearby-access')).toMatchObject({ paint: { 'line-dasharray': [2, 2] } });
    expect(layer.getAccessNotice()).toContain('导航至目的地附近');
    expect(layer.getAccessNotice()).toContain('不含连接段');
    expect(map.fitBounds).toHaveBeenLastCalledWith([[361, 0], [362, 1.001]], { padding: 60, maxZoom: 16 });
    expect(markers.filter(m => !m.removed).at(-1)?.popup?.text).toContain('通行未验证');
    map.reload(); expect(map.layers.size).toBe(2);
    expect(markers.filter(m => !m.removed)).toHaveLength(3);
    layer.clear(); expect(map.sources.size).toBe(0); expect(layer.getAccessNotice()).toBe('');
    expect(markers.every(m => m.removed)).toBe(true); layer.dispose();
  });
  it('uses backend requested coordinates without separate waypoint calls and cleans up after map removal', () => {
    const map = new FakeMap(); const layer = createRouteLayer(map.map(), { id: 'details' });
    const nearby: RouteResult = { ...route, snap_details: route.snapped.map((p, i) => ({
      requested_coordinate: { ...p, lat: p.lat + (i ? 0.001 : 0) }, snapped_coordinate: p,
      snap_distance_m: i ? 111.195 : 0, search_radius_m: 1000, snap_method: 'road_projection',
      fallback_used: false, access_status: i ? 'unknown' : 'on_network', arrival_status: i ? 'nearby' : 'on_network'
    })) };
    layer.setRoute(nearby);
    expect(map.sources.has('details-access')).toBe(true);
    expect(layer.getAccessNotice()).toContain('附近');
    expect(markers.filter(m => !m.removed).some(m => m.point?.[1] === 1.001)).toBe(true);
    map.remove(); expect(markers.every(m => m.removed)).toBe(true);
    expect(() => layer.dispose()).not.toThrow();
  });
  it('shows access gaps for a zero-length road route without inventing a road line', () => {
    const map = new FakeMap(); const layer = createRouteLayer(map.map(), { id: 'zero' });
    layer.setWaypoints([{ lon: 1, lat: -0.001 }, { lon: 1, lat: 0.001 }]);
    layer.setRoute({ ...route, geometry: [route.geometry[0]], snapped: [route.snapped[0], route.snapped[0]],
      distance_m: 0, duration_s: 0, steps: [] });
    expect(map.sources.has('zero')).toBe(false);
    expect((map.sources.get('zero-access')!.data as any).features).toHaveLength(2);
    expect(layer.getAccessNotice()).toContain('不含连接段');
    layer.dispose(); expect(map.sources.size).toBe(0);
  });
});
