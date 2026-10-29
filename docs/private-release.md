# 私有 Cargo 发布

Cargo 公共和私有发布已统一到 [发布指南](release.md)，命令与 huavd-cairnkit 对齐，必须显式指定目标：

```bash
make release-check REGISTRIES=internal
make release-dry-run REGISTRIES=internal
make release-publish REGISTRIES=internal
```

私有配置仍为 `.private-release.env` 中的 `CARGO_REGISTRIES_INTERNAL_INDEX` 和 `CARGO_REGISTRIES_INTERNAL_TOKEN`，脚本自动加载。旧 `scripts/private-release.py` 入口转发至统一脚本，也需要 `--registry internal`。

## 前端 npm 包发布

`@routeweft/routing-client` 使用独立的 npm registry，不使用 Cargo 索引。
仅在需要发布前端包时，在 `.private-release.env` 中额外添加 `NPM_REGISTRY_INTERNAL`（HTTPS npm 仓库地址，支持路径前缀）和
`NPM_TOKEN_INTERNAL`（发布 token），然后用 `set -a; source .private-release.env; set +a` 导入（前端脚本仍从环境读取配置）。
仓库需可读取锁文件中的构建依赖；当前 TypeScript 锁定下载地址为公共 npm registry。

```bash
make release-frontend-dry-run  # 安装锁定依赖、测试、打包、发布预演，不上传
make release-frontend-publish  # 同样先验证，再上传到指定 npm 仓库
```

Devbox 对应命令为 `devbox run release:frontend:dry-run` 和
`devbox run release:frontend:publish`。需要 Python 3、Node.js 和 npm。

前端发布使用**当前工作区**的 `frontend/packages/routing-client`，包含未提交修改；
这与 Cargo 发布从 HEAD 创建快照不同。建议先提交发布内容并修改该包的版本号与锁文件，
已有版本通常不能重复上传。静态首页 `@routeweft/frontend` 不发布。

脚本在临时目录重新构建、测试，并上传同一个已预演的 tarball；不使用已有 dist。
临时 npm 配置限制 token 到目标仓库路径，退出后清理配置、缓存和包。
不会改动用户 `.npmrc` 或将私有地址写入源码。npm 输出默认隐藏，避免泄漏私有配置；
预演仍需网络及凭据，但不证明服务端允许最终写入。命令本身不执行版本递增或 Git 提交。
