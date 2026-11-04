# 按需查询与内存所有权

省级数据显式选择 `--storage paged`；默认 `resident` 保持兼容。编辑/构建使用 Graph；paged 直接在磁盘紧凑出边和 CSR 上运行带转向限制、途经点状态的精确搜索，不扩图、不构造局部 Graph/Engine/R-tree。路径确定后才解码名称、类别、几何和指引。两种模式共用路径恢复和指引实现。

## 离线索引与兼容性

```sh
routeweft prepare-query --dataset staging --graph-capacity regional --max-load-bytes 12884901888
routeweft verify-query --dataset staging --graph-capacity regional --max-load-bytes 134217728
routeweft resource-plan --dataset staging --graph-capacity regional --storage paged
routeweft serve --dataset staging --graph-capacity regional --storage paged \
  --max-load-bytes 134217728 --page-cache-bytes 4194304 \
  --max-plan-workspace-bytes 16777216 --query-workspace-bytes 67108864
```

基础图须为 v2。prepare-query 原子生成单文件 query.idx（RWFTQI02，索引 schema 2），不改变 graph.bin 或 dataset_id。除字典位置、CSR、空间 BVH 和弱连通分量外，v2 索引增加按 CSR 排列的 24 字节出边记录（原始边 ID、终点、整数成本）及按节点交错存储的 4 组 64 位 landmark potential。离线预算包括这些结构、无向 CSR、最短路队列和扩容重叠；与消费端预算分开。

landmark 来自无向放宽图，Auto 使用整数 ALT 下界的 A*。离线 verify-query 对每条原边验证 potential 的可行性 `abs(p(u)-p(v)) <= duration_ms`，以及不可达标记一致性，并绑定紧凑出边与原始记录。这保证下界不高估原图路径；单向、限制和途经点保留在实际搜索状态中。部分边终点取各候选可到达端点下界的最小值，虚拟分割点使用零下界；允许状态重新入队，不假设下界一致。无地理裁剪或行政界截断。

RWFTQI01/schema 1 仍可读，使用直接紧凑 Dijkstra，不具备 landmark 加速；`--algorithm dijkstra` 也可明确选择独立的无下界搜索。旧 reader 会拒绝 RWFTQI02，不能直接替换旧不可变 ready；应重新生成、完整语义校验并冻结新回执。`version.capabilities.paged_queries` 声明版本 2、读写索引格式、算法及页大小。paged 未加载 MLD，不冒称 MLD；resident 原有 MLD 保留。

先完整 verify-query，再冻结基础图、索引、profile、tool_build、参数和摘要，将完整索引 SHA-256 通过 `--query-index-sha256` 或 PagedResourceIdentity 固定。加载认证与布局检查不代替离线语义验证；只重算 checksum 不能证明索引正确。v1 基础图、缺失/损坏/错图索引均明确失败，无静默 resident 或网络回退。归档仍是扁平普通文件，不需要大量块文件。

## 查询预算与诊断

应用 LRU 页为 4 KiB，每页另收取 256 字节管理成本，总页数由配置字节上限决定。紧凑出边批次相邻，减少 CSR 与全局边数组往返；字典和节点数据主要在吸附与最终恢复阶段读取。OS 文件缓存不属于应用页缓存，cache-miss bytes 也不是闪存物理读取量。

搜索保存 ID、整数成本、父标签和分割区间。按 labels Vec、状态 HashMap、frontier BinaryHeap 的实际 capacity 收费，另收取限制自动机、控制空间及扩容时新旧缓冲重叠；不再按每条纳入边预收 10 KiB。恢复前释放搜索表和队列，再收费路径缓冲及输出构造。预算不足明确 BudgetExceeded，不能把截断搜索当成 NoRoute。

诊断 phases 列出 snap、prepare、search、path_recovery、recover 的时间及命中/miss/读取增量，包含失败前已发生的工作。paged_workspace 记录标签与 frontier 当前数量/容量、分项字节、规划扩容峰值与限额。decoded_path_edges 为最终路径解码次数；working_graph_edges 保留兼容，直接搜索始终为零，不再代表搜索规模。states/settled_states/scanned_edges 记录搜索工作量。page_cache 仍为引擎累计值，不能直接当作单次查询读取。

## Runtime 总预算

宿主设置 RuntimeConfig.memory_budget_bytes=Some(bytes) 启用共享逻辑字节准入；None 保持旧数量配额。Runtime 的 clone 共享池；多个 runtime 或不同 Loader 必须传入同一个 MemoryBudget（Runtime::with_memory_budget），共享池限额优先于各 config 字段。

移动端初始策略可用：进程路由总预算 128 MiB、单查询工作区 64 MiB、页缓存 4 MiB、规划工作区 16 MiB、单查询并发。使用 PagedEngineLoader 及 PagedResourceIdentity，同时将 RuntimeConfig.query_workspace_bytes 和 PagedOptions.query_workspace_bytes 都设为 64 MiB；默认查询工作区仍为兼容的 256 MiB，不会替宿主自动降低。128 MiB 是建议的宿主策略，不是库硬编码。

加载依次预留规划、加载峰值并降为驻留量；缓存、staging、查询及新旧活跃引擎共用池。超时/取消等待不会提前释放 blocking 任务配额；驱逐后外部 Arc 仍持有的引擎继续占用配额和文件租约。memory_usage() 暴露限额、使用/峰值与拒绝次数，错误含需求及可用量。同一 paged 引擎串行搜索，争用明确拒绝；无配额绕过。

paged 资源计划 schema 1、estimator 2，覆盖持久图/索引、缓存/稳定量、加载峰值和查询工作区。收费是逻辑上界，不是 RSS 硬限制。宿主保留结果、分配器/进程开销、内核文件缓存及其他业务需另行预算和实测。

## 认证、启动与前后台恢复

每次实际 open 仍完整流式校验基础图语义与摘要及索引摘要；不提供凭文件名、mtime 或未认证磁盘回执跳过验证的接口。load_diagnostics.paged 拆分 base_validation_and_hash_ms、index_header_ms、index_hash_ms，列出认证文件字节与认证模式；文件字节数不是实际多遍扫描量。

首次安装应离线完整 verify-query 并冻结文件。进程内前后台恢复可保留已验证的 Resident/PagedEngine Arc，经 runtime 命中复用，避免重新 open；进程被终止、新地图身份或显式重新 open 均重新认证。宿主须保持文件不可变和租约有效。这不等价于跨进程信任缓存，也不宣称可抵抗违反不可变契约的并发写入。

Linux release 的首次启动/重复打开与保留句柄恢复须分别记录；OS 缓存没有受控清理时只能称“缓存状态未控制”，不能冒称冷存储实测。Android/iOS 真机启动、PSS、后台恢复、闪存 I/O 与能耗仍需设备验收。

## 实际进程门禁

benchmark-routing.py --storage paged 对照独立 resident Dijkstra；--baseline-max-load-bytes 仅为隔离 oracle 提供预算。--max-query-read-mib 限制每个测量及预热请求的分阶段 miss 读取之和；缺少有效阶段数据则失败。不得靠预热隐藏首次请求读取放大。

--max-peak-rss-mib / --max-steady-rss-mib 使用实际引擎 PID 的 Linux /proc，核对 ready.pid。记录 VmRSS/VmHWM、RssAnon/File/Shmem、平台/内核、KiB 单位和时间线；缺失值为 null，内存验收失败。范围仅引擎单进程，不包含 Python 包装器或后代。峰值结合内核 VmHWM；steady 是测量请求后的 RSS，不是长期空闲统计。时间线最多 4096 条，溢出仍统计峰值。

低预算局部成功不等于所有省级长距离或手机交付达标；必须冻结多 OD、限制/via、绕行、不可达、连续重算和新旧地图切换集合，保留失败回执。正式发布、下游锁定和指定移动设备验收各自需要证据。
