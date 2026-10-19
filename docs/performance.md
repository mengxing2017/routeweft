# 城市验证与性能记录

日期：2026-10-19。下面是固定历史 Monaco 数据和指定硬件下的单次测量，不是所有城市、当前道路或生产性能承诺。完整摘要见 [city-validation-summary.json](city-validation-summary.json)，输入及许可见 [数据来源](data-sources.md)。

## 条件

- Intel(R) Xeon(R) E-2278G CPU @ 3.40GHz，16 逻辑 CPU，约 62.6 GiB 内存；Linux x86_64，Rust 1.97.1 release/LTO。
- 单数据集 driving-v1：15,539 节点、27,145 方向边、35 条禁止序列（27 个解析成功的 relation 展开而来）。
- 输入 1,759,101 bytes；图与 manifest 共 4,455,764 bytes，逐文件 gzip 后合计 422,082 bytes（仅容量测量，加载器不读 gzip 数据集）。
- 构建 0.176 s，构建进程 exec 后采样 VmHWM 67,216 KiB；内存探针的引擎新进程加载 33.93 ms。
- 独立进程加载后 VmHWM 11,180 KiB，执行 1,000 OD 后 VmHWM 13,068 KiB。内存包含引擎、查询及探针输入；不等于算法临时分配的精确差值或硬 RSS 配额。
- 文件系统页缓存未清理；“新进程加载”不是冷磁盘测试。主机并未专用隔离，同期存在开发检查；不外推不同硬件和负载。

## 固定 OD 与持续到达负载

种子 20261019，在固定方向边端点/中点采样 1,000 组：846 组成功、154 组 NoRoute，全部响应 p95 为 5.83 ms。NoRoute 表示在当前保守 profile、方向候选和限制模型下无路，不能据此证明真实道路绝对不连通。没有 NoSegment、BudgetExceeded 或进程异常。

持续负载先预热 30 秒，再测量 300 秒；Route 和 Nearest 各 5 QPS，分别使用单 worker，按固定时刻将请求送入各自队列。延迟从计划到达时刻到响应完成，包含调度/排队，且不剔除无路响应。

| 操作 | 测量请求 | 成功 | 业务无路 | p95 | 候选门槛 |
| --- | ---: | ---: | ---: | ---: | ---: |
| Route | 1,500 | 1,261 | 239 | 24.04 ms | ≤500 ms，通过 |
| Nearest | 1,500 | 1,500 | 0 | 2.85 ms | ≤50 ms，通过 |

Route 的非成功响应均为 NoRoute，约 15.93%；没有超时、预算失败或异常。端点/中点采样不是线上用户请求分布；本结果不能代替更大区域、复杂限制、移动定位、冷磁盘和资源受限环境的验证。未运行同源第三方引擎差异对照，也未验证 1.85 MSRV 或其他平台。

## 复现

将固定哈希 PBF 放到自选目录，输出目录须不存在：

```bash
cargo build --manifest-path backend/Cargo.toml --release --locked
backend/target/release/routeweft build --input monaco.osm.pbf --profile driving --output dataset/
backend/target/release/routeweft verify --dataset dataset/ --json
cargo run --manifest-path backend/Cargo.toml -p routeweft-routing --release --example city_benchmark -- dataset/ measurements/
cargo run --manifest-path backend/Cargo.toml -p routeweft-routing --release --example memory_probe -- dataset/ measurements/od.json
```

基准会实际运行约 5 分半；生成 od.json、od-results.json、load-results.json 和 summary.json，不联网。相同配置连续构建的 graph.json 与 manifest.json 已逐字节比对一致。

当前验证只覆盖城市级保守子集。更多真实场景、车辆规则和大区域性能均需独立证据；不得把这份记录解释为完整功能覆盖或生产导航资格。
