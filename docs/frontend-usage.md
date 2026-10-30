# 外部工程使用前端接口包

两个库独立于本仓库的 React 首页使用，不需要引入 Ops DTO、UI 框架或 Rust 源码。包是 ESM，包含 TypeScript 类型。client 使用现代浏览器或 Node.js 18+；发布构建使用 Node.js 22。MapLibre 适配器在浏览器中使用，需要宿主提供 MapLibre GL JS 5 与地图实例。

## 安装与版本

| 用途 | 安装 |
| --- | --- |
| 仅计算路线、最近道路或交互状态 | `npm install --save-exact @routeweft/routing-client@0.1.0-pre` |
| 使用 MapLibre 图层 | `npm install --save-exact @routeweft/routing-client@0.1.0-pre @routeweft/maplibre@0.1.0-pre maplibre-gl@5.24.0` |

上述是本仓库当前版本；须先发布到所选 registry，不能把 manifest 版本当成远程已存在。pnpm 用户可换成 `pnpm add --save-exact`。提交宿主锁文件，升级前查看对应 README 的迁移说明。

私有仓库的下游项目 `.npmrc` 示例（将地址换为实际 npm endpoint，使用只读 token）：

```ini
@routeweft:registry=https://packages.example.com/repository/npm-hosted/
//packages.example.com/repository/npm-hosted/:_authToken=${NPM_TOKEN}
```

token 从本地或 CI 环境注入，**不要放进前端源码、Vite 环境或浏览器 bundle**。scope 映射使其他依赖继续从宿主默认 registry 安装。公共 npmjs 发布通常无需额外 scope 映射；检查不要残留指向旧私有仓库的同名 scope 配置。[npmrc 官方说明](https://docs.npmjs.com/cli/v11/configuring-npm/npmrc/)

## 宿主传输契约

本包不内置 HTTP URL、认证或 fetch 适配器；RouteWeft Rust Engine/CLI 也不提供 OSRM HTTP 服务。宿主实现 `RouteTransport`（仅 route）或 `RoutingTransport`（route + nearest），负责 HTTP/IPC 调用、返回结构校验、认证、数据集选择和错误映射。

```ts
import {
  createRouteClient, createRoutingClient, RoutingError,
  type RouteTransport, type RoutingTransport,
} from '@routeweft/routing-client';

// routeBridge 是宿主提供的已验证 HTTP/IPC 实现，不是库内置端点。
export async function calculateRoute(routeBridge: RouteTransport) {
  const client = createRouteClient(routeBridge);
  const abort = new AbortController();
  try {
    return await client.route(
      [{ lon: 113.2644, lat: 23.1291 }, { lon: 113.3245, lat: 23.1065 }],
      { profile: 'driving', radius_m: 1000, candidates: 8 },
      abort.signal,
    );
  } catch (error) {
    if (error instanceof RoutingError && error.code === 'NoRoute') return undefined;
    throw error;
  }
}

export async function findNearest(bridge: RoutingTransport) {
  return createRoutingClient(bridge).nearest({ lon: 113.2644, lat: 23.1291 });
}
```

宿主的 `route(request, signal)` 返回 `Promise<RouteResult>`，`nearest(request, signal)` 返回 `Promise<Nearest[]>`。TypeScript 类型不是运行时 JSON 校验器；不要通过 `as RouteResult` 代替宿主协议验证。AbortSignal 应传递到 HTTP/IPC，取消是否终止后端工作由宿主协作实现。

| 契约 | 值 |
| --- | --- |
| 坐标 | `{ lon, lat }`，经度归一化到 [-180,180]；纬度 [-85,85]；必须有限数 |
| route | 2～10 个途经点一次提交，保留跨途经点转向历史 |
| profile | 仅 `driving`；结果 profile 如 `driving-v1` 是后端规则身份 |
| `radius_m` | 默认 1000，范围 (0,10000] |
| `candidates` | 默认 8，整数 1～16 |
| 结果单位 | `distance_m` 米，`duration_s` 秒；geometry 为 Point 数组 |
| 数据身份 | 返回 `dataset_id`、`profile`、`format`；数据集由宿主管理 |

`RouteResult` 同时含 `legs`、`steps` 和 `snapped`；最近道路结果含 edge、吸附点、fraction、distance_m、from、to。以包内 `.d.ts` 为完整字段契约，不伪造 OSRM alternatives、lanes 等字段。

## 交互请求与 planner

普通 client 调用彼此独立。需要拖拽预览时使用 `createRouteController(client)`：每个交互控件一个实例，旧结果返回 `superseded`；preview 在连续 Network/RateLimited 后退避，返回 `skipped` 时保留已有渲染。拖拽结束使用默认请求，不能一直停留在 preview 模式。

表单/点选场景可使用 planner：

```ts
import { createRouteClient, createRoutePlanner, type RouteTransport } from '@routeweft/routing-client';

export function createPlanner(bridge: RouteTransport) {
  const planner = createRoutePlanner(createRouteClient(bridge));
  const unsubscribe = planner.subscribe(() => {
    const { points, picking, busy, route, error } = planner.getSnapshot();
    // 宿主根据快照更新 UI；错误保存在 error，不会被 calculate 自动重新抛出。
    console.log({ points, picking, busy, route, error });
  });
  planner.setPoints([{ lon: 113.2644, lat: 23.1291 }, { lon: 113.3245, lat: 23.1065 }]);
  void planner.calculate();
  return { planner, dispose() { unsubscribe(); planner.dispose(); } };
}
```

planner 提供 setPoint/setPoints、pick、swap、clear、cancel、calculate、getSnapshot/subscribe、dispose。编辑会取消并失效旧计算，避免过期成功或错误覆盖新状态；快照按只读状态使用。组件卸载时取消订阅并 dispose，避免悬挂请求和监听器。

宿主把网络/限流错误映射为 `RoutingError('Network' | 'RateLimited', message)`；语义错误如 NoRoute、InvalidInput、BudgetExceeded 不应都转成网络错误。前端不承诺 SearchBudget 或执行时间，后端/宿主负责实际预算。完整 ErrorCode 以 `.d.ts` 为准。

## MapLibre 图层

宿主先创建地图与样式，并导入 MapLibre CSS。样式/瓦片地址由宿主配置；库不使用演示路由或瓦片服务器。下面接收已创建的地图和传输实例：

```ts
import type { Map } from 'maplibre-gl';
import 'maplibre-gl/dist/maplibre-gl.css';
import { createRouteClient, createRoutePlanner, type RouteTransport } from '@routeweft/routing-client';
import { createRouteLayer } from '@routeweft/maplibre';

export function attachRouting(map: Map, bridge: RouteTransport) {
  const planner = createRoutePlanner(createRouteClient(bridge));
  const layer = createRouteLayer(map, {
    fit: { padding: 40, maxZoom: 15 },
    onPick(point) {
      const index = planner.getSnapshot().picking;
      if (index !== null) planner.setPoint(index, point);
    },
  });
  const render = () => {
    const state = planner.getSnapshot();
    layer.setWaypoints(state.points);
    layer.setRoute(state.route);
    layer.setPicking(state.picking !== null);
  };
  const unsubscribe = planner.subscribe(render);
  render();
  // 宿主调用 planner.pick(0/1)、setPoint(...) 和 calculate() 驱动交互。
  return {
    planner,
    focusStep: (index: number) => layer.focusStep(index),
    dispose() { unsubscribe(); planner.dispose(); layer.dispose(); },
  };
}
```

RouteLayer 只管理自身 source/layer/marker 与监听器，地图由宿主销毁。默认资源 ID 隔离多个实例；显式 id 不能与样式或其他控件重复。样式切换后会恢复路线，NoRoute/空结果不会画起终点直线。`dispose()` 可在地图销毁前后执行。

`projectRouteToWorld` 与图层的世界副本处理仅用于展示，不改后端数据，也不是跨日期变更线切割算法。计算仍使用规范化经纬度，保留原始途经点用于显示。

## 升级和问题定位

当前 `0.1.0-pre` 接口：只需 route 的宿主可实现 RouteTransport 并用 createRouteClient；原 createRoutingClient 仍要求 nearest。新增 planner 不要求任何 UI 框架，MapLibre 已分离到可选包。纬度范围以当前实现 [-85,85] 为准。

安装 404/403 先检查 registry、scope 映射、版本是否已发布及 token 读权限；类型无法解析检查 ESM/Bundler 配置和包内 dist/index.d.ts；地图不显示先检查宿主样式已加载、route.geometry 和 MapLibre CSS。传输协议/认证错误由宿主排查，不向公共 OSRM 服务发送本包的请求。

维护者发布步骤见[远程发布指南](frontend-release.md)。包内 README 与第三方声明随 npm tarball 分发。
