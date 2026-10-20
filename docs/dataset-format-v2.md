# Dataset v2 二进制格式

使用 `routeweft build --input city.osm.pbf --profile driving --format 2 --output dataset/` 显式生成。默认构建仍为 [v1 JSON](dataset-format.md)。目录包含 `manifest.json` 与 `graph.bin`，部署须完整复制目录，并在目标环境执行 `verify`。

manifest 字段沿用 v1，`format=2`、`graph_file=graph.bin`，profile 仍为 `driving-v1`。数据集身份为以下 UTF-8 字节的 SHA-256，不含末尾换行：

```text
routeweft-v2\n{source_sha256}\n{profile}\n{builder}\n{graph_sha256}
```

v2 坐标保留构图时的原始浮点位；v1 JSON 读取可能有十进制解析带来的末位舍入差异。跨格式比较查询浮点坐标时应按数值精度比较，不能要求 JSON 字节完全一致；整数边代价和引用不做量化。

同图的 v1/v2 因编码和身份前缀不同具有不同 dataset_id；不能替换 graph_file 或 format 复用旧身份。读取器拒绝未知版本、未知标志、任意 payload 路径、未知 manifest 字段、损坏、计数不一致和非法拓扑。元信息模式仅检查 manifest，不校验 payload。

## graph.bin 布局

整数及 IEEE-754 binary64 均为小端序，无平台填充或原生 usize。所有区段紧密连续，禁止尾随字节。

64 字节固定头：

| 偏移（字节） | 类型 | 含义 |
| ---: | --- | --- |
| 0 | 8 字节 | ASCII `RWFTG002` |
| 8 | u64 | 标志，必须为 0 |
| 16 | u64 | 节点数 N |
| 24 | u64 | 方向边数 E |
| 32 | u64 | 禁止序列数 R |
| 40 | u64 | 字符串数 S |
| 48 | u64 | 禁止序列总边引用数 T |
| 56 | u64 | 字典区段总字节数 D |

头部之后依次为：

1. **字典 D 字节**：S 条 `u32 字节长度 + UTF-8 字节` 记录，无结尾零字节。字符串按 UTF-8 字节严格递增、去重；空字符串允许，每项至多 4096 字节。字典 ID 为从零开始的记录序号，S 不得超过 2E。D 包含长度字段，须满足 `4S ≤ D ≤ 4100S`。
2. **节点 16N 字节**：每条 `f64 longitude + f64 latitude`，WGS84 经度在前。保留浮点位模式，不做定点量化；加载时校验有限、合法坐标及纬度绝对值≤85°。当前 Engine 仍要求经纬跨度各≤5°。
3. **方向边 56E 字节**：每条依次为 `u64 from, u64 to, i64 way, u64 distance_mm, u64 duration_ms, u32 name_id, u32 class_id, u64 flags`。flags 仅 bit 0 表示 roundabout，其他位必须为零。from/to 为节点下标，name/class 引用字典；名称至多 4096 字节、类别至多 64 字节；距离及耗时均为正数。
4. **禁止序列 4R+8T 字节**：每条 `u32 长度 + 长度个 u64 方向边下标`，长度 2～256，各边引用有效且首尾连续，禁止重复序列。

总长度必须精确等于 `64 + D + 16N + 56E + 4R + 8T`，并与 manifest.graph_bytes 一致。读取器检查计数和算术溢出后逐条解码，不直接按未验证头部数量整体 reserve。

## 使用边界

`verify`、默认 `inspect`、`Engine::open`、`nearest` 和 `route` 支持两种格式。完整读取同时校验 payload SHA-256、长度、manifest 身份、计数及 Graph 拓扑；只重算哈希不能绕过语义校验。发布沿用同父目录唯一 staging、完整图预校验、流式写入、sync、落盘摘要/长度复核、manifest 复核和不覆盖发布规则。

v2 是存储编码，不代表外存构建或按需图访问。当前仍在内存构图，加载时将字典字符串还原为 Graph 的每边字符串，并创建运行时索引；完整图和查询索引均驻内存。字典解码还限制所有方向边展开后的 name/class UTF-8 字节总量，不超过 `min(LoadOptions.max_bytes, 256 MiB)`；超限返回 graph_string_bytes 资源错误，写入时执行同一检查，避免生成当前加载器不能接受的数据集。类别长度在复制字符串前检查。该预算不是进程 RSS 硬限制。

默认 256 MiB payload、100 万节点、200 万方向边和 100 万限制边引用上限不变。调用者可收紧 LoadOptions，二进制解码的节点/边/限制引用实现上限不能通过放宽选项绕过。

空间/邻接索引没有持久化，当前格式没有缓存恢复或来源认证机制。未来布局变化需新的格式版本或明确协商，当前读取器不会忽略保留标志。消费者使用前应检查 `version.dataset_formats`；只支持 v1 的旧工具应继续接收默认 v1 数据集。
