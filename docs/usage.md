# 使用 RouteWeft

源码地址：https://github.com/mengxing2017/routeweft 。当前使用方式是从源码构建 CLI，或将 Rust crate 嵌入宿主。`routeweft-daemon` 是 CLI 所在 crate 的名称，当前不提供 HTTP 服务。

## 获取与构建

准备 Git、rustup、系统 C 链接器；合成示例另需 Python 3。Linux 是当前已验证平台。首次执行 Cargo 会按照 `rust-toolchain.toml` 获取指定工具链，并下载锁定依赖。

```bash
git clone https://github.com/mengxing2017/routeweft.git
cd routeweft
cargo build --manifest-path backend/Cargo.toml --release --locked
backend/target/release/routeweft --help
```

可执行文件位于 `backend/target/release/routeweft`，可以使用绝对路径调用。正式构建应记录 `git rev-parse HEAD`，固定输入 PBF 的 SHA-256，并保留 `Cargo.lock`；更新代码后按 [数据集格式](dataset-format.md) 检查兼容性。源码可获取与代码使用授权是两件事，当前许可状态见 [README](../README.md#文档和发布边界)。

无需真实地图的完整冒烟流程见 [快速开始](../README.md#快速开始)。合成坐标仅适用于该示例，不能拿来查询真实城市。

## 使用自己的城市 PBF

从合法来源准备城市级原始 OSM `.osm.pbf`。以下命令在 RouteWeft 仓库根目录执行，需将输入路径替换成已有文件；输出目录必须不存在：

```bash
mkdir -p datasets
backend/target/release/routeweft build \
  --input /absolute/path/to/city.osm.pbf \
  --profile driving --output ./datasets/city-driving-v1
backend/target/release/routeweft verify --dataset ./datasets/city-driving-v1 --json
backend/target/release/routeweft inspect --dataset ./datasets/city-driving-v1 --json
```

构建前先创建输出的父目录（例如 `mkdir -p datasets`），不要预先创建数据集目录本身。构建后目录包含 `manifest.json` 和 `graph.json`；部署时完整复制该目录，再在目标机器执行 verify。

以下是 Monaco 范围内的示例坐标，仅在输入覆盖该区域且附近道路可通行时适用；使用其他城市时替换坐标。顺序为 **经度,纬度**：

```bash
backend/target/release/routeweft nearest \
  --dataset ./datasets/city-driving-v1 --coordinate '7.4206,43.7311' --json
backend/target/release/routeweft route \
  --dataset ./datasets/city-driving-v1 \
  --coordinates '7.4206,43.7311;7.4269,43.7384' --json
```

结果为 JSON，路线包含几何、米制距离、秒制预计耗时、分段及基本步骤。`NoSegment` 表示没有找到支持范围内的道路候选，`NoRoute` 表示当前图和规则下查询无路；二者均返回退出码 3。完整参数、预算和错误见 [CLI](cli.md)。

当前是静态汽车子集，条件规则保守处理；CLI 输入 PBF 上限为 512 MiB，另有解码、对象和图容量限制，查询要求经纬跨度各不超过 5°。文件小于上限也不保证满足其他容量限制。优先使用城市级数据，不将省域、全国地图构建能力视作路由容量承诺。详见 [profile](driving-profile.md)、[数据格式](dataset-format.md) 和 [性能记录](performance.md)。

## 消费者接入

外部构建工具可以调用 RouteWeft CLI，使用原始 OSM PBF 生成路由数据集；不要输入地图过滤后的 PBF、中间 GeoJSON 或 PMTiles。构建时固定源码提交、输入 SHA-256、profile 和显式策略，并保存构建与预检报告。命令和错误契约见 [CLI](cli.md)，数据集身份与完整性要求见 [数据集格式](dataset-format.md)。

消费者负责工具锁定、区域配置、质量门、打包安装及更新流程。RouteWeft 提供路由能力，不维护消费者的代码、配置或发布流程。地图与路由配对时应记录地图版本、dataset_id 和源 PBF 哈希；构建日期不能代替实际数据快照。

## 城市输入预检与独立复核

先运行 `routeweft preflight --input city.osm.pbf --profile driving --json`。转向限制失败时，使用关系 ID 对比原始输入与切片，见 [数据提取和排查](data-sources.md)。只有人工审核过覆盖损失后，才考虑 [显式保守模式](cli.md#限制关系诊断与显式保守模式)。默认严格失败不会改变。

本项目提供独立验证脚本，需 Python 3.12+，使用显式本地文件，不下载数据。以下坐标及 policy 对应固定南宁切片，输出目录必须不存在；其他输入须替换成经过审核的成功与无路坐标：

```bash
python3 scripts/validate-city.py --binary backend/target/release/routeweft \
  --input /absolute/path/to/nanning.osm.pbf --output /absolute/path/to/new-acceptance \
  --invalid-restrictions block_ways \
  --coordinates '108.32,22.82;108.33,22.82' \
  --no-route '108.2532541,22.7745994;108.2534996,22.7738817' --no-route-radius-m 0.01
```

脚本执行 version、preflight、重复 build、verify、成功 nearest/route、明确 NoRoute，以及自行生成的 ZIP/tar.gz 解包后重复检查。acceptance.json 保存每次命令的总耗时、可选 Linux VmHWM 采样、输入/工具身份、诊断与归档哈希。RSS 采样可能漏掉短峰值，不能当作精确峰值或资源隔离。生成的归档是独立复核用文件，不作为消费者的发布包或客户端安装包。

## 嵌入 Rust 应用

公开入口是 `routeweft_routing::Engine::open/nearest/route`，见 [API 与示例](api.md)。常驻宿主应复用已加载的 Engine，而不是每次查询重新启动 CLI 和加载图。CLI 适合构建、检查、脚本调用与初步接入；进程宿主负责超时、并发和生命周期。
