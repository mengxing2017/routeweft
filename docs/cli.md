# CLI schema 1

`routeweft --help` 显示命令。除帮助外，所有成功和错误均为单行 JSON，stdout 只含协议数据；日志预留 stderr。`--json` 是显式标记，省略后也输出 JSON。重复/未知选项、位置参数和缺失值拒绝。

| 命令 | 必须参数 | 行为 |
| --- | --- | --- |
| build | --input、--profile driving、--output | 从本地 PBF 构建，不覆盖已有目录；--format 1（默认）或 2 |
| preflight | --input、--profile driving | 默认 full 内存建图；可显式 --mode structure 低内存结构/容量扫描 |
| version | 无 | 返回工具版本和能力，不加载数据集 |
| inspect | --dataset | 默认完整校验；--mode metadata 仅读取 manifest，verified=false |
| verify | --dataset | 完整校验后返回 verified=true 与 manifest |
| nearest | --dataset、--coordinate lon,lat | 返回有限方向候选及 dataset/profile/format |
| route | --dataset、--coordinates lon,lat;lon,lat | 返回几何、米/秒成本、分段、步骤、吸附位置及身份 |

可选二进制存储：

```bash
routeweft build --input city.osm.pbf --profile driving --format 2 --output city-binary/
routeweft verify --dataset city-binary/ --json
```

`--format` 仅用于 build；verify/inspect/nearest/route 根据 manifest 自动选择读取器。v1 是默认格式，v2 使用 graph.bin 和名称/类别字典，详见 [v2 格式](dataset-format-v2.md)。`version.dataset_formats` 和 `capabilities.dataset_write_formats` 声明 `[1,2]`，默认使用内存构建，v2 另可显式选择磁盘流水线构建；加载仍持有完整图。两种格式均不扩大输入或图容量上限。

### 显式磁盘构建

```bash
routeweft build --input city.osm.pbf --profile driving --output city-external/ \
  --builder external --format 2 --scratch-dir ./scratch \
  --max-scratch-bytes 1073741824 --max-scratch-files 64 --json
```

`--builder memory` 是默认值，支持格式 1/2。`--builder external` 必须同时指定 `--format 2`，采用磁盘暂存、构图及完整校验后发布；两者使用相同的 driving-v1 和 reject/block_ways 策略。默认严格拒绝，外存模式不会自动排除无效限制。

`--scratch-dir` 指定工作目录的父目录，默认使用输出父目录；它须在输出目录之外，并与输出位于同一文件系统，以便最终 rename。工具只清理自己创建的私有工作目录，不删除 scratch 父目录或其他文件。输出须不存在，包括空目录和链接。源 PBF 在构建期间须保持不变，提交前会再次核对摘要。

`--max-scratch-bytes` 默认 1 GiB，`--max-scratch-files` 默认 64，覆盖本次构建拥有的暂存、保留的中间结果及最终两个文件的同时占用，包含完成元数据的预留；不计原始输入、其他进程文件或文件系统块开销。成功结果新增 `build.builder=external` 与 `build.resources`：`peak_accounted_scratch_bytes` 是保守计费峰值，`peak_completed_files` 仅为阶段完成检查点观测值；活动文件数也受预算约束，但后者不是其实际峰值。二者都不是 RSS。

可选 `--build-timeout-ms` 设置协作式截止时间，0 表示立即截止；只在阶段边界和最终提交前检查，不中断正在运行的阶段，因此不能作为硬超时。上述 scratch/构建时限参数只用于 external，内存模式使用它们会被拒绝。强制终止或断电可能留下私有工作目录或目标预占空目录；当前无自动恢复。最终 rename 后的目录 sync 错误可能返回失败但留下完整目标，重试前应检查已有目标。

external 仍执行既有 PBF/解压/对象/图上限。字典和限制聚合有内存预算，加载与查询仍驻内存；该模式不构成省域或 3 GiB 输入支持声明。

查询选项：`--radius-m` 默认 1000，`--max-work` 默认 2,000,000，`--max-states` 默认 250,000，`--max-queue` 默认 500,000，`--timeout-ms` 默认 10,000。deadline 从数据集加载完成后开始，限制实际查询计算，不包括进程启动/加载；宿主如需请求总时限，应在库外统筹。

成功包络：`{"schema":1,"status":"ok",...}`。错误包络：`{"schema":1,"status":"error","error":{"kind":"NoRoute","message":"NoRoute"}}`。

| 退出码 | 类别 |
| ---: | --- |
| 0 | 成功 |
| 2 | InvalidInput / UnsupportedProfile |
| 3 | NoSegment / NoRoute |
| 4 | Io / InvalidDataset / IncompatibleFormat / 构建及成本错误 |
| 5 | 查询预算/取消，或 external 构建阶段截止/协作取消 |

构建/加载容量上限归为数据处理错误退出 4，错误 kind 仍区分 BudgetExceeded；查询预算耗尽及 external 的 BuildTimeout/BuildCancelled 退出 5。当前 CLI 没有取消信号到 AtomicBool 的桥接，协作取消由库宿主提供；操作系统终止进程仍遵循系统行为。

Route schema：`dataset_id/profile/format/distance_m/duration_s/geometry/legs/steps/snapped`；Point 为 `{lon,lat}`。Leg 的 geometry_start/end 是包含两端的几何索引。Step 附 maneuver/name/米/秒/几何索引/可空 exit。所有 waypoint 成本累加和全程一致；无路不返回虚构的直线几何。

## 输入预检与工具能力

```bash
routeweft version --json
routeweft preflight --input city.osm.pbf --profile driving --json
```

`version` 返回 version、cli_schema、dataset_formats、profiles、commands、invalid_restriction_policies 和 capabilities。capabilities 声明固定容量、默认内存构建/驻内存查询、预检模式及缓存能力；build_modes 为 [memory,external]，external_build_formats 为 [2]。external_memory_build=false、large_pbf_3gib=false 继续保留，表示尚未完成外存规模能力验收，不能据新增构建模式或文件大小扩大支持范围。version 是 Cargo 包版本，不是唯一源码身份；集成仍需固定 Git 提交、Cargo.lock、构建回执和二进制 SHA-256。CLI schema=1 是包络版本，dataset format=1/2 是磁盘格式，driving-v1 是规则版本，builder 是构建器版本字符串，四者不可互相替代。

默认 `preflight --mode full` 使用与 build 相同的解码、规则、建图和限制解析路径，在内存构建并校验后丢弃图，不创建数据集。它不是廉价文件头检查，也不保证磁盘空间、输出路径、序列化文件大小或实际查询性能。成功返回 report；失败退出 4，不在 stdout 混入日志。

### 低内存结构扫描与元信息读取

```bash
routeweft preflight --input city.osm.pbf --profile driving --mode structure --json
routeweft inspect --dataset dataset/ --mode metadata --json
```

structure 使用相同的单块解码器，但不保留全局 node/way/relation 集合，不建图、不检查重复 ID、引用闭包或驾驶/限制规则。成功报告明确 `validation_scope=structure_only`、`semantic_validation=false`、`duplicate_ids_checked=false`，包含源哈希、对象数、累计解压字节及完整解码预计保留的对象字节数。后者不是扫描进程的 RSS。它仍执行当前构建器的容量门槛；超限提前失败时不代表已经扫描完全部输入。该模式拒绝 `--invalid-restrictions`，不能把结构通过当作完整预检通过。

metadata 仅检查 manifest 结构、声明容量及内容身份，不打开图文件（graph.json 或 graph.bin），返回 `verified=false`、`verification_scope=manifest_only`。默认 inspect 与 verify 仍加载图并校验完整性和拓扑。元信息读取不能代替交付校验，也不认证来源。

build 本身执行完整语义检查并返回审计 report；若不需要提前排查，可直接构建并验证报告，避免先完整 preflight 再重复建图。当前没有阶段缓存或恢复功能。输入容量见下节，数据集限制见 [数据集格式](dataset-format.md)。

### 输入容量与资源边界

默认构建器使用全内存对象与图；显式 external 使用磁盘流水线。两种模式的输入均须同时满足以下限制，压缩文件较小也可能因对象或解码规模超限而被拒绝：

| 资源 | 上限 | 含义 |
| --- | ---: | --- |
| input_bytes | 512 MiB | 压缩 PBF 大小 |
| blob_bytes / raw_block_bytes | 各 32 MiB | 单块 blob 大小 / 声明解压大小 |
| decoded_bytes | 512 MiB | 累计解压处理量 |
| estimated_osm_bytes | 512 MiB | 完整 OSM 对象保留估算 |
| osm_objects | 2,000,000 | node、way、relation 合计 |

上述限制是独立容量门槛，不是进程 RSS 保证；对象估算不包含全部临时缓冲、图和查询索引。数据集还受 [图文件和对象数量限制](dataset-format.md) 约束。CLI 不提供调高上述输入限制的参数；external 提供 scratch 预算，但不提供阶段缓存、恢复或 RSS 硬预算参数。宿主应根据实际资源设置进程级资源限制。

### 容量错误

主要输入、解码、对象和图容量错误在原有 kind/退出码之外返回 `error.resource`：

```json
{"resource":"osm_objects","limit":2000000,"observed":2000001,"stage":"pbf_decode"}
```

按 resource/stage 分型，不解析 message。主要阶段为 input、pbf_read、pbf_decode、graph_build、graph_validation、dataset_manifest、dataset_read、dataset_write；external 还会返回 external_build、external_join、external_edges、external_adjacency、external_restrictions、external_blockways、v2_validation 等阶段。自动扣除保留文件后，子阶段 resource.limit 可能小于 CLI 总预算。estimated_osm_bytes 是对象估算，decoded_bytes 是累计处理工作量，均不是进程内存硬限制。resource 为可选字段；部分容量或解析错误仅返回 kind/message，调用方应处理字段缺失。

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

external 的流水线、源文件复核及发布错误也保留 `error.details`；通用 failure_code 为 ExternalBuildError，最终数据集写入为 DatasetPublicationError，输入变化为 SourceChanged，阶段截止/取消为 BuildTimeout/BuildCancelled。静态限制仍使用 InvalidRestriction 及关系级诊断。早期失败可能尚无来源摘要；各阶段资源错误按 error.resource 分型。源关系按 ID 排序处理，未排序 PBF 的诊断列表次序可能与内存模式不同，关系身份与处理策略保持。
