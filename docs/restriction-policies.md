# 限制关系政策与省级验收

默认 `reject` 拒绝无法完整解析的静态 driving 限制。`block_ways` 保留原有要求：异常关系必须具有可界定且可用的 from/to。新增 `quarantine_ways` 是显式、非等价的保守降级：仅对批准的异常类别，依据完整原始 relation 排除全部现存 way 成员，不推断空 role。

未知/嵌套 relation 依赖、无可封道路、未批准规则、成员/封路/轮次/审计预算超限仍拒绝。`no_right_turn_on_red` 仅在新政策下隔离全部成员道路，不能解释为全天 no_right_turn。产物通过 `quarantine_ways_policy_v1` 与实际排除清单标识降级；发布校验核对政策、源 SHA、诊断及排除计数。不能将 `planned` 当成 `applied`。

| 语义或结构 | driving 行为 |
| --- | --- |
| 静态 no/only left/right/straight/u-turn | node-via、有序 way-via，受限路径编译为限制状态机 |
| no_entry 多 from / no_exit 多 to | 支持合法组合；其他多 from/to 不作任意展开 |
| 相同前缀的多个 only | 先合并允许出口；出口因隔离消失后重新求值 |
| 车型标签 / except | 按既有 driving 优先级选择，非适用关系不编译 |
| 条件道路、适用的条件转向 | 保守封路，不计算时间条件 |
| 空 role、缺 from/to/via、不可达或缺失引用 | reject 拒绝；新政策在依赖可界定时隔离全部现存 way |
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
