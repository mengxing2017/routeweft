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


## 构建诊断计数

manifest.diagnostics 为名称到整数的可扩展计数表，新增计数键不改变 format=1。图及 manifest 的必需结构没有变化，旧消费者仍需按原格式校验；未知 format/profile 不能因 CLI schema 相同而接受。

| 计数 | 粒度与含义 |
| --- | --- |
| input_nodes/input_ways/input_relations | 解码后的输入对象数，不等于保留图数量 |
| unsupported_way_class | 不在 driving 类别表内的 way 数，含无 highway 标签的 way |
| blocked_nodes | 通行/条件/障碍物规则保守阻断的 node 数，一节点计一次 |
| access_or_conditional_ways_blocked | 兼容保留的汇总 way 数；下列细分原因按首次匹配互斥计数 |
| blocked_way_access/area/emergency_access/dimensions/conditional_restriction/conditional_tag | 对应访问、面、应急、物理尺寸、条件限制、道路条件标签的 way 数 |
| reversible_ways_blocked | 可变方向规则排除的 way 数 |
| zero_length_segments_omitted | 舍入为零长度的相邻节点段数，不是 way 数 |
| conditional_restrictions_blocked | 适用于当前汽车模式的条件 relation 数，不是删除的 way 数 |
| excepted_restrictions/other_mode_restrictions/non_driving_restrictions | 分别为 except 排除、其他模式、当前图无适用路径的 relation 数；不代表完整源数据校验 |
| resolved_restrictions | 成功解析的静态 relation 数；多入口/出口展开后仍计一条 relation |
| invalid_restrictions_blocked | 显式保守模式累计阻断的无效 relation 数 |
| invalid_restriction_ways_blocked | 显式保守模式累计排除的不同源 way ID 数，含源中存在但本就不参与 driving 的成员 |

只出现实际发生的可选计数键；未出现按零处理。input_* 与显式保守阻断计数固定输出。细分键在原汇总前添加 blocked_way_ 前缀，例如 blocked_way_dimensions。最终 report/manifest 的常规排除计数基于最终构图尝试，保守阻断计数累计整个迭代；二者不是同一个分母，不能直接相加当作全部输入。

关系级 ID、标签、成员、失败原因和动作通过 build/preflight 的 report 输出，须由构建宿主随回执保存。示例验证脚本的 build-report.json 是附加审计材料，不是引擎加载必需文件，manifest 的图哈希也不认证该附加文件。区域告警阈值由真实输入审核制定，不统一要求零告警。
