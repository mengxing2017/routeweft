# 限制关系政策与省级验收

默认 `quarantine_ways` 隔离依赖可界定的异常限制涉及的现存道路，继续转换其他路网，并在成功报告中保存告警与实际排除清单。显式 `reject` 提供严格拒绝模式。`block_ways` 保留原有要求：异常关系必须具有可界定且可用的 from/to。`quarantine_ways` 是允许覆盖损失的保守降级：仅对批准的异常类别，依据完整原始 relation 排除全部现存 way 成员，不推断空 role。

未知/嵌套 relation 依赖、无可封道路、未批准规则、成员/封路/轮次/审计预算超限仍拒绝。`no_right_turn_on_red` 仅在新政策下隔离全部成员道路，不能解释为全天 no_right_turn。产物通过 `quarantine_ways_policy_v1` 与实际排除清单标识降级；发布校验核对政策、源 SHA、诊断及排除计数。不能将 `planned` 当成 `applied`。

| 语义或结构 | driving 行为 |
| --- | --- |
| 静态 no/only left/right/straight/u-turn | node-via、有序 way-via，受限路径编译为限制状态机 |
| no_entry 多 from / no_exit 多 to | 支持合法组合；其他多 from/to 不作任意展开 |
| 相同前缀的多个 only | 先合并允许出口；出口因隔离消失后重新求值 |
| 车型标签 / except | 按既有 driving 优先级选择，非适用关系不编译 |
| 条件道路、适用的条件转向 | 保守封路，不计算时间条件 |
| 空 role、缺 from/to/via、不可达或缺失引用 | 显式 reject 拒绝；默认在依赖可界定时隔离全部现存 way |
| 嵌套关系、未知限制值、解析预算超限 | 拒绝，不输出部分限制 |

隔离集合单调增长，当前每轮完整重建拓扑直至固定点，复用不可变源暂存；尚未实现仅重算受影响关系的工作队列。节点引用闭包按 `(node,way,ordinal)` 外排后顺序连接，源引用完整性与 retained graph 分开核验。

## 操作

```sh
routeweft audit-restrictions --input region.osm.pbf --output inventory \
  --max-memory-bytes 268435456 --max-scratch-bytes 17179869184 --json
routeweft build --input region.osm.pbf --profile driving --builder external \
  --format 2 --graph-capacity regional --output dataset --scratch-dir scratch \
  --max-scratch-bytes 17179869184 --invalid-maxspeed use_default \
  --invalid-restrictions quarantine_ways --max-blockways-attempts 8
routeweft verify --dataset dataset --graph-capacity regional --json
routeweft customize --dataset dataset --graph-capacity regional \
  --max-load-bytes 12884901888 --max-index-bytes 4294967296 --json
routeweft coverage --dataset dataset --graph-capacity regional \
  --max-load-bytes 12884901888 --excluded-ways excluded-way-ids.json --json
routeweft serve --dataset dataset --graph-capacity regional --max-load-bytes 12884901888
```

上面的显式内存额度是广东验证配置，不是默认值。加载预算与索引定制预算独立，默认均为 2 GiB；定制完成也必须确认常驻服务 ready 包络的 acceleration 非空。服务应复用驻留 Engine，避免每次请求重新加载省级图。查询预算不随加载额度增加。

`audit-restrictions` 扫描全部源 restriction，导出 JSONL、摘要和六个重点关系的依赖 fixture。fixture 是 driving 投影对象 JSON，不是保留全部 OSM 标签的原始 XML。缺源对象、车型过滤、无有效方向边及 retained 路径失败分别记录，预算截断不能标为 complete。

`coverage` 返回强弱连通分量、最大分量节点数、有向长度及隔离 way 的残留边数。最终路径解包后重放原始边连通性、限制状态和成本；`validated_route_edges` 记录该验证。静态限制合法性不代表已支持信号灯或条件语义。

`scripts/prepare-routing-extract.py` 要求同快照父源 SHA、边界和显式 osmium 可执行文件；使用 smart restriction 闭包并检查引用，保存派生 manifest。没有父源时不能从不完整子集恢复缺失对象，更不能猜空 role。

`scripts/validate-regional-routing.py` 保留每条成功与失败查询，以实际道路距离分组，使用默认生产查询预算；精确 Dijkstra 样本单独设置对照预算。完整验收还需要源数据溯源、覆盖差异、宿主安装 smoke、断路/单行/via-way/途经点用例及同输入外部引擎对比，不能只看构建成功。

## 5652903 失败与依赖边界契约（2026-10-30）

宿主显式选择 `block_ways` 时，空 from/to role 继续返回 `NoSafeFromToMember`。RouteWeft CLI/库默认现为 `quarantine_ways`。宿主若仍显式传入 `block_ways`，需修改为 `quarantine_ways` 或省略该参数才能采用默认容错策略；程序不暗中覆盖显式策略。`blockways_failure.recoverable=false` 表示当前策略不能继续，不表示源修订或另行审核后的显式隔离路径也无效。消费者读取结构化 `error.details`，不得解析 message 自动切换政策。

| 情况 | 结构化处置 |
| --- | --- |
| `block_ways` 缺安全 from/to role | `NoSafeFromToMember` |
| `block_ways` 有 from/to role，但这些 way 都不存在 | `ReferencedWayAbsent`，保留计划/实际集合的区别 |
| 有嵌套 relation 或未知类型成员 | 两种封路政策均为 `UnboundedMemberDependency`；无可见 way 或带条件标签也不能据此判无关 |
| 已进入隔离判定但无现存 way | `NoPresentWayMember`；不伪造可隔离集合 |
| 隔离类别不在批准范围 | `QuarantineCategoryNotApproved` |
| 没有新增可排除道路 | 协调器的 `NoNewExclusions` 防护，不无限重试 |
| 轮次/审计/成员引用预算耗尽 | 资源错误及已有 failure_code；不发布未完成图 |

对普通、仅含有界 node/way 且没有 retained way 的关系，仍可判为不适用于当前 driving 图，不要求为了生成 blocker 而隔离无关道路。嵌套或未知依赖不同：没有展开其依赖就不能证明不相关。本轮在 resolver 的相关性提前返回、内存路径的无邻接优化以及条件规则提前返回之前检查该边界；未实现递归解析嵌套限制。

失败诊断里的 `members` 最多投影 256 项；`members_truncated=true` 与 `restriction_audit.truncated=true` 含义不同。前者是单条成员证据投影，隔离决策仍读取完整原始关系；后者表示整体审计未完整保留，不能继续封路发布。blocker 的 members 同样是投影，不能从它猜测被截断成员。relation ID、attempt、reason、recoverable、source SHA 与计划/实际集合沿用既有字段，无新增同义字段或能力枚举。

成功隔离须核对 `planned_blocked_way_ids == applied_blocked_way_ids == blocked_way_ids`、无 blocker、完整审计、政策版本标记及 coverage 零残留，再进行 verify/route。合成 5652903 fixture 仅复现异常结构，不代表真实关系坐标/完整成员证据，更不能代替指定输入 SHA 的省级验收。
