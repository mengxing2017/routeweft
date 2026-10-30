# @routeweft/maplibre

MapLibre GL JS 5 适配器，版本 `0.1.0-pre`。宿主提供地图实例及已认证的 route transport。

发布到目标 npm registry 后安装：

```sh
npm install --save-exact @routeweft/routing-client@0.1.0-pre @routeweft/maplibre@0.1.0-pre maplibre-gl@5.24.0
```

私有 scope 配置、完整类型化示例和清理流程见[外部使用指南](https://github.com/mengxing2017/routeweft/blob/main/docs/frontend-usage.md)。地图须由宿主创建，加载样式并导入 `maplibre-gl/dist/maplibre-gl.css`。以下 transport 和 map 均由宿主提供：

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

## 版本与发布

版本由本包 package.json 管理，依赖 client 精确版本。仓库根目录使用 `make release-frontend-dry-run` / `make release-frontend-publish`，配置 NPM_REGISTRY / NPM_TOKEN。公共/私有仓库、版本升级及失败续发见[发布指南](https://github.com/mengxing2017/routeweft/blob/main/docs/frontend-release.md)。
