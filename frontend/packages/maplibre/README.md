# @routeweft/maplibre

Optional MapLibre 5 adapter. The host supplies an authenticated transport and a map instance.

```ts
import { createRouteClient, createRoutePlanner } from '@routeweft/routing-client';
import { createRouteLayer } from '@routeweft/maplibre';
const planner = createRoutePlanner(createRouteClient(transport));
const layer = createRouteLayer(map, { onPick: point => {
  const index = planner.getSnapshot().picking;
  if (index !== null) planner.setPoint(index, point);
} });
const unsubscribe = planner.subscribe(() => {
  const state = planner.getSnapshot();
  layer.setRoute(undefined);
  layer.setWaypoints(state.points);
  layer.setRoute(state.route);
  layer.setPicking(state.picking !== null);
});
// Host teardown: unsubscribe(); planner.dispose(); layer.dispose();
```

Default IDs isolate instances. Explicit IDs must be unique in the map style.
Route state is restored after `style.load`; no straight-line fallback is drawn.
World copies affect display only. Dispose before or after map removal.

Run the local synthetic fixture with `pnpm --dir frontend install`, build routing-client, then `pnpm --dir frontend/packages/maplibre example`. The example uses a blank map and an explicitly supplied local fixture transport; it never contacts a routing service.
