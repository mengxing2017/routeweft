# @routeweft/maplibre

MapLibre GL JS 5 适配器。宿主提供地图实例及已认证的 route transport。

发布到目标 npm registry 后安装：

```sh
npm install --save-exact @routeweft/routing-client@<version> @routeweft/maplibre@<version> maplibre-gl@5.24.0
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
Route state is restored after `style.load`; no route is invented when routing fails.
World copies affect display only. Dispose before or after map removal.

路外起终点会显示灰色虚线、道路吸附点及“连接示意，通行未验证”的提示；道路仍使用独立实线。原始点通过 `setWaypoints` 保留；只传新引擎的 `snap_details` 也可显示网外原始点。镜头同时覆盖原始点和道路位置，包括零道路长度的合法结果。虚线不是步行导航，不计入道路里程/耗时。

宿主可在结果面板使用 `layer.getAccessNotice()` 显示“导航至目的地附近”等说明，清除路线后返回空串。示例页面已接入该提示。新引擎的附近候选与详细语义见后端 `docs/api.md`；本适配器也支持旧响应配合 `setWaypoints` 显示偏移。

Run the local synthetic fixture with `pnpm --dir frontend install`, build routing-client, then `pnpm --dir frontend/packages/maplibre example`. The example uses a blank map and an explicitly supplied local fixture transport; it never contacts a routing service.

## 版本与发布

版本由本包 package.json 管理，依赖 client 精确版本。仓库根目录使用 `make release-frontend-dry-run` / `make release-frontend-publish`，配置 NPM_REGISTRY / NPM_TOKEN。公共/私有仓库、版本升级及失败续发见[发布指南](https://github.com/mengxing2017/routeweft/blob/main/docs/frontend-release.md)。

`<version>` 请替换为已发布的前端统一版本。源码版本来源为 [frontend/package.json](../../package.json)，由统一版本命令同步包清单。
