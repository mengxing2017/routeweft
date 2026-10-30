# Rust 嵌入 API

`routeweft-routing` 可直接链接，不依赖 daemon 或异步运行时。当前 API 为 0.x，不承诺与 P0 原型兼容。

```rust,ignore
use routeweft_dataset::{LoadOptions, Point};
use routeweft_routing::{Engine, QueryOptions, SearchBudget};
use std::path::Path;

let engine = Engine::open(Path::new("dataset"), LoadOptions::default())?;
let query = [Point { lon: 7.4206, lat: 43.7311 },
             Point { lon: 7.4269, lat: 43.7384 }];
let route = engine.route(&query, QueryOptions::default(), &SearchBudget::default())?;
let matches = engine.nearest(query[0], QueryOptions::default(), &SearchBudget::default())?;
```

可编译版本见 `backend/crates/routeweft-routing/examples/load_dataset.rs`：

```bash
cargo run --manifest-path backend/Cargo.toml -p routeweft-routing --example load_dataset -- dataset/
```

Engine 持有不可变图、R-tree 和限制自动机，允许 Arc 共享并发只读查询；每次查询使用独立状态。`manifest()` 返回当前数据集身份。`from_graph()` 为测试/受信任嵌入输入入口，校验图结构但不代替磁盘完整性验证；生产数据建议 open。

`QueryOptions` 指定 Driving、最大吸附半径和候选数；Cycling/Walking 返回 UnsupportedProfile。`SearchBudget` 指定 max_work、max_states、max_queue、max_geometry、可选 Instant deadline 以及可选 AtomicBool 取消标记。默认最大几何点数 100,000；取消标记由调用者管理，库不偷偷生成后台任务。预算耗尽与 NoRoute 分别返回，调用者负责排队及并发限制。

错误枚举区分 InvalidInput、UnsupportedProfile、NoSegment、NoRoute、InvalidDataset、IncompatibleFormat、BudgetExceeded、Cancelled、CostOverflow 和 Io。查询 API 不 panic 终止宿主；资源申请仍受平台分配失败行为影响，应配合输入及部署限制。

NodeEngine 保留节点级 P0 搜索，只有合成图用途，不提供持久格式或完整坐标语义。使用多个独立查询拼接不会继承转向历史；多途经点必须一次传给 Engine::route。

## 构建宿主与预检

构建宿主可依赖 `routeweft-preprocess`：`preflight(&Path, BuildOptions)` 返回 InputReport，不写数据集；`build_with_options(&Path, &Path, BuildOptions)` 返回 `(Manifest, InputReport)`。`prepare(&Osm, BuildOptions)` 用于已解码对象。默认 build/graph 使用 QuarantineWays，隔离可界定的异常限制涉及道路并保留审计；显式 Reject 用于严格拒绝，BlockWays 保留原 from/to 要求。

预处理错误类型为 `routeweft_preprocess::Error`，保留底层 source 和结构化 report；不再要求调用者解析 InvalidDataset 的文本。Relation 现在保留源 id；直接构造测试 OSM 时须提供唯一关系 ID。消费者应固定提交并重新编译，不能假定 0.x 内部类型向后兼容。

InputReport 是构建审计，不是可传给查询引擎的路由图。保存它与输入/工具身份；Engine::open 自动读取 format=1/2 数据集。策略含义和诊断分类见 [CLI](cli.md)。

`build_with_options` 和 `routeweft_dataset::write` 默认生成 v1。显式选择二进制可调用 `build_with_format(input, output, options, routeweft_dataset::DatasetFormat::BinaryV2)`，已有 Graph 可调用 `routeweft_dataset::write_with_format`。两者保留原有审计与不可变发布规则；v2 不改变 Engine 的驻内存模型。格式契约见 [Dataset v2](dataset-format-v2.md)。

显式磁盘构建可调用 `routeweft_preprocess::staging::build_external(input, output, scratch_parent, ExternalBuildOptions, ExternalBuildControl)`，返回 `ExternalBuildResult { manifest, report, resources }`，只生成 v2。全局磁盘预算使用 `options.stage.scratch_bytes/run_files`，另可配置字典、校验及限制工作集预算。`ExternalBuildControl` 接受可选的 `AtomicBool` 取消标记和 `Instant` 截止时间，只在阶段边界/最终提交前检查，不会中断当前阶段。scratch 和输出须在同一文件系统，输入与父目录在调用期间保持不变；具体预算和失败清理契约见 [磁盘构建](cli.md#显式磁盘构建)。原有 build API 继续使用内存路径；两种模式都不扩大现有输入/图上限或改变 Engine 的驻内存模型。

### 构建审计配置与兼容

`BuildOptions.restriction_audit: RestrictionAuditLimits` 控制累计审计数量（默认 4096）、序列化预算（默认 16 MiB）和 BlockWays 总构图轮数（默认 64）。调用 `validate()` 可提前校验；`prepare`、PBF 构建及 external 入口也校验。推荐使用 `..Default::default()` 初始化其余选项。直接使用 `RestrictionOptions` 的调用者通过 `audit` 配置同一限制，外部构建通过 `ExternalBuildOptions.build.restriction_audit` 透传。

`InputReport.restriction_audit: RestrictionAuditSummary` 提供实际限额、扫描完整性、观察数/保留数和保守计费；`RestrictionDiagnostic.attempt` 记录轮次。观测计数不是唯一关系总数，提前停止时仅为下界。原 JSON schema=1 和原字段保留，新字段为增量；Rust 结构体字面量需要增加字段或采用 Default。这次 API 增量随新的预发布版本发布，不复用已上传版本。

独立 publication 读取器支持 BlockWays v1/v2 中间 manifest，v2 固定硬边界不相信文件自报的较大限额；最终 dataset graph 格式不变。审计逻辑内存预算不是 RSS，错误仍需匹配 resource 与完整性标记，不能因为 restriction_diagnostics 非空就忽略资源失败。

## 可选 Tokio 宿主层

`routeweft-runtime` 不依赖 daemon。宿主先授权并解析不可变目录，再传入期望身份和配额配置：

```rust,ignore
use routeweft_dataset::DatasetIdentity;
use routeweft_runtime::{EngineLoader, Runtime, RuntimeConfig};
use routeweft_routing::QueryOptions;
use std::{sync::Arc, time::{Duration, Instant}};

let runtime = Runtime::new(Arc::new(EngineLoader), RuntimeConfig::default());
let deadline = Instant::now() + Duration::from_secs(10);
let expected = DatasetIdentity::from(&trusted_manifest);
let engine = runtime.load(dataset_path, expected, deadline).await?;
let route = runtime.route(engine, coordinates, QueryOptions::default(), deadline).await?;
```

默认加载采用 city 档位，区域宿主需配置 `RuntimeConfig.load.capacity` 与对应预算。`DatasetIdentity::verify` 只负责期望身份与 manifest 的完整比对，图结构与摘要校验仍由 dataset/Engine 的加载 API 完成。runtime 的真实配额保持、文件保活和准备校验语义见 `backend/crates/routeweft-runtime/README.md`。

本次 runtime 提取已使用 Rust 1.85.0、`--locked --offline` 验证 dataset/routing/runtime 的编译与测试；workspace 继续声明 edition 2021、Rust 1.85。宿主可使用更高版本工具链。
