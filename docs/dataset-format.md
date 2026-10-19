# Dataset v1 格式与完整性

目录仅使用 `manifest.json` 和 `graph.json`。两者为 UTF-8 JSON，无二进制端序；整数采用 JSON 十进制表示。不要用 JavaScript Number 无损处理所有 u64 代价。读取器拒绝未知 schema 字段，暂不支持向前兼容拓展。

manifest：format=1、dataset_id、builder 版本、profile=driving-v1、输入 SHA-256、可选 replication 秒级快照、OSM attribution、受控 graph_file、图字节数/SHA-256、节点/方向边/禁止序列计数及构建诊断。核心身份为以下四个字段的固定 UTF-8 拼接 SHA-256：

```text
routeweft-v1\n{source_sha256}\n{profile}\n{builder}\n{graph_sha256}
```

`graph.json` 包含 nodes（WGS84 lon/lat）、edges（from/to、OSM way ID、距离毫米/耗时毫秒、名称/道路类别/roundabout）及 forbidden（方向边 ID 序列）。节点/边 ID 为数组下标，非负且受数量上限约束；OSM ID 为 i64，代价为 u64。禁止序列至少两边、最多 256 边，引用必须连续且去重。边耗时/距离必须正数；小于毫米的零长度段在预处理时计数省略。

构建不写入时钟，输入及 profile 相同、构建器版本相同则核心图确定；地图数据或 profile 实现变化必须重建并调整相应版本。输入在读取前后各哈希，发现变化拒绝发布。OSM 元数据和未纳入图的对象不作为图字段持久化。

默认加载上限：图文件 256 MiB，manifest 1 MiB，节点 1,000,000，方向边 2,000,000，限制序列总边数 1,000,000。JSON 数组反序列化另有固定数量上限，不先按未可信条目数整体 reserve；调用者可通过 LoadOptions 收紧读取和验证上限。解析内存仍高于文件大小，不提供等于文件大小的 RSS 保证。

每个坐标合法且纬度绝对值≤85°；当前查询还要求经/纬跨度各≤5°。不兼容版本拒绝打开；未知图路径、哈希/字节数/计数不符、非法引用或直接符号链接均拒绝。校验检测损坏，不认证来源：有权整体重写 manifest 和 payload 的人可以生成另一份完整有效数据。

写入先使用同父目录唯一临时目录，创建文件、sync、重新打开校验，再 rename 并同步父目录；不覆盖已有输出，不替换有效的非空数据集。宿主必须使用受控发布父目录，避免其他进程并发篡改文件或符号链接；本实现不是对抗拥有目录写权限者的文件系统沙箱。查询不会执行数据内脚本或下载内容。
