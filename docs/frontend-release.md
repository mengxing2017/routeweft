# 前端接口包版本与远程发布

`frontend/packages` 中的库通过 npm registry 供外部工程安装，版本独立于 Cargo；React/Vite 首页 `@routeweft/frontend` 是私有应用，不发布。

| 包 | 当前版本 | 外部用途 |
| --- | --- | --- |
| `@routeweft/routing-client` | `0.1.0-pre` | TypeScript 类型、route/nearest 客户端、请求控制器、planner |
| `@routeweft/maplibre` | `0.1.0-pre` | MapLibre 5 路线图层、途经点和点选适配；依赖 client `0.1.0-pre` |

版本值来自各包 `package.json`，不是远程已发布状态。包为 ESM，包含 `dist/index.js`、`dist/index.d.ts`、README 和来源/许可声明。原创代码许可维持现有声明，发布配置不变更许可。下游接入见[使用指南](frontend-usage.md)。

## 环境与仓库配置

需要 Python 3.11+、Node.js 22、npm 和 pnpm 11.22.0；可使用项目 Devbox。需要目标仓库的 `@routeweft` scope 发布权限。私有仓库可以是提供 npm 协议的 Nexus、Artifactory、Verdaccio 等服务；地址必须是 HTTPS **npm registry endpoint**，不是 Git 地址或 Cargo 索引。

在仓库根目录 `.private-release.env` 添加两项（无需 `source`，脚本自动读取）：

```dotenv
NPM_REGISTRY='https://packages.example.com/repository/npm-hosted/'
NPM_TOKEN='填入目标仓库发布token'
```

配置文件已被 Git 忽略。也可用 CI secret/导出环境变量，环境值优先；兼容旧变量 `NPM_REGISTRY_INTERNAL`、`NPM_TOKEN_INTERNAL`。新变量优先于旧变量；显式设置空的 `NPM_TOKEN` 会禁用旧 token 回退。显式 `--registry` 优先于环境地址。未指定目标会失败，不会默认上传到 npmjs。dry-run 不强制 token，但私有仓库下载依赖可能仍需读权限。

脚本将 token 绑定到目标 registry 的 host/path，临时配置权限为 0600，不改用户 `.npmrc`。目标须能提供构建依赖，或允许访问锁文件引用的公共依赖。若仓库只负责 hosted 上传、不代理 npmjs 依赖，应先在可访问构建依赖的环境配置镜像/锁文件；不能通过修改已打包产物绕过校验。

## 私有发布

在仓库根目录运行：

```bash
make release-test
make release-frontend-dry-run
# 检查两包名称、版本和 dist-tag，正式上传：
make release-frontend-publish
```

Devbox 对应 `devbox run release:frontend:dry-run` / `devbox run release:frontend:publish`。直接脚本入口：

```bash
python3 scripts/frontend-release.py --registry https://packages.example.com/repository/npm-hosted/
python3 scripts/frontend-release.py --registry https://packages.example.com/repository/npm-hosted/ --execute
```

默认 `--access restricted`，正式上传需要 token。服务端如何映射 visibility/access 由目标 npm registry 决定。

## npmjs 公共发布

将 `NPM_REGISTRY` 设为 `https://registry.npmjs.org/`，`NPM_TOKEN` 改为有目标 scope 权限的 npmjs token，并显式指定 public：

```bash
make release-frontend-dry-run FRONTEND_RELEASE_ARGS="--access public"
make release-frontend-publish FRONTEND_RELEASE_ARGS="--access public"
# 或直接指定公共目标：
python3 scripts/frontend-release.py --registry https://registry.npmjs.org/ --access public
python3 scripts/frontend-release.py --registry https://registry.npmjs.org/ --access public --execute
```

公共和私有目标分开运行，分别使用各自凭据。同一包版本可分别上传到不同 registry；不能假定一个仓库已上传就代表另一个也存在。公共 scoped 包使用 `--access public`；实际权限及账号认证条件由服务端校验。[npm publish 官方说明](https://docs.npmjs.com/cli/v11/commands/npm-publish/)

## 版本管理

采用 SemVer。修复升级 patch；兼容新增能力升级 minor；不兼容 API 修改应升级 major，0.x 阶段以 minor 明确表达破坏性变化，并写迁移说明。CLI JSON schema、Rust crate 版本、数据集格式与 npm 版本是不同契约。

在仓库根目录更新，例如 client `0.1.0-pre → 0.1.0-pre.1`：

```bash
npm --prefix frontend/packages/routing-client version 0.1.0-pre.1 --no-git-tag-version
```

随后编辑 `frontend/packages/maplibre/package.json`，把 `dependencies.@routeweft/routing-client` 同步为精确 `0.1.0-pre.1`。由于 adapter 的依赖发生变化，已发布过的 adapter 也须升级自身版本，例如：

```bash
npm --prefix frontend/packages/maplibre version 0.1.0-pre.1 --no-git-tag-version
pnpm --dir frontend install --lockfile-only
make check-frontend
make release-frontend-dry-run
```

提交两个 manifest、`frontend/pnpm-lock.yaml`、client 的 `package-lock.json` 和版本相关文档。当前两包统一为 `0.1.0-pre`，默认发布到 `next`，下游须显式指定版本。首次发布当前版本时无需人为递增；远端已有版本才需要新版本。发布命令不会自动改版本、提交或创建 Git tag。建议完成审查后记录如 `routing-client-v0.1.0-pre.1`、`maplibre-v0.1.0-pre.1` 的来源标签。

预发布示例：`0.1.0-pre.1`。保持 adapter 依赖与选定 client 精确版本一致；默认稳定版 tag 为 `latest`，预发布为 `next`，也可 `--tag beta`。脚本禁止将预发布显式打到 latest：

```bash
make release-frontend-dry-run FRONTEND_RELEASE_ARGS="--tag next"
make release-frontend-publish FRONTEND_RELEASE_ARGS="--tag next"
```

下游验证时安装精确版本，不依赖浮动 `latest`/`next`。

## 发布流程与失败续发

前端发布使用**当前工作区**，包含未提交修改；这与 Cargo 的提交快照发布不同。请固定审查后的内容，发布期间不要并发修改源码。

每次运行：

1. 检查两包 SemVer、包名和 adapter 精确依赖版本。
2. 将 frontend 复制到临时目录，去掉旧 dist、node_modules、本地配置和旧 tgz；用冻结的 pnpm workspace 锁安装依赖。
3. 依次构建并测试 client、adapter。
4. 打包选定库，检查 JS、类型、README、声明、实际包版本，以及是否残留 `workspace:`/`file:`/`link:` 依赖。
5. **所有选定包**通过 `npm publish --dry-run` 后，带 `--execute` 才按 client → adapter 上传同一份 tgz。

任一步失败停止；临时文件自动清理，日志中的 token 和 URL 脱敏。预演不上传，但不证明服务端允许最终写入。依赖安装可能需要网络，打包测试不使用旧 dist。

若 client 上传成功而 adapter 失败，已上传版本不会回滚；等待前置版本能被目标仓库安装后，使用相同源码只续发 adapter：

```bash
make release-frontend-dry-run FRONTEND_RELEASE_ARGS="--package maplibre"
make release-frontend-publish FRONTEND_RELEASE_ARGS="--package maplibre"
```

单包仍测试两包，但只预演/上传所选包。单独发布 adapter 要求其 client 依赖已存在于同一 registry。版本已存在、权限不足、网络/依赖索引延迟等失败需按具体日志处理，不能对已发布版本重复上传。服务端副作用无法原子回滚；全组预演也不保证后续上传全成功。

## 外部消费验收

在一个没有本仓库 workspace、override 或相对路径依赖的新目录中验证。私有地址通过下游 `.npmrc` 的 scope 映射配置（见[使用指南](frontend-usage.md)）：

```bash
npm view @routeweft/routing-client@0.1.0-pre version
npm view @routeweft/maplibre@0.1.0-pre dependencies
npm install --save-exact @routeweft/routing-client@0.1.0-pre @routeweft/maplibre@0.1.0-pre maplibre-gl@5.24.0
```

核对安装版本、类型检查、ESM 导入和宿主 transport smoke。MapLibre 在浏览器中验收图层与销毁；不要用 Node SSR 导入成功代替地图行为验收。第一次正式发布还必须完成远程无本地链接安装验证，仓库内测试不能替代它。
