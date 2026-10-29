# CLI schema 1

`routeweft --help` 显示命令。除帮助和 `--version` / `-V` 文本别名外，所有成功和错误均为单行 JSON，stdout 只含协议数据；日志预留 stderr。`--json` 是显式标记，省略后也输出 JSON。重复/未知选项、位置参数和缺失值拒绝。

`version --json` 保留既有字段，并新增 `build`：`git_commit` 是完整源码提交号，`source_dirty` 是源码修改状态，无法确认时两者为 null；`source_identity_kind` 为 `git`、`git-archive`、`provided` 或 `unknown`。`target`、`profile`、`rustc` 记录编译环境。身份在编译时固定，不查询运行目录、Git 或网络。dirty 或 unknown 构建的 `release_eligible=false`，不得用于正式发布；该字段仅为来源状态提示，不构成可信签名。

Git checkout 自动采集整个仓库（含未忽略的未跟踪文件）的状态。无 `.git` 的源码归档默认返回 unknown，不会借用外层仓库身份。可信归档流水线可同时设置 `ROUTEWEFT_BUILD_COMMIT`（40 位十六进制提交号）、`ROUTEWEFT_BUILD_SOURCE_DIRTY`（true/false）、`ROUTEWEFT_BUILD_SOURCE_KIND`（git-archive/provided）后编译；缺少任一字段或格式错误则编译失败。这些声明由流水线负责核验，不会自动验证归档内容。每次 Cargo 构建都会重新采集身份，避免 HEAD、工作区或注入来源变化后复用旧元数据；不记录非确定构建时间。

安装器应独立校验来源锁、安装回执、实际二进制 SHA-256 与内嵌提交；旧 CLI 缺少 `build` 时须明确标记为外部来源验证。内嵌身份不能替代可信构建记录或文件摘要，缓存也不能仅以包版本为键。

| 命令 | 必须参数 | 行为 |
| --- | --- | --- |
| build | --input、--profile driving、--output | 从本地 PBF 构建，不覆盖已有目录；--format 1（默认）或 2 |
| preflight | --input、--profile driving | 默认 full 内存建图；可显式 --mode structure 低内存结构/容量扫描 |
| version | 无 | 返回工具版本和能力，不加载数据集 |
| inspect | --dataset | 默认完整校验；--mode metadata 仅读取 manifest，verified=false |
| verify | --dataset | 完整校验后返回 verified=true 与 manifest |
| nearest | --dataset、--coordinate lon,lat | 返回有限方向候选及 dataset/profile/format |
| route | --dataset、--coordinates lon,lat;lon,lat | 返回几何、米/秒成本、分段、步骤、吸附位置及身份 |
| customize | --dataset | 离线生成可选 routing.mld，加速 v1/v2 查询；支持 --graph-capacity、--max-load-bytes、--max-work、--timeout-ms |
| serve | --dataset | 常驻加载一次，stdin/stdout JSONL 串行查询；先输出 ready，EOF 退出 |

长距离场景应在 build 后执行 customize；默认 auto 自动使用有效的加速文件，`route --algorithm dijkstra` 保留基线。查询新增分阶段诊断和具体预算失败原因，kind 保持兼容。命令、升级与常驻协议详见[路由加速](routing-acceleration.md)。

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

`--progress off|json` 仅适用于 external，默认 `off`。`json` 在 stderr 输出 JSONL 事件（`event=build_progress`），stdout 仍只有最终 schema 1 结果或错误。字段包括 `stage`、`attempt`（暂存为 0）、`completed` 和 `elapsed_ms`；completed 的单位由阶段决定：staging_records 为已读对象数、way_closure 为已检查道路数、edges 为已连接节点数、adjacency/complete 为边数、restriction_scan_complete 为累计审计条数，其他阶段开始事件为 0。`diagnostics_observed` 是累计诊断计数，`scratch_peak_bytes` 按 `scratch_scope` 区分 staging、restrictions、blockways 私有阶段或 build_checkpoints 处观测的构建资源峰值，不是 RSS，也不是统一全局实时用量。`cancellation_checkpoint=true` 仅标记协调器确实完成取消/截止检查的边界事件；阶段事件不意味着新增取消检查点，超时仍是上述协作式边界检查。

外部构建报告的 `build_metrics` 独立于语义诊断和数据集 manifest，包含暂存/总耗时、每轮 connection/edges/adjacency/restrictions 耗时及新增排除数。`build_way_closure_*` 衡量最近一次完整原始 way 引用闭包检查：节点查找数、二分行探测数、缓存未命中后的文件行读取次数/字节及耗时（不等于块设备 I/O）。查找使用每表 4096 组、每组 4 行的固定缓存（每行 48 字节，含索引/替换元数据共 1 MiB/表），只读取实际探测行，不预读大页；relation access 仍按每次逻辑行探测计费，缓存命中不绕过预算。这些计数不能单独证明整次构建的性能瓶颈。

external 使用独立的累计解码工作量预算；单块、输入及图上限继续适用。字典和限制聚合有内存预算，加载与查询仍驻内存；该模式不构成省域或 3 GiB 输入支持声明。

### 显式大图容量档位

`--graph-capacity city|regional` 适用于 build、inspect、verify、nearest、route，默认 `city`。`regional` 构建必须使用 external/v2；full/structure 预检仍使用原内存容量。该档位扩展图处理额度，不扩大 PBF 输入上限或解码工作量预算，也不保证任意省域都能构建成功。

| 资源 | city | regional |
| --- | ---: | ---: |
| 图节点 | 1,000,000 | 8,000,000 |
| 有向边 | 2,000,000 | 16,000,000 |
| 图文件 | 256 MiB | 2 GiB |
| 单轴经纬跨度 | 5 度 | 10 度 |

图容量从外存关联、方向边、邻接、限制处理传递到最终 v2 写入及校验；各阶段核对档位身份。scratch 仍是独立的全链路额度，默认 1 GiB 不适用于所有大图。`version.capabilities.graph_capacity_profiles` 声明各档位。v2 字节格式保持不变，旧工具可能因自身容量限制拒绝大图。regional 产物的 manifest diagnostics 包含 `graph_capacity_regional=1`。

```bash
routeweft build --input region.osm.pbf --profile driving --builder external \
  --format 2 --graph-capacity regional --output region/ --scratch-dir scratch/ \
  --max-scratch-bytes 17179869184 --invalid-maxspeed use_default
routeweft verify --dataset region/ --graph-capacity regional
routeweft nearest --dataset region/ --graph-capacity regional \
  --max-load-bytes 8589934592 --coordinate 108.32,22.82
```

inspect full / verify 对 v2 使用流式完整语义及摘要校验，校验工作区额度 64 MiB，不加载完整 Graph。v1 保持原读取路径。

nearest / route 接受 `--max-load-bytes`，默认 2 GiB。加载前依据 manifest 估算 Graph、字符串/字典、CSR、R-tree 构建和限制自动机的分配量；不足时返回 `estimated_resident_bytes`、stage=`engine_load`，在打开图载荷和创建索引之前拒绝。库 `LoadOptions.max_resident_bytes` 同时限制图解码估算；regional Engine 使用 `Engine::open_with_capacity`。v1 没有可信的记录数头部，估算保守采用原 serde 数量上限，以防伪造较小 manifest 数量绕过预算。

该估算包含容量冗余和分配开销，**不是进程 RSS 或系统内存硬上限**；不含页缓存及独立查询状态，宿主仍应配置进程资源限制。查询 `--max-work/--max-states/--max-queue/--timeout-ms` 独立生效，增大图容量不会自动提高搜索预算。完整图及运行时索引仍驻内存，尚未提供按需图加载。

regional 吸附按查询点纬度做局部平面投影，空间索引继续采用共同坐标系及保守候选范围；距离仍采用球面计算。纬度范围仍为 [-85,85]，不支持跨日期变更线的数据集。city 沿用原投影，避免改变既有查询结果。

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

metadata 仅检查 manifest 结构、声明容量及内容身份，不打开图文件（graph.json 或 graph.bin），返回 `verified=false`、`verification_scope=manifest_only`。默认 inspect 与 verify 校验完整性和拓扑；v2 使用流式校验，v1 加载完整图。元信息读取不能代替交付校验，也不认证来源。

build 本身执行完整语义检查并返回审计 report；若不需要提前排查，可直接构建并验证报告，避免先完整 preflight 再重复建图。当前没有阶段缓存或恢复功能。输入容量见下节，数据集限制见 [数据集格式](dataset-format.md)。

### 输入容量与资源边界

默认构建器使用全内存对象与图；显式 external 使用磁盘流水线。内存构建与 full/structure 预检的输入均须同时满足以下限制，压缩文件较小也可能因对象或解码规模超限而被拒绝：

| 资源 | 上限 | 含义 |
| --- | ---: | --- |
| input_bytes | 512 MiB | 压缩 PBF 大小 |
| blob_bytes / raw_block_bytes | 各 32 MiB | 单块 blob 大小 / 声明解压大小 |
| decoded_bytes | 512 MiB | 累计解压处理量 |
| estimated_osm_bytes | 512 MiB | 完整 OSM 对象保留估算 |
| osm_objects | 2,000,000 | node、way、relation 合计 |

external 解码仍限制输入 512 MiB、单块 32 MiB；累计对象上限为 100,000,000，累计解压为 16 GiB，完整对象累计估算工作量为 64 GiB（`osm_work_bytes`）。这些计数不代表保留内存；对象逐条交给磁盘暂存，批次、单条记录、scratch、关联和限制聚合各有独立预算。`version.capabilities.external_decode_limits` 声明这些工作量上限；原 `limits` 继续描述默认内存/city 模式。full/structure 预检仍按内存容量拒绝超限输入，不能用来判断 external 是否可构建。实际 external build 执行语义检查；图及加载使用上述显式容量和内存预算，3 GiB 输入仍不支持。

上述限制是独立容量门槛，不是进程 RSS 保证；对象估算不包含全部临时缓冲、图和查询索引。数据集还受 [图文件和对象数量限制](dataset-format.md) 约束。CLI 不提供调高上述输入限制的参数；external 提供 scratch 预算，但不提供阶段缓存、恢复或 RSS 硬预算参数。宿主应根据实际资源设置进程级资源限制。

### 容量错误

主要输入、解码、对象和图容量错误在原有 kind/退出码之外返回 `error.resource`：

```json
{"resource":"osm_objects","limit":2000000,"observed":2000001,"stage":"pbf_decode"}
```

按 resource/stage 分型，不解析 message。主要阶段为 input、pbf_read、pbf_decode、graph_build、graph_validation、dataset_manifest、dataset_read、dataset_write；external 还会返回 external_build、external_join、external_edges、external_adjacency、external_restrictions、external_blockways、v2_validation 等阶段。自动扣除保留文件后，子阶段 resource.limit 可能小于 CLI 总预算。estimated_osm_bytes 是对象估算，decoded_bytes 是累计处理工作量，均不是进程内存硬限制。resource 为可选字段；部分容量或解析错误仅返回 kind/message，调用方应处理字段缺失。

## 无效限速值的显式回退

build 和 full preflight 接受 `--invalid-maxspeed reject|use_default`，默认 `reject`。内存和 external 构建使用相同策略；structure 预检不解释 profile，显式传入此参数会被拒绝。`version.invalid_maxspeed_policies` 声明支持的选项。

`reject` 对无法解析的文本、非有限数、非正数或大于 300 km/h 的值返回退出码 4、`details.failure_code=InvalidMaxspeed`。`use_default` 显式允许对这些无效值估算行驶成本：通用 `maxspeed` 回退到道路类别默认速度；`maxspeed:forward/backward` 回退到已解析的通用速度。有效限速仍取它与回退上限的较低值，mph 单位换算继续生效。缺失或空白值沿用既有默认行为，不计入回退。条件、通行权限、物理尺寸和转向限制仍按 driving-v1 处理。

```bash
routeweft build --input region.osm.pbf --profile driving --builder external \
  --format 2 --output region/ --scratch-dir scratch/ \
  --max-scratch-bytes 8589934592 --invalid-maxspeed use_default --json
```

该选项会改变含无效限速道路的预计耗时，可能影响路线选择；默认速度是成本估计，不能当作已核实的法定限速。不会因为限速回退删除道路。成功或失败报告包含 `maxspeed_policy` 和 `maxspeed_diagnostics`，例如：

```json
{"way_id":1533487270,"field":"maxspeed","value":"辅路","action":"use_default","effective_kmh":50.0}
```

实际回退速度按道路类别及方向计算。严格失败时 action 为 `rejected`、effective_kmh 为 null。诊断保留完整原值，最多 256 条且累计字段/值字节加每条 128 字节计费不超过 64 KiB；超限返回 `maxspeed_audit_entries` 或 `maxspeed_audit_bytes`（stage=`profile`），不会丢弃警告后继续。审计描述最终构图尝试中采用或拒绝的值，限制关系重试排除的道路不再出现在最终限速审计中。

成功数据集 manifest 的 diagnostics 保存 `maxspeed_policy_use_default=1`（显式启用时）及 `maxspeed_fallback_values`（发生回退时），逐道路审计位于 build/preflight 报告中，调用者应保留报告。此策略独立于 `--invalid-restrictions`；处理未知限速不表示允许无效转向关系。

## 限制关系诊断与显式保守模式

build/preflight 接受 `--invalid-restrictions reject|block_ways`，默认 reject，不提供 ignore 选项。

缺少 via 的相关机动车限制保持严格失败，示例（省略其他 report 字段）：

```json
{"schema":1,"status":"error","error":{"kind":"InvalidDataset","message":"relation 6843783: MissingVia: restriction has no via member; turn location is not inferred","details":{"valid":false,"policy":"reject","failure_code":"InvalidRestriction","restriction_diagnostics":[{"relation_id":6843783,"code":"MissingVia","action":"rejected","tags":{"restriction":"no_right_turn","type":"restriction"},"members":[{"kind":"way","id":389194568,"role":"from"},{"kind":"way","id":389190340,"role":"to"}],"members_truncated":false}]}}}
```

使用 `error.details.restriction_diagnostics[].code` 分型，不解析人类可读 message。稳定分类包括 MissingVia、MissingFromTo、MissingReference、InvalidMemberType、UnsupportedMultiplicity、UnsupportedViaShape、UnsupportedRestriction、UnresolvedRestriction、RestrictionLimitExceeded；含义见 [profile](driving-profile.md)。PBF 解码错误使用 details.failure_code=PbfReadError，其他图校验错误为 GraphValidationError；读取前的文件错误为 InputReadError，数据集写入失败为 DatasetWriteError；底层 kind/退出码仍保留原契约。早于读取的 IO/限额失败可能没有来源身份或完整诊断。

诊断默认累计最多保留 4,096 条，并受下述字节与逻辑内存预算约束；每条最多 256 个成员，超出时 members_truncated=true。诊断标签最多 32 项，键/值最多 128/512 字符，role 最多 64 字符；完整源信息仍需检查原始 PBF。报告是有界诊断，不是完整 OSM 原件的副本。

只有经过审核后才显式启用：

```bash
routeweft preflight --input city.osm.pbf --profile driving --invalid-restrictions block_ways --json
routeweft build --input city.osm.pbf --profile driving --invalid-restrictions block_ways --output dataset/ --json > build-result.json
```

block_ways 排除无效关系中已存在的全部 way 成员，并迭代复核其他限制；无法安全阻断、图为空、预算或普通解析失败仍拒绝。它不会推断 via、修补源数据、删除限制后放行，也不宣称源输入完全有效。可能明显降低道路覆盖与可达性。

成功 build 新增 report 字段，包含 policy、source_sha256/source_snapshot、最终节点/边/禁止序列数量、计数、逐关系审计和 blocked_way_ids。preflight/build 的 report.valid=true 表示在所选策略下图校验通过，允许同时存在 action=blocked_ways 的告警。失败报告中的节点/边数可能仅反映失败尝试，不是有效成品。

完整审计位于 CLI report，不新增 dataset v1 必需字段。请保存 build-result.json 并随构建回执分发；manifest.diagnostics 只保存计数，不包含关系级完整审计。库调用者使用 build_with_options/prepare/preflight 及结构化 Error.report；默认 build/graph 仍严格拒绝。

external 的流水线、源文件复核及发布错误也保留 `error.details`；通用 failure_code 为 ExternalBuildError，最终数据集写入为 DatasetPublicationError，输入变化为 SourceChanged，阶段截止/取消为 BuildTimeout/BuildCancelled。静态限制仍使用 InvalidRestriction 及关系级诊断。早期失败可能尚无来源摘要；各阶段资源错误按 error.resource 分型。源关系按 ID 排序处理，未排序 PBF 的诊断列表次序可能与内存模式不同，关系身份与处理策略保持。

2026-10-29 算法升级：加速文件为 `RWFTML02`，旧 `RWFTML01` 需重新执行 customize；基础地图不需重新导入。auto 使用双向限制状态查询、图割分区与度二链压缩。route 的小分量替代吸附受原吸附半径约束，并通过 diagnostics.snap_fallbacks 报告；nearest 返回实际最近候选。完整语义与资源上限见[加速指南](routing-acceleration.md)。

### 转向限制审计预算

build 和 full preflight 接受以下预算；structure 预检拒绝这些参数。默认审计可保留超过 256 条异常，但仍有条数、序列化字节和保守逻辑内存三重边界：

| 参数 | 默认 | 有效范围 |
| --- | ---: | --- |
| `--max-restriction-diagnostics` | 4096 | 1～32768，跨轮次累计记录数 |
| `--max-restriction-audit-bytes` | 16777216 | 1～16777216 字节，包含可能改为 blocked_ways 的预留 |
| `--max-blockways-attempts` | 64 | 1～257，仅显式 block_ways；包括首次构图 |

内存审计另有固定 64 MiB 保守对象收费，均不是进程 RSS 限制。条数、字节不能设置为 0；非法配置在读取 PBF 前失败。仅最后一轮仍需重建时才触发轮次超限，不会启动被禁止的下一轮。单条关系的成员展示、路径长度、搜索和访问预算不随审计数量增加而改变。

超限返回退出码 4 / BudgetExceeded，`error.resource` 包含 `restriction_audit_records`、`restriction_audit_bytes`、`restriction_audit_memory_bytes` 或 `blockways_attempts`，以及 limit、observed、stage。failure_code 为 RestrictionAuditLimitExceeded 或 BlockWaysAttemptLimitExceeded，不再因诊断列表溢出而丢弃已收集关系。关系扫描中其他资源/读取错误同样保留已收集的审计。

`error.details.restriction_audit`（成功时为 report.restriction_audit）包含 records_observed、records_retained、retained_bytes、retained_memory_bytes、scan_complete、truncated、attempt、attempts_completed 和 limits。observed 是已发现失败事件的数量，跨轮次不保证唯一；scan_complete=false 时只是下界，不能解释为整个输入的异常总数。每条诊断新增 attempt，标明发现轮次。members_truncated 仅表示单条关系展示成员截断，与完整关系列表的 truncated 不同。只有完整的 InvalidRestriction 列表才允许 BlockWays 排除重建；有部分诊断的预算错误仍停止。

BlockWays 中间审计 manifest 写入版本 2，固定最大 32 MiB，读取器仍兼容旧版本 1（1 MiB/256 条）。该大小与子阶段元数据预留计入 scratch；预算不足可能在构图前失败。connection/edges/adjacency 等 child manifest 仍是 1 MiB，最终路网二进制格式不变。调用者应保存成功报告中的排除审计。

```bash
routeweft build --input region.osm.pbf --profile driving --builder external --format 2 \
  --graph-capacity regional --invalid-restrictions block_ways --invalid-maxspeed use_default \
  --max-restriction-diagnostics 4096 --max-restriction-audit-bytes 16777216 \
  --max-blockways-attempts 64 --scratch-dir ./scratch --max-scratch-bytes 21474836480 \
  --max-scratch-files 4096 --output ./new-regional-dataset --json > build-result.json
```

足够的首轮审计容量不代表所有级联轮次可完成，也不代表排除后的路网质量达标。先验证、再抽样路线；不对缺失 via 或不支持的关系推断合法运动。省级输入应运行 regional/external build，不能用仍受内存对象/城市跨度限制的 full preflight 判断可构建性。
