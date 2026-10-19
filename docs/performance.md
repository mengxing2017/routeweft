# 城市验证与性能记录

日期：2026-10-19。前半部分记录固定历史 Monaco 数据，后文补充南宁切片的显式保守模式验证；均为指定硬件下的测量，不是所有城市、当前道路或生产性能承诺。完整摘要见 [city-validation-summary.json](city-validation-summary.json)，输入及许可见 [数据来源](data-sources.md)。

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

## 南宁固定切片：显式保守模式验证

2026-10-19 使用 [数据来源](data-sources.md) 中固定南宁切片，JSON 摘要见 [nanning-validation-summary.json](nanning-validation-summary.json)。这次结果使用 `--invalid-restrictions block_ways`：5 条源数据无效 relation 涉及的 8 条道路被排除。默认 reject 仍失败，不把该结果描述为未经排除的完整城区导航，也不替代消费者的部署验收。

数据集 `d0b8611982f748ca5276d8b32d2076e07ef137cb3a9c7567ee9a663d4363718b`：42,345 节点、68,164 方向边、33 条禁止序列，28 条成功解析的静态 relation。PBF 1,336,837 bytes，graph+manifest 共 10,982,096 bytes。连续两次构建的两个核心文件逐字节一致；归档包含附加审计，ZIP 1,249,155 bytes、tar.gz 1,183,625 bytes。归档时间戳可变化，不承诺归档字节可复现。

同一开发主机 Linux x86_64 / Xeon E-2278G / 16 逻辑 CPU / 约 62.6 GiB RAM，Rust 1.97.1 release/LTO，OS 页缓存未清、机器非隔离。下表为独立 CLI 进程的总时延，包含启动、加载、哈希和协议输出；RSS 是执行新程序后每约 2 ms 采样的 VmHWM，可能漏峰，不能作为硬内存保证。

| 命令 | 耗时 ms | 采样 VmHWM KiB |
| --- | ---: | ---: |
| preflight | 258.45 | 60,260 |
| build（首次） | 468.64 | 79,408 |
| build（重复） | 408.04 | 79,336 |
| verify | 74.25 | 23,168 |
| nearest | 94.63 | 25,000 |
| route（成功） | 99.29 | 25,288 |
| route（明确 NoRoute） | 94.93 | 25,000 |

独立库探针加载约 91.46 ms；加载后内核 VmHWM 25,016 KiB，1,000 OD 后 33,424 KiB。固定种子 20261019，在保留方向边端点/中点采样 1,000 OD：840 成功、160 NoRoute，全部响应 p95 19.22 ms，无预算失败或其他错误。无路只代表当前图、规则和吸附策略，不证明现实道路不连通。

持续到达负载采用既有 city_benchmark：预热 30 秒，测量 300 秒，Route/Nearest 各 5 QPS，各自独立 worker，包含排队和失败响应。Route 1,500 次，1,260 成功、240 NoRoute，p95 37.35 ms；Nearest 1,500 次全成功，p95 0.81 ms。该值为复用引擎的查询时延，与包含加载的 CLI 时延不是同一口径。测量期间同机存在开发检查，不外推为生产性能承诺。

成功查询：`108.32,22.82;108.33,22.82`，1,864.268 m / 123.488 s。明确单行反向无路：`108.2532541,22.7745994;108.2534996,22.7738817`，吸附半径 0.01 m，退出 3/NoRoute；对应保留 way 57038484 的正向查询成功。

另用隔离的实验对照图仅移除禁止序列，核查真实限制确实影响查询：way 388985484→999853018 的边中点查询，正式限制图耗时 173.115 s、对照 0.829 s；way 395680031→1527161847 为 7.508 s / 3.057 s（半径 0.01 m）。对照图只用于验证限制作用，绝不发布或用来绕过质量门。精确坐标保存在公开 JSON 摘要；新增多入口/出口合法语义另有合成禁止/允许回归。

复现构建、明确成功/无路以及两种归档解包复核，使用 [城市验证脚本](usage.md#城市输入预检与独立复核)。持续负载与内存探针使用上文相同示例程序，把数据集和测量目录替换为此次显式策略构建的目录。真实 PBF 和完整原始结果不入源码，摘要不包含开发机路径。

这是 RouteWeft 独立校验与实验归档结果，尚未覆盖联合安装、冷磁盘或消费者部署验收。项目原创部分的许可决策仍未完成。
