# @routeweft/routing-client

供下游前端使用的独立 ESM / TypeScript 包，无运行时依赖，不要求 React、Leaflet 或 DOM 页面。运行环境须支持 AbortController（现代浏览器或 Node.js 18+）。

## 构建与安装

```sh
cd frontend/packages/routing-client
npm ci
npm test
npm pack
# 在下游工程中安装上一步生成的包（替换实际路径）
npm install /path/to/routeweft-routing-client-0.1.0.tgz
```

包包含编译后的 JS、类型声明和第三方声明。没有公共发布配置；项目原创代码许可尚未确定。

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

`route` 接收 2～10 个连续途经点，必须一次提交以保留跨途经点转向历史。仅支持 driving。`radius_m` 默认 1000、范围 (0,10000]；`candidates` 默认 8、范围 1～16。坐标使用 `{lon,lat}`，经度归一化，纬度限定 [-90,90]。距离为米、时长为秒；结果字段与 `routeweft-routing/src/roads.rs` 中 Serialize 类型一致，不伪造 OSRM alternatives、lanes 或其他未实现功能。宿主负责 SearchBudget 和数据集生命周期，不通过前端承诺引擎执行预算。

`client.route/nearest` 是独立查询，不互相取消，可传 AbortSignal。`controller` 每实例代表一个交互控件，只保留最新请求，旧请求完成返回 `superseded`；即使传输层忽略取消也不会返回可渲染的旧结果。底层 Promise 仍需宿主最终完成/拒绝，取消不能强行终止不配合的传输。拖拽时传第三参数 `{preview:true}`，连续两次 Network/RateLimited 错误后退避 4 秒，后续失败倍增至 30 秒；退避内返回 `skipped`，调用方保留现有路线。拖拽结束用默认调用，始终发起请求。成功清除退避。宿主应将网络/限流错误映射为 `RoutingError`，其他异常原样抛出。

`projectRouteToWorld` 按上游策略将几何移到第一个非零偏移的世界副本，吸附点使用各自偏移，不修改原始结果；它不是跨日期变更线几何切割算法。渲染后的坐标仅供显示。原始类型与接口不依赖任何地图组件。

抽取来源、修改说明和许可见 THIRD_PARTY_NOTICES.md 与 LICENSE.osrm-frontend。

## 发布到私有 npm 仓库

在仓库根目录配置 `NPM_REGISTRY_INTERNAL` 和 `NPM_TOKEN_INTERNAL` 后执行：

```sh
make release-frontend-dry-run
make release-frontend-publish
```

发布当前工作区的包，先测试和重新打包，再上传。配置方法及 Devbox 入口见
[私有发布说明](../../../docs/private-release.md#前端-npm-包发布)。
