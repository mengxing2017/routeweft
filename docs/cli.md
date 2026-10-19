# CLI schema 1

`routeweft --help` 显示命令。除帮助外，所有成功和错误均为单行 JSON，stdout 只含协议数据；日志预留 stderr。`--json` 是显式标记，省略后也输出 JSON。重复/未知选项、位置参数和缺失值拒绝。

| 命令 | 必须参数 | 行为 |
| --- | --- | --- |
| build | --input、--profile driving、--output | 从本地 PBF 构建，不覆盖已有目录 |
| preflight | --input、--profile driving | 内存建图校验，不写数据集 |
| version | 无 | 返回工具版本和能力，不加载数据集 |
| inspect | --dataset | 完整校验后返回 manifest，非不校验的快速读取 |
| verify | --dataset | 完整校验后返回 verified=true 与 manifest |
| nearest | --dataset、--coordinate lon,lat | 返回有限方向候选及 dataset/profile/format |
| route | --dataset、--coordinates lon,lat;lon,lat | 返回几何、米/秒成本、分段、步骤、吸附位置及身份 |

查询选项：`--radius-m` 默认 1000，`--max-work` 默认 2,000,000，`--max-states` 默认 250,000，`--max-queue` 默认 500,000，`--timeout-ms` 默认 10,000。deadline 从数据集加载完成后开始，限制实际查询计算，不包括进程启动/加载；宿主如需请求总时限，应在库外统筹。

成功包络：`{"schema":1,"status":"ok",...}`。错误包络：`{"schema":1,"status":"error","error":{"kind":"NoRoute","message":"NoRoute"}}`。

| 退出码 | 类别 |
| ---: | --- |
| 0 | 成功 |
| 2 | InvalidInput / UnsupportedProfile |
| 3 | NoSegment / NoRoute |
| 4 | Io / InvalidDataset / IncompatibleFormat / 构建及成本错误 |
| 5 | 查询 BudgetExceeded / Cancelled |

构建/加载上限归为数据处理错误退出 4，错误 kind 仍区分 BudgetExceeded；查询预算耗尽退出 5。当前 CLI 没有取消信号到 AtomicBool 的桥接，协作取消由库宿主提供；操作系统终止进程仍遵循系统行为。

Route schema：`dataset_id/profile/format/distance_m/duration_s/geometry/legs/steps/snapped`；Point 为 `{lon,lat}`。Leg 的 geometry_start/end 是包含两端的几何索引。Step 附 maneuver/name/米/秒/几何索引/可空 exit。所有 waypoint 成本累加和全程一致；无路不返回虚构的直线几何。

## 输入预检与工具能力

```bash
routeweft version --json
routeweft preflight --input city.osm.pbf --profile driving --json
```

`version` 返回 version、cli_schema、dataset_formats、profiles、commands 和 invalid_restriction_policies。version 是 Cargo 包版本，不是唯一源码身份；集成仍需固定 Git 提交、Cargo.lock、构建回执和二进制 SHA-256。CLI schema=1 是包络版本，dataset format=1 是磁盘格式，driving-v1 是规则版本，builder 是构建器版本字符串，四者不可互相替代。

`preflight` 使用与 build 相同的解码、规则、建图和限制解析路径，在内存构建并校验后丢弃图，不创建数据集。它不是廉价文件头检查，也不保证磁盘空间、输出路径、序列化文件大小或实际查询性能。成功返回 report；失败退出 4，不在 stdout 混入日志。

## 限制关系诊断与显式保守模式

build/preflight 接受 `--invalid-restrictions reject|block_ways`，默认 reject，不提供 ignore 选项。

缺少 via 的相关机动车限制保持严格失败，示例（省略其他 report 字段）：

```json
{"schema":1,"status":"error","error":{"kind":"InvalidDataset","message":"relation 6843783: MissingVia: restriction has no via member; turn location is not inferred","details":{"valid":false,"policy":"reject","failure_code":"InvalidRestriction","restriction_diagnostics":[{"relation_id":6843783,"code":"MissingVia","action":"rejected","tags":{"restriction":"no_right_turn","type":"restriction"},"members":[{"kind":"way","id":389194568,"role":"from"},{"kind":"way","id":389190340,"role":"to"}],"members_truncated":false}]}}}
```

使用 `error.details.restriction_diagnostics[].code` 分型，不解析人类可读 message。稳定分类包括 MissingVia、MissingFromTo、MissingReference、InvalidMemberType、UnsupportedMultiplicity、UnsupportedViaShape、UnsupportedRestriction、UnresolvedRestriction、RestrictionLimitExceeded；含义见 [profile](driving-profile.md)。PBF 解码错误使用 details.failure_code=PbfReadError，其他图校验错误为 GraphValidationError；读取前的文件错误为 InputReadError，数据集写入失败为 DatasetWriteError；底层 kind/退出码仍保留原契约。早于读取的 IO/限额失败可能没有来源身份或完整诊断。

诊断最多保留 256 条，否则预算失败；每条最多 256 个成员，超出时 members_truncated=true。诊断标签最多 32 项，键/值最多 128/512 字符，role 最多 64 字符；完整源信息仍需检查原始 PBF。报告是有界诊断，不是完整 OSM 原件的副本。

只有经过审核后才显式启用：

```bash
routeweft preflight --input city.osm.pbf --profile driving --invalid-restrictions block_ways --json
routeweft build --input city.osm.pbf --profile driving --invalid-restrictions block_ways --output dataset/ --json > build-result.json
```

block_ways 排除无效关系中已存在的全部 way 成员，并迭代复核其他限制；无法安全阻断、图为空、预算或普通解析失败仍拒绝。它不会推断 via、修补源数据、删除限制后放行，也不宣称源输入完全有效。可能明显降低道路覆盖与可达性。

成功 build 新增 report 字段，包含 policy、source_sha256/source_snapshot、最终节点/边/禁止序列数量、计数、逐关系审计和 blocked_way_ids。preflight/build 的 report.valid=true 表示在所选策略下图校验通过，允许同时存在 action=blocked_ways 的告警。失败报告中的节点/边数可能仅反映失败尝试，不是有效成品。

完整审计位于 CLI report，不新增 dataset v1 必需字段。请保存 build-result.json 并随构建回执分发；manifest.diagnostics 只保存计数，不包含关系级完整审计。库调用者使用 build_with_options/prepare/preflight 及结构化 Error.report；默认 build/graph 仍严格拒绝。
