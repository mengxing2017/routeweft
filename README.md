# RouteWeft

基于 OpenStreetMap 的 Rust 离线路网构建与路由工具。提供可嵌入库及 `routeweft` CLI，不依赖外部路由运行时、数据库或业务服务。

已实现城市级汽车子集：PBF 读取、保守通行规则、有向建图、静态 no/only 转向限制（via-node/via-way）、版本化数据集、道路吸附、2～10 个连续途经点的最快路及基本结构化指引。条件规则采取保守排除策略，不能解释为完整导航产品或完整汽车法规支持。支持范围和近似见 [profile 说明](docs/driving-profile.md)。提供 `preflight` 输入预检和关系级限制诊断；无效限制默认拒绝，可显式选择排除受影响道路的保守模式。

源码仓库：[mengxing2017/routeweft](https://github.com/mengxing2017/routeweft)。完整的获取、构建、真实数据查询及地图构建协作流程见 [使用指南](docs/usage.md)。

## 快速开始

准备 Git、通过 rustup 管理的 Rust 工具链、系统 C 链接器及 Python 3。当前验证平台为 Linux；工具链版本由 `rust-toolchain.toml` 指定。首次构建需要下载工具链和 Cargo 依赖。

```bash
git clone https://github.com/mengxing2017/routeweft.git
cd routeweft
```

在仓库根目录执行以下合成数据示例；每次使用新的临时目录：

```bash
cargo build --manifest-path backend/Cargo.toml --release --locked
routeweft_demo_dir="$(mktemp -d)"
python3 backend/testdata/generate_fixture.py "$routeweft_demo_dir/synthetic.osm.pbf" --dense
backend/target/release/routeweft build --input "$routeweft_demo_dir/synthetic.osm.pbf" --profile driving --output "$routeweft_demo_dir/dataset"
backend/target/release/routeweft verify --dataset "$routeweft_demo_dir/dataset" --json
backend/target/release/routeweft inspect --dataset "$routeweft_demo_dir/dataset" --json
backend/target/release/routeweft nearest --dataset "$routeweft_demo_dir/dataset" --coordinate '0.0005,0' --json
backend/target/release/routeweft route --dataset "$routeweft_demo_dir/dataset" --coordinates '0.0002,0;0.0008,0;0.002,0.0005' --json
```

输出目录必须不存在。真实城市使用显式提供的 `.osm.pbf` 替换合成输入，工具不会下载数据。完整数据来源和固定验证样本见 [数据说明](docs/data-sources.md)。

## 库与模块

`backend/` 是独立 Cargo workspace：

| crate | 职责 |
| --- | --- |
| routeweft-domain | 零基础设施依赖的坐标和领域约束 |
| routeweft-dataset | 图、manifest、完整性检查、受限读取和发布 |
| routeweft-preprocess | PBF、汽车规则、拓扑与静态限制 |
| routeweft-routing | 只读引擎、索引、限制自动机、查询和指引 |
| routeweft-daemon | CLI 进程及保留的旧编排接口 |

公开嵌入入口是 `routeweft_routing::Engine::open/nearest/route`；见 [API](docs/api.md) 和 `load_dataset` 示例。库不依赖 CLI、HTTP 或异步运行时。消费者负责认证、发布生命周期、地图展示和业务协议。

## 验证

```bash
cargo fmt --manifest-path backend/Cargo.toml --all -- --check
cargo clippy --manifest-path backend/Cargo.toml --workspace --all-targets --locked -- -D warnings
cargo test --manifest-path backend/Cargo.toml --workspace --locked
cargo doc --manifest-path backend/Cargo.toml --workspace --no-deps --locked
```

端到端 fixture 使用 Python 3 标准库；许可清单检查需要 Python 3.11+（tomllib）。真实城市和持续负载基准是显式可选任务，不加入普通离线测试。见 [性能与复现](docs/performance.md)。前端仍为独立静态首页；本批次不新增地图业务页面或 HTTP 服务。

## 文档和发布边界

- [使用指南与消费者接入](docs/usage.md)
- [CLI 与退出码](docs/cli.md)
- [数据集格式](docs/dataset-format.md)
- [架构](docs/architecture.md)与 [ADR 0002](docs/adr-0002-dataset-and-routing.md)
- [命名迁移](docs/migration.md)
- [第三方来源与分发材料](THIRD_PARTY_NOTICES.md)

CH/MLD、全国性能、跨数据集拼接、实时交通、骑行步行、完整车道引导和移动定位播报不在支持范围。数据集检查不提供来源签名或身份认证。

源码已托管到上述 GitHub 仓库，所有 crate 保持 `publish=false`；本文提供源码构建方式。当前仓库尚未声明项目原创部分的许可证。复用代码、依赖及 OSM 数据分别遵循各自条件；已有 BSD 声明不等于整仓库采用 BSD。
