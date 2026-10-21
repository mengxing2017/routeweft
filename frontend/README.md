# RouteWeft 前端

采用 React、TypeScript 和 Vite，独立于后端 Rust workspace 构建。

在仓库根目录执行：

```bash
pnpm --dir frontend install
make web-dev
```

默认开发地址为 `http://127.0.0.1:5173`。检查与生产构建：

```bash
pnpm --dir frontend typecheck
pnpm --dir frontend build
```

构建结果输出到 `frontend/dist/`。当前仅提供首页入口，尚未集成地图和路由 API，不会向上游演示服务器发送请求。后续按页面/功能组织 UI，将 HTTP 请求封装在前端服务模块，通过后端公布的 API 契约交互；业务规则仍由后端领域层维护。

## 下游接口包

[`@routeweft/routing-client`](packages/routing-client/README.md) 提供独立可打包的 TypeScript 接口、route/nearest 客户端与交互请求控制器。它不依赖首页或地图组件，宿主注入 HTTP/IPC 传输。构建、测试和下游安装方式见包内文档。
