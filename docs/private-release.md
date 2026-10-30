# 私有 Cargo 发布

Cargo 公共和私有发布已统一到 [发布指南](release.md)，命令与 huavd-cairnkit 对齐，必须显式指定目标：

```bash
make release-check REGISTRIES=internal
make release-dry-run REGISTRIES=internal
make release-publish REGISTRIES=internal
```

私有配置仍为 `.private-release.env` 中的 `CARGO_REGISTRIES_INTERNAL_INDEX` 和 `CARGO_REGISTRIES_INTERNAL_TOKEN`，脚本自动加载。旧 `scripts/private-release.py` 入口转发至统一脚本，也需要 `--registry internal`。

## 前端 npm 包发布

两个独立版本的接口包 `@routeweft/routing-client` 和 `@routeweft/maplibre` 使用 npm registry。
在 `.private-release.env` 添加 `NPM_REGISTRY`（HTTPS npm endpoint）和 `NPM_TOKEN`，脚本自动加载，无需 source；兼容旧的 `NPM_REGISTRY_INTERNAL` / `NPM_TOKEN_INTERNAL`。

```bash
make release-frontend-dry-run
make release-frontend-publish
```

Devbox 对应 `devbox run release:frontend:dry-run` / `devbox run release:frontend:publish`。
发布当前工作区，在临时目录安装锁定依赖、构建测试两包、校验 tarball，全部预演通过才按依赖顺序上传。静态首页不发布。
详细配置、公共仓库、版本升级及失败续发见[前端发布指南](frontend-release.md)，下游安装和接入见[使用指南](frontend-usage.md)。
