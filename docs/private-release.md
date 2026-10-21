# 公司私有 Cargo 发布

需要 Python 3.11+、Git 和 Cargo。公司制品服务须支持 Cargo registry 协议。

## 配置

只需配置仓库索引地址和发布 token：

```bash
cp .private-release.env.example .private-release.env
chmod 600 .private-release.env
# 编辑 .private-release.env，填写下面两个变量。
set -a
source .private-release.env
set +a
```

| 变量 | 内容 |
| --- | --- |
| `CARGO_REGISTRIES_INTERNAL_INDEX` | 公司 Cargo 索引的 HTTPS 或 sparse+HTTPS 地址 |
| `CARGO_REGISTRIES_INTERNAL_TOKEN` | 公司 Cargo 服务的发布 token |

真实配置文件已被 Git 忽略。不要填写 npm hosted 地址，也不要把真实地址或 token 提交到源码。Cargo 使用本机现有缓存，无需另配 `CARGO_HOME`。

## 发布

在仓库根目录执行：

```bash
make release-check       # 检查受 Git 跟踪的文件是否含目标仓库域名
make release-dry-run     # 预演，不上传
make release-publish     # 正式发布四个库
```

也可以使用 `devbox run release:check`、`devbox run release:dry-run`、`devbox run release:publish`。

脚本自动从当前 `HEAD` 创建临时工作区，补齐版本依赖，按 `domain → dataset → routing → preprocess` 顺序处理，完成或失败后清理临时文件。源码中的 `publish=false` 保持不变，CLI 不发布。发布前请先提交需要发布的源码；未提交内容不包含在发布包内。

正式发布对每个包先预演、检查压缩包，再上传；Cargo 负责等待已发布包在索引中可见，任一步失败立即停止。首次向空 registry 发布时，后续包的独立预演可能因前序版本尚未发布而失败，可直接使用正式发布入口逐包完成检查和上传。

如果只需发布某个库（例如前序包已成功发布）：

```bash
python3 scripts/private-release.py release --package routeweft-routing --execute
```

默认发布 `HEAD`，也可通过命令参数 `--ref <tag或commit>` 选择版本，不需要额外环境变量。

## 地址隔离

发布目标固定使用通用别名 `internal`，实际地址从环境读取。检测规则自动取目标索引的域名，不需要维护检测清单。Cargo 输出不直接打印，避免错误信息携带内部地址；失败返回非零退出码。该检查只覆盖已知目标域名，不等于检查了所有内部地址。

私有发布包中的 Cargo 生成 manifest/锁文件可能包含索引地址，因此这些包保留在公司仓库，不作为公开附件分发。临时包包含已有第三方许可材料；脚本不选择项目许可证，也不要求每次发布额外提供许可证文件。当前原创部分的许可证状态见项目 README。

## 验证

```bash
python3 -m unittest discover -s scripts/tests -v
```
