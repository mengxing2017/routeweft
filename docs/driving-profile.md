# driving-v1 支持矩阵

这是保守的静态汽车子集，不覆盖所有地区车辆法规。代价是预计行驶时间，没有路况、路口等待、转向或掉头惩罚，也没有道路偏好 routability；因此路线应按本项目的成本模型和支持范围评估。

| 规则 | 实现与限制 |
| --- | --- |
| 道路 | motorway/trunk/primary/secondary/tertiary 及 link、unclassified/residential/living_street/service；其他类别省略并统计 |
| 默认速度 km/h | motorway 110、trunk 90、primary 70、secondary 60、tertiary 50、unclassified 40、residential 30、living_street 10、service 20；各级 link 为 50/45/40/35/30 |
| maxspeed | 正数 km/h、km/h 后缀、mph；forward/backward；上限取默认速度与声明限速较低值。未知文本/条件速度不猜测 |
| access | motorcar→motor_vehicle→vehicle→access，方向标签优先于同层通用标签；yes/permissive/designated 允许，其他显式值保守拒绝 |
| 单行 | yes/1/true、-1、no/0/false；roundabout 和 motorway 默认正向单行；未知值拒绝，reversible/alternating 排除 |
| 条件规则 | 带 :conditional 的道路/节点保守阻断；适用于 driving 且未被 except 排除的条件转向 relation 涉及的 way 排除并计数，不解释时段；纯其他模式条件不阻断汽车道路 |
| 障碍物 | 未明确可通行的障碍物节点禁掉相邻路段，属于保守近似，可能影响到达性；不是可停靠端点特判 |
| 物理尺寸 | 带 maxheight/maxwidth/maxweight/maxlength/maxaxleload 的道路保守排除，尚无车辆参数比较 |
| 特殊道路 | area=yes、service=emergency_access 排除；ferry、track、construction 等非支持道路不建图 |
| 静态转向 | no/only left/right/straight/u-turn、no_entry/no_exit；单 via-node 或有序 via-way；no_entry 允许多 from、no_exit 允许多 to，其余单 from/to；支持机动车模式标签和 except |
| only 多规则 | 相同边前缀允许出口先合并；via-way 每个前缀均受限，不能提早离开 |
| 不可解析限制 | 默认严格拒绝；可显式 block_ways 保守排除受影响道路，迭代校验并输出关系级审计；不忽略后放行 |
| 拓扑 | 同一 OSM node 连接，几何交叉不创造连通；缺失引用拒绝构建 |
| 端点与途经点 | 最多 10 点，边中点按方向拆分；不在边中途掉头，途经点不重置限制状态；节点处未禁止掉头可行 |
| Nearest | 局部平面投影 + 球面距离，半径 0～10 km，方向候选默认 8、最多 16；不使用高度/heading |
| 指引 | 出发/到达、基本转向、掉头、环岛进出及出口编号、基于 link 类别的简单 merge/exit；相同 way 的独占形状点不当作转向 |

环岛出口按经过节点是否存在可离开环岛的方向边计数，是基本结构语义；不提供复杂多车道环岛或完整的导航步骤归并。名称相同的直行段合并，所有步骤附几何索引和该段成本。复杂道路层叠和同位候选可能有歧义，真实导航应结合支持矩阵评估，不默认具备定位闭环。

PBF 读取支持 raw/zlib 和普通/dense 节点，未知 required_features 或压缩拒绝。文件及累计解压各≤512 MiB，单 blob≤32 MiB，blob header≤64 KiB，节点/way/relation 总对象数≤2,000,000。另有 512 MiB 对象内容估算预算（非精确 RSS，单对象临时解析与容器开销另计）。这是有上限的全内存构建器；不适合全国级输入。无约束的原始对象保留仍可能产生明显的内存放大，部署应设置进程 RSS 配额。


## 限制形态及诊断

- 单 via-node：枚举 from 入边与 to 出边；同一 way 的 u-turn 区分反向与直行。
- 有序 via-way：按成员顺序解析连续方向路径；only 约束每个前缀，多个相同前缀的允许出口先合并。
- 多 from 的 no_entry、多 to 的 no_exit：展开所有保留的 from/to 组合，每条禁止路径均进入限制自动机；任一应适用组合无法解析时不丢弃该组合。
- 缺 from/to 为 MissingFromTo，缺 via 为 MissingVia，引用未在输入中出现为 MissingReference；不能自动补造 via。
- from/to 非 way 为 InvalidMemberType；不在上述范围内的多 from/to 为 UnsupportedMultiplicity；多个 via-node、混合 node/way、relation via、相邻重复 via/to way 为 UnsupportedViaShape。
- 未支持的 restriction 值为 UnsupportedRestriction；成员完整但保留图中无对应方向路径，或 only 的允许出口被 profile 排除，为 UnresolvedRestriction。源数据无效、切片缺引用和模型不支持需分别排查。
- 路径搜索最多 100,000 个搜索工作单位、10,000 条展开路径、每条最多 256 条边；via-way 数最多 64，from×to 最多 256；超限为 RestrictionLimitExceeded，不输出部分限制。

无关模式或完全不涉及保留道路的关系不进入静态限制解析；non_driving_restrictions 不证明这些源关系本身合法。block_ways 只针对会影响当前保留图的静态限制失败，不是任意输入错误的容错开关。详细 JSON 与策略见 [CLI](cli.md)。
