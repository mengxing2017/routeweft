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

构建宿主可依赖 `routeweft-preprocess`：`preflight(&Path, BuildOptions)` 返回 InputReport，不写数据集；`build_with_options(&Path, &Path, BuildOptions)` 返回 `(Manifest, InputReport)`。`prepare(&Osm, BuildOptions)` 用于已解码对象。默认 build/graph 使用 Reject，显式选项为 InvalidRestrictionPolicy::BlockWays。

预处理错误类型为 `routeweft_preprocess::Error`，保留底层 source 和结构化 report；不再要求调用者解析 InvalidDataset 的文本。Relation 现在保留源 id；直接构造测试 OSM 时须提供唯一关系 ID。消费者应固定提交并重新编译，不能假定 0.x 内部类型向后兼容。

InputReport 是构建审计，不是可传给查询引擎的路由图。保存它与输入/工具身份；Engine 仍加载原有 format=1 数据集。策略含义和诊断分类见 [CLI](cli.md)。
