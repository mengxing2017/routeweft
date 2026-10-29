# 长距离路由加速与数据升级

RouteWeft 使用多级单元捷径缩小查询空间。采用 OSRM MLD 的分区、单元权重预计算、查询层级与路径解包思路，按 RouteWeft 的禁止边序列语义重新实现；不是 OSRM C++ 或 `.osrm` 二进制兼容移植。在线使用保持限制前缀和连续途经点历史的双向层级搜索；当前实现不包含 CH、Lua profile 或实时权重更新。

## 升级已有数据集

先构建或安装新 CLI，再对不可变的数据集执行一次离线定制：

```sh
routeweft customize --dataset region/ --graph-capacity regional \
  --max-load-bytes 8589934592
routeweft route --dataset region/ --graph-capacity regional \
  --max-load-bytes 8589934592 --coordinates '108.32,22.82;109.12,21.48'
```

无需重新导入 PBF。加速文件升级为 `RWFTML02`：旧的 `RWFTML01` 必须重新执行 customize，新版本明确返回 IncompatibleFormat，不会误用旧指标。基础 graph 的 v1/v2 格式不变。`customize` 支持 v1/v2，在数据集目录原子发布可选的 `routing.mld`。`build` 仍只发布基础图，**新建图和改变图权重后也需要运行 customize**。部署时一起分发 manifest、graph 文件和 routing.mld；数据集目录在运行期间保持不可变。新版本 Engine 自动加载已有加速文件；没有文件时保留 Dijkstra 回退，不会每次启动重新计算捷径。文件存在但不兼容、损坏或绑定其他图时明确拒绝加载，不静默忽略。

定制默认叶单元 256 节点，8 倍合并，共 3 层。单元边界超过 128 个节点时不生成该单元的 clique，使用较低层或基础图，避免稠密边界产生平方级空间增长。CLI 默认定制工作量 20 亿次，协作时限 30 分钟；`--max-work`、`--timeout-ms` 可以覆盖。库还可配置叶大小、层数、边界数、总捷径数和工作区逻辑字节上限。构图、排序、文件写入等阶段之间和最短路循环内检查时限，不提供硬实时中断。失败不会发布部分文件；目录 sync 失败可能返回错误但保留已经 rename 的完整文件。

加载与定制均将加速器计入 `--max-load-bytes`，与原图一起受逻辑内存估计限制；它不是 RSS 保证。库定制另有默认 2 GiB 工作区上限，总捷径最多 2000 万条。需要更大预算时先检查指标和实际内存，不取消预算保护。

## 正确性与压缩边界

分区采用递归平衡图割：空间方向投影和连通分量提供候选种子，按实际路网割边数选择，再用保持两侧节点数的边界交换改进。它替代原 Morton 固定分组；属于本项目的平衡图割实现，并非逐行移植 OSRM 的最大流求割器。叶单元大小是目标值，各层仍按嵌套单元 ID 聚合。

定制前构造稀疏限制状态图：物理节点的根状态，加上禁止序列自动机中非终止前缀的唯一结束节点。每条允许的状态转移带原始边 ID；禁止转移不进入图。捷径对状态节点进行最短路计算，可以穿过合法的涉限道路，终点携带真实自动机状态，不再统一置零，也不再将所有涉限边强制放在全部层的边界。

所有吸附候选的原始边两端及其限制状态所在单元在每层打开。双向查询搜索同一有向查询图及其转置，以相遇成本更新上界；反向枚举限制自动机前驱，并反演途经点阶段推进。不同层级的入向捷径分别核对其源节点的实际查询层级，避免把“反向沿同层走”误当作正向图的完整转置。同一层、同一单元的连续 clique 展开被剪枝；层级切换不会沿用这个标记。

局部原图查询使用度二链压缩。链保存原始边序列和整数成本前缀；兼容道路属性才允许连接，限制端点及其邻接点保持显式，以保留合法掉头清除限制历史的行为。分区边界、吸附分割和全部途经点构成链的切断点，查询跨链跳跃后仍逐边恢复几何和引导。此处压缩在线查询拓扑，保留原始 graph、空间索引及吸附 ID，没有宣称缩小基础地图文件或消除原图常驻内存。

弱连通分量始终可用，不同弱分量安全地返回 NoRoute；相同分量不代表有向可达。加载阶段也迭代计算强连通分量数量，但不会仅因 SCC 不同就拒绝路线。route 在最近候选没有共同弱分量时，对小于 1000 节点的小分量查找最大弱分量内的候选，仍受同一吸附半径约束，独立于普通 top-k 候选截断；成功回退记录 snap_fallbacks。如果所有途经点都能使用同一小分量，保留该分量。nearest 接口继续返回实际最近候选，不应用 route 的替代策略。

选中捷径逐层解包为原始边，再使用已有几何和引导逻辑；等价最优路径可能采用不同几何，不保证与 Dijkstra 逐点相同。

`--algorithm dijkstra` 强制使用原始搜索，保留相同吸附、限制、成本、引导与预算逻辑，供正确性差分和性能诊断。它仍验证已存在的加速文件；它不是绕过文件校验的选项。库的 `Engine::customize` 可在内存中构造同一加速器，适用于嵌入应用和小图测试。

## 常驻查询，避免每请求加载

```sh
routeweft serve --dataset region/ --graph-capacity regional \
  --max-load-bytes 8589934592
```

进程只加载一次。首行是 `status=ready`、dataset_id、load_ms、load_diagnostics 和加速器信息；随后 stdin 每行一个请求，stdout 每行一个响应，EOF 正常退出。请求串行处理，服务端可使用进程池；当前不提供 HTTP 或内置并发。嵌入式调用可直接复用 `Arc<Engine>`。

```json
{"id":"route-1","operation":"route","coordinates":[{"lon":108.32,"lat":22.82},{"lon":109.12,"lat":21.48}]}
{"id":"nearest-1","operation":"nearest","coordinates":[{"lon":108.32,"lat":22.82}],"radius_m":1000}
{"id":"short-budget","coordinates":[{"lon":108.32,"lat":22.82},{"lon":109.12,"lat":21.48}],"limits":{"max_work":100,"timeout_ms":1000}}
```

operation 默认 route，nearest 必须恰好一个坐标。radius_m 默认 1000。id 原样返回；无法解析的 JSON 返回 null id。每行最大 65536 字节，超长请求会终止会话，其他请求错误不会终止。每个请求有独立队列、预算和时限，不继承上一请求的消耗。limits 可收紧 max_work、max_states、max_queue、timeout_ms，不能超过启动进程时相应的配置上限。没有跨数据集拼接、在线图更新或隐式重载。

## 诊断字段与预算

CLI 查询结果（含查询错误）增加 `diagnostics`：algorithm、elapsed_ms、snap_ms、search_ms、recover_ms、work、states、settled_states、scanned_edges、relaxed_shortcuts、queue_peak、forward_settled、reverse_settled、clique_skips、compressed_edges、snap_fallbacks、failure。failure 含 reason、phase、observed、limit；reason 区分 work、states、queue、geometry、deadline。外层 error.kind 保持 BudgetExceeded，库原 Error 枚举兼容。取消仍返回 Cancelled。

`work` 包括吸附、过期堆弹出、基础边/捷径枚举和解包工作；scanned_edges/relaxed_shortcuts 仅统计在线搜索枚举量。双向搜索的 states 和 queue_peak 分别合计正反两侧，解包继续记录其工作区峰值；不是进程全部分配字节。选中前驱链拼接后释放两侧搜索字典和队列，再恢复几何。工作区估计与 RSS 不能混用。forward_settled/reverse_settled 区分搜索方向，clique_skips 记录跳过的同单元 clique，compressed_edges 记录链跳跃省去的中间路段数。

默认查询预算维持 work=2,000,000、states=250,000、queue=500,000、geometry=100,000、CLI timeout=10,000 ms，库不设默认时限。通过缩小搜索空间修复长距离失败，而不是自动无上限重试。达到限制仍明确失败。

单次 CLI 的 `load_ms` 包含加载、图验证、加速器校验及索引构建；`load_diagnostics` 分为 dataset_ms、acceleration_ms、index_ms，另含 weak_components、strong_components。查询 elapsed_ms 不含这些启动步骤。JSON 编码、stdout/网络、消费者队列与进程启动耗时也不计入查询时限；外层应自行设置覆盖完整生命周期的时限。若需要比较查询本身，优先使用 serve 或同一个 Engine，不能将 CLI 总墙钟时间当作搜索时间。

## 加速文件 v2

`routing.mld` 是小端编码：8 字节 `RWFTML02`，64 字节 ASCII dataset_id，u32 限制状态节点数、fanout、层数，逐节点 u32 叶单元 ID；每层含 u32 单元数/u32 捷径数、逐单元 u8 可用标记、节点数+1 个 u32 CSR offsets、逐捷径 u32 目标/u64 耗时。末尾 32 字节 SHA-256 覆盖之前全部字节。限制状态图、边界标记、反向 CSR、弱分量和压缩链在加载时根据权威基础图重建；根状态按物理节点 ID 排列，前缀状态按确定性自动机遍历排列。加速器信息新增 restriction_states、restriction_transitions、boundary_nodes 和 partition。

绑定 dataset_id 同时绑定图摘要、权重和限制。读取前检查文件类型和预算，逐数组验证计数、offset 单调性、端点、单元归属和层级，再校验摘要。没有来源签名或任意外部手工构造度量的正确性认证；与基础图一样，仅使用可信构建流程产物。当前不支持直接编辑捷径成本；应重新定制。

现有 verify/inspect 的 verified 字段仍只覆盖 manifest 和基础 graph；它们不读取可选加速文件。加速器完整性、绑定和结构由 Engine 打开时检查，可以用 nearest 或 serve 的 ready 握手做部署检查。

## 固定 OD 差分基准

```sh
python3 scripts/benchmark-routing.py --binary backend/target/release/routeweft \
  --dataset region/ --pairs pairs.json --output benchmark-results/ \
  --graph-capacity regional --max-load-bytes 8589934592 --repeat 3
```

pairs.json 是坐标数组的数组，支持 2–10 个连续途经点。输出目录须不存在。脚本各启动一个常驻基线和加速进程，记录 ready 加载指标、每条查询和二进制/地图身份。Dijkstra 基线提高资源预算以尽量完成对照，auto 保持默认预算。比较最短成本和腿数；任何加速失败、可达性差异、成本差异或未完成的 oracle 对照都会使脚本失败并保留证据。P50/P95/P99 同时包含成功和失败的查询耗时，另列错误，不能只凭快返回的失败宣称性能改善。它是显式基准，不自动下载地图，也不加入普通离线测试。
