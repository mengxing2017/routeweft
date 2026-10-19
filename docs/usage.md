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

## 与 map-build 配合

[map-build](https://github.com/mengxing2017/map-build)（本地项目名 `huavd-map-build`）也是独立的开源项目，可与 RouteWeft 复用同一份原始 PBF：

```text
同一份城市 OSM PBF
  ├─ map-build → PMTiles、地图样式及配套资源 → 地图展示
  └─ RouteWeft → 路由数据集 → CLI / Rust API → 路线几何与指引
```

推荐在同一父目录分别检出 `routeweft` 与 `map-build`，数据放在独立目录。完成 map-build 自身 README 的环境准备后，在 RouteWeft 根目录执行下面的示例。`monaco` 是 map-build 当前区域目录中的 ID，PBF 路径需替换成实际的 Monaco 原始数据：

```bash
routeweft_source_pbf=/absolute/path/to/monaco.osm.pbf
routeweft_map_workdir="$PWD/map-work"
routeweft_build_date="$(date +%Y%m%d)"
sha256sum "$routeweft_source_pbf"

make -C ../map-build build REGION=monaco YYYYMMDD="$routeweft_build_date" \
  INPUT="$routeweft_source_pbf" WORKDIR="$routeweft_map_workdir" ROLE=base
make -C ../map-build verify REGION=monaco YYYYMMDD="$routeweft_build_date" \
  WORKDIR="$routeweft_map_workdir" ROLE=base

mkdir -p datasets
backend/target/release/routeweft build --input "$routeweft_source_pbf" \
  --profile driving --output ./datasets/monaco-driving-v1
backend/target/release/routeweft verify --dataset ./datasets/monaco-driving-v1 --json
```

上例展示输入复用方式，未执行整套地图构建验收。使用 map-build 已下载的数据时，向 RouteWeft 传入其原始 PBF 的绝对路径；不要传过滤后的中间数据或 `.pmtiles`。构建日期只是会话标识，不代表 OSM 数据快照，应另记录输入哈希与实际快照。

两个工具当前分别构建和验证，尚无自动整合的发布包或安装协议。宿主可以在地图上绘制 RouteWeft 返回的路线几何；HTTP 接口、坐标交互、路由数据集安装和更新策略由消费者实现。需要联合分发时，在外层清单记录地图包版本、路由 dataset_id、两者输入哈希和数据快照，同时保留 OSM 署名及适用许可材料。

## 嵌入 Rust 应用

公开入口是 `routeweft_routing::Engine::open/nearest/route`，见 [API 与示例](api.md)。常驻宿主应复用已加载的 Engine，而不是每次查询重新启动 CLI 和加载图。CLI 适合构建、检查、脚本调用与初步接入；进程宿主负责超时、并发和生命周期。
