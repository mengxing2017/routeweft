# Cargo 公共与私有仓库发布

源码保持 `publish = false`，不记录私有 registry 名称、地址或凭据。发布脚本从指定 Git 提交创建临时副本，仅在副本中启用选定的 registry，并将包间依赖改为目标仓库的精确版本。私有地址通过环境变量注入。当前项目尚未声明原创代码许可证；发布脚本不选择或变更项目许可证。临时包会包含已有的 LICENSES、THIRD_PARTY_NOTICES.md 及数据来源材料。

需要 Python 3.11+、Git、Make 和项目锁定的 Rust/Cargo 1.97.1 工具链（支持 workspace 发布预演）。执行前先提交需要发布的代码；默认发布 HEAD，未提交和未跟踪文件不会进入快照。脚本不会自动提交代码。

## 配置与入口

```sh
cp .private-release.env.example .private-release.env
chmod 600 .private-release.env
# 编辑文件：公共发布填写 CARGO_REGISTRY_TOKEN；私有发布填写对应 INDEX/TOKEN
# 脚本会自动读取仓库根目录的 .private-release.env，无需手动 source

make release-test
# 公共仓库示例；私有仓库请将 crates-io 换成已配置的别名
make release-check REGISTRIES=crates-io
make release-dry-run REGISTRIES=crates-io
# 明确准备上传时执行：
make release-publish REGISTRIES=crates-io
```

也可使用 `devbox run release:test`、`devbox run release:check REGISTRIES=crates-io`、`devbox run release:dry-run REGISTRIES=crates-io`、`devbox run release:publish REGISTRIES=crates-io`。Python/Make 需在环境中可用。

Make、Devbox、Python 及旧兼容入口均通过同一脚本自动加载仓库根目录的 `.private-release.env`，与运行时当前目录无关。文件不存在时使用环境变量；已导出的环境变量优先于文件（包括空值）。支持 `KEY=value`、可选 `export`、单/双引号、空行和 `#` 注释；值按字面读取，不执行 shell 命令，也不展开 `$VAR` 或 `$(...)`。文件接受 Cargo registry 配置变量及现有前端 npm 配置变量；格式错误会报告行号，不输出配置值。

配置变量：

- `CARGO_REGISTRIES_INTERNAL_INDEX`：HTTPS 或 sparse+HTTPS Cargo 索引地址。
- `CARGO_REGISTRIES_INTERNAL_TOKEN`：该服务的发布 token。

使用标准 Cargo 名称区分目标：`crates-io` 为公共仓库，`internal` 为示例私有仓库别名；也可以指定其他私有 registry 别名。

- 公共仓库：设置 `CARGO_REGISTRY_TOKEN`，不需要配置索引地址。包间依赖使用 crates.io 默认来源，不添加私有 `registry` 字段。
- 私有仓库：设置 `CARGO_REGISTRIES_INTERNAL_INDEX` 和 `CARGO_REGISTRIES_INTERNAL_TOKEN`。其他私有别名对应 `CARGO_REGISTRIES_<大写别名>_INDEX/TOKEN`，别名仅含小写字母、数字、下划线，以字母开头。

```sh
# 公共仓库 crates.io
make release-dry-run REGISTRIES=crates-io
make release-publish REGISTRIES=crates-io
# 私有仓库
make release-dry-run REGISTRIES=internal
make release-publish REGISTRIES=internal
# 同一版本依次发布到公共和私有仓库
make release-check REGISTRIES="crates-io internal"
make release-dry-run REGISTRIES="crates-io internal"
make release-publish REGISTRIES="crates-io internal"
# Python 入口，也可以结合 --ref 和 --package
python3 scripts/release.py release --registry crates-io --registry internal
python3 scripts/release.py release --registry crates-io --ref HEAD --package routeweft-domain --execute
```

`release` 会在任何上传前检查所有目标的索引地址和 token；重复的目标只执行一次。所有目标固定使用同一个来源 commit，每个目标使用独立临时副本，发布权限、包间依赖、归档检查和 Cargo 上传均使用该目标的 registry。每个仓库内仍按依赖顺序发布；任意步骤失败立即停止，包括后续仓库。已上传的版本不会回滚，重试时请指定尚未完成的仓库及包，避免重复上传。所有发布和检查入口都必须显式指定目标；没有默认仓库。未指定 `REGISTRIES` / `--registry` 时立即提示用法，不会读取私有仓库配置或启动 Cargo。

`check`、`prepare`、`publish` 支持单个 `--registry`。保留的副本只能使用其 `release-source.json` 记录的同名 registry 发布；发布到另一仓库需要另建副本。`.private-release.env` 已忽略，继续作为本地凭据文件使用。旧入口 `scripts/private-release.py` 保留兼容，新的统一入口为 `scripts/release.py`。不要直接在源码目录使用 `cargo publish`，它仍受 `publish=false` 保护。

## 顺序、重试和快照

顺序为 domain → dataset → routing → preprocess；routeweft-daemon CLI 不发布。脚本只修改临时副本中的 backend Cargo workspace；manifest 布局变化时停止，要求重新审查。

```sh
python3 scripts/release.py release --registry crates-io --ref <tag或commit>
python3 scripts/release.py release --registry crates-io --ref <tag或commit> --package routeweft-routing --execute
```

不带 `--execute` 只预演。未指定 `--package` 时，一次执行 `cargo publish --workspace --dry-run`，由 Cargo 将包组内尚未发布的依赖放入临时 registry 进行打包和构建验证；随后检查四个库的归档。此流程不上传、不要求提前发布 domain，也不使用 `--no-verify`。正式发布逐包预演、扫描生成的 `.crate` 后上传，失败立即停止，临时目录自动清理。已成功上传的版本不会回滚或删除；修复后按同一提交选包继续，已存在的版本不要重复上传。

整组预演可用于首次发布，但仍需要连接已配置的 registry；它验证实际包内容和包组内依赖，不证明远端已有这些版本，也不证明上传权限。指定 `--package` 的单包预演仍要求其前置版本已在远端发布。首次正式发布按依赖顺序逐包完成，前置版本在远端可获取后才继续。发布后的无 override 独立消费验证仍是单独门槛。

公共仓库（`crates-io`）正式发布时，脚本在成功上传一个包后、处理下一个包前默认等待 60 秒，首包和末包之后不等待。固定间隔只能降低限流概率，不能保证避免限流。私有仓库与 dry-run 不增加等待。

公共上传返回 HTTP 429 时，自动重试当前包，默认最多额外重试 3 次；不会重新上传前面已成功的包。脚本优先读取 Cargo 日志中的 `Please try again after ... GMT` 或 `Retry-After`（HTTP 日期或秒数），等到服务端时间后再留 1 秒余量。无法解析重试时间时，依次退避 60、120、240 秒。重试耗尽或遇到其他错误立即停止；私有仓库的 429 也立即停止。dry-run 失败不会触发上传重试。等待期间每 30 秒显示剩余时间，Ctrl+C 可取消并清理临时副本。

```sh
# 可选：调整公共包间隔和 429 重试上限
make release-publish REGISTRIES=crates-io PUBLISH_INTERVAL=120 MAX_RATE_LIMIT_RETRIES=5
python3 scripts/release.py release --registry crates-io --execute --publish-interval 120 --max-rate-limit-retries 5
# 单包续发或保留副本发布同样支持 --max-rate-limit-retries
python3 scripts/release.py release --registry crates-io --ref <原提交> --package routeweft-routing --execute --max-rate-limit-retries 5
```

参数均为非负整数；间隔为 0 时关闭包间等待，重试次数为 0 时关闭自动重试。自动等待不跨进程保存状态；重启后仍需用相同提交及失败的包继续，避免重复上传已成功的版本。

可以保留一个审查副本：

```sh
python3 scripts/release.py prepare --registry internal --ref HEAD --output /tmp/routeweft-release-review
python3 scripts/release.py publish --registry internal --stage /tmp/routeweft-release-review --package routeweft-domain
```

输出目录必须不存在且位于源码仓库之外。`release-source.json` 记录来源 commit、版本、包列表和 registry 别名。审查副本应留在内部。

## 验证边界

`release-check` 扫描受 Git 跟踪的当前文件；私有发布匹配目标索引域名，公共发布匹配环境中已配置的私有索引域名。公共发布不会把 crates.io 的公共域名当作私有地址。也可通过 Python 入口的 `--patterns <文件>` 指定需要拦截的私有地址，尤其是没有私有索引环境变量时。准备阶段还会扫描实际提交快照。该检查只覆盖已知域名，不是完整来源或许可证审计。Cargo 生成的 manifest/锁文件可能包含内部索引地址，脚本仅对这两个文件中的精确索引值作扫描豁免；这些私有归档不能作为公开附件。

Cargo 日志实时输出，URL 和环境凭据在显示前脱敏；连续 15 秒没有新日志时显示等待状态和累计用时。按 Ctrl+C 会停止 Cargo 及其子进程，再清理临时目录。脚本兼容普通归档目录及 Cargo 的 tmp-crate/tmp-registry 临时布局，同名归档内容不一致时停止。失败时也可使用保留的临时副本在内部受控环境诊断。dry-run 不证明服务端允许最终上传，也不能替代发布后的无本地 override 消费验证。基础包发布后，仍需修改并验证私有 SDK 的 registry 依赖来源，然后独立验收消费者。
