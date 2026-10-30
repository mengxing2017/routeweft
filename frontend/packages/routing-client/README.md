# @routeweft/routing-client

供下游前端使用的独立 ESM / TypeScript 包，无运行时依赖，不要求 React、Leaflet 或 DOM 页面。运行环境须支持 AbortController（现代浏览器或 Node.js 18+）。

## 安装

发布到目标 npm registry 后，下游固定版本安装：

```sh
npm install --save-exact @routeweft/routing-client@0.1.0-pre
```

私有 registry 配置、完整 transport/planner 示例见[外部使用指南](https://github.com/mengxing2017/routeweft/blob/main/docs/frontend-usage.md)。

## 本地构建与打包

```sh
cd frontend/packages/routing-client
npm ci
npm test
npm pack
# 在下游工程中安装上一步生成的包（替换实际路径）
npm install /path/to/routeweft-routing-client-0.1.0-pre.tgz
```

包包含编译后的 JS、类型声明和第三方声明。支持显式指定公共或私有 npm registry；项目原创代码许可维持第三方声明中的现有说明。

## 下游接入

后端只有 Rust Engine / CLI，**没有 HTTP 服务，也不实现 OSRM HTTP 协议**。宿主实现 `RoutingTransport`，负责把请求转给嵌入 Engine 的服务或 IPC，并验证返回 JSON、处理认证和错误。不要在浏览器运行 CLI，也不要将本包请求直接发给 OSRM。

```ts
import {
  createRoutingClient, createRouteController, projectRouteToWorld,
  type RoutingTransport,
} from '@routeweft/routing-client';

// hostRouting 是下游提供且符合 RoutingTransport 的 HTTP/IPC 桥接实例。
export function createMapRouting(hostRouting: RoutingTransport) {
  const client = createRoutingClient(hostRouting);
  const controller = createRouteController(client);
  return { client, controller };
}

// 在下游代码中：
// const { client, controller } = createMapRouting(hostRouting);
// const coordinates = [{ lon: 7.4206, lat: 43.7311 }, { lon: 7.4269, lat: 43.7384 }];
// const outcome = await controller.route(coordinates, { radius_m: 1000 });
// if (outcome.status === 'ok') renderRoute(projectRouteToWorld(outcome.route, coordinates));
// const matches = await client.nearest(coordinates[0]);
// controller.cancel(); // 页面卸载或放弃查询
```

`route` 接收 2～10 个连续途经点，必须一次提交以保留跨途经点转向历史。仅支持 driving。`radius_m` 默认 1000、范围 (0,10000]；`candidates` 默认 8、范围 1～16。坐标使用 `{lon,lat}`，经度归一化，纬度限定 [-85,85]。距离为米、时长为秒；结果字段与 `routeweft-routing/src/roads.rs` 中 Serialize 类型一致，不伪造 OSRM alternatives、lanes 或其他未实现功能。宿主负责 SearchBudget 和数据集生命周期，不通过前端承诺引擎执行预算。

`client.route/nearest` 是独立查询，不互相取消，可传 AbortSignal。`controller` 每实例代表一个交互控件，只保留最新请求，旧请求完成返回 `superseded`；即使传输层忽略取消也不会返回可渲染的旧结果。底层 Promise 仍需宿主最终完成/拒绝，取消不能强行终止不配合的传输。拖拽时传第三参数 `{preview:true}`，连续两次 Network/RateLimited 错误后退避 4 秒，后续失败倍增至 30 秒；退避内返回 `skipped`，调用方保留现有路线。拖拽结束用默认调用，始终发起请求。成功清除退避。宿主应将网络/限流错误映射为 `RoutingError`，其他异常原样抛出。

`projectRouteToWorld` 按上游策略将几何移到第一个非零偏移的世界副本，吸附点使用各自偏移，不修改原始结果；它不是跨日期变更线几何切割算法。渲染后的坐标仅供显示。原始类型与接口不依赖任何地图组件。

抽取来源、修改说明和许可见 THIRD_PARTY_NOTICES.md 与 LICENSE.osrm-frontend。

## 远程发布与版本

版本由本包 package.json 管理。仓库根目录配置 NPM_REGISTRY / NPM_TOKEN 后：

```sh
make release-frontend-dry-run
make release-frontend-publish
```

默认验证并按依赖顺序发布 client 和 MapLibre adapter。公共仓库使用显式 `--access public`。配置、版本升级、预发布与失败续发见[发布指南](https://github.com/mengxing2017/routeweft/blob/main/docs/frontend-release.md)。

## 接口说明

`createRouteClient` accepts a route-only transport; `createRoutingClient` still requires route and nearest. Controllers accept either client's route capability. Coordinates are checked against the Rust engine latitude range [-85, 85].

`createRoutePlanner` exposes immutable snapshots/subscriptions, 2–10 waypoint editing, picking, swap, clear, cancellation and calculation. Generation checks suppress stale results and errors even when the transport ignores AbortSignal. The planner retains original world coordinates; the client normalizes only the request.

MapLibre rendering lives in the optional `@routeweft/maplibre` peer adapter. See its runnable example and resource lifecycle API.
