# 地图自有样本与常驻导航验收

本能力对应 RW-NAV-002 / RW-NAV-003。地图构建宿主可直接调用已交付命令，独立完成会话适配、回执冻结与打包流程；本仓库不依赖 map-build 或 Ops 先实现接入。

## 从当前地图生成跨边样本

```sh
routeweft sample-routes --dataset staging/ --graph-capacity regional \
  --max-load-bytes 12884901888 --count 12 --waypoints 3 \
  --distance-bands-m 1000,10000 --max-path-edges 1024 \
  --max-attempts 10000 --max-work 2000000 --timeout-ms 30000 \
  --max-sample-workspace-bytes 16777216 --json > samples.json
```

`version.capabilities.route_sampling` 声明 schema=1、sampler_version=1。库接口为 `Engine::sample_routes(SamplingOptions, SearchBudget)`，复用权威图和限制自动机，不另建全图 Python 拓扑。种子为 SHA-256(`routeweft-sampler-1:` + graph_sha256)，使用固定的整数 PRNG；同图、规则和足够预算下结果可复现。

距离层次按起终点的大圆距离划分，避免长绕圈冒充长距离 OD；上例为 [0,1000)、[1000,10000)、[10000,+∞)。样本数均分到各层，余数从短层分配，每层至少请求一个。`--distance-bands-m none` 明确选择单一距离层，适合小型测试图。

每个样本带 coordinates、witness_edges、witness_end_automaton、见证距离/整数时间和 OD 距离。见证包含至少两个不同有向边，起终点坐标不同；逐转移检查限制，途经点沿同一合法行走选取，不重置限制历史。它证明存在合法跨边行走，不宣称见证是最短路线；仍须对输出坐标执行真实路由和差分验证。弱连通不被当作有向可达的证明。

最多 1000 个样本、2–10 个坐标、2–4096 个见证边、8 个距离层。工作区在采样前按最坏结果容量准入；扫描、候选选择和尝试受 work、max_attempts、路径长度及协作期限限制。采样工作区包含本地结果和去重集合，不包含已加载引擎或 CLI 的 JSON/输出缓冲，宿主应分别预留。

成功返回 status=ok、sampling.coverage_complete=true。图太小、距离层不可覆盖、唯一坐标路径不足、达到 work 或 deadline 时保留部分结果、各层请求/实际数量及 shortfall_reasons，status=insufficient，CLI 退出 3。工作区不足返回 BudgetExceeded。覆盖不足不会用单边样本补数；验收脚本会拒绝这种样本集。

配置只保存规则和预算，坐标写入验收证据。事故坐标可以作为测试夹具，不加入地区生产配置。采样成功也不是全图、全限制或生产延迟 SLA 的证明。

## 显式发布验收

```sh
python3 scripts/benchmark-routing.py --binary backend/target/release/routeweft \
  --dataset staging/ --pairs samples.json --output acceptance-results/ \
  --graph-capacity regional --max-load-bytes 12884901888 \
  --mode acceptance --require-mld --require-reachable --warmup 1 --repeat 3 \
  --max-load-ms 15000 --max-ready-ms 20000 --max-p95-ms 200 --max-p99-ms 500 \
  --max-client-p95-ms 1000 --max-work 2000000 --max-states 250000
```

数值是宿主策略示例，应按目标部署选择，不是本项目宣告的省级 SLA。输入支持原有 2–10 坐标数组，也支持完整 sample-routes 响应。生成的样本集必须 coverage_complete，并匹配当前 dataset_id、graph_sha256 和 profile。输出目录必须不存在，避免覆盖旧证据。

默认 mode=differential 保留无 MLD 的合法回退。mode=acceptance 至少配置一项明确门槛；未要求 MLD 的策略不会被解释成 MLD 认证。只有 mode=acceptance 且全部检查通过时 summary.acceptance_passed=true。`accepted` 表示通过当前显式策略，必须连同 mode/policy 一起消费。

- `--require-mld` 同时要求 ready 存在 MLD、加载摘要匹配当前文件、每条 auto 查询（含预热和错误响应）的实际算法为 mld。
- `--require-reachable` 要求每条预热和测量路线成功；普通差分模式允许两侧一致的合法 NoRoute。
- `--max-load-ms` 约束 auto 引擎内部加载耗时；`--max-ready-ms` 约束启动到 ready 的客户端墙钟。两者分开保存。
- 热查询 P95/P99 基于 diagnostics.elapsed_ms，仅计测量轮次；预热单独保存并排除。失败样本仍计分位数和错误数量。
- `--max-client-p95-ms` 使用发送单条请求至读取完整响应的客户端墙钟，含编码/协议传输；不冒充纯引擎时间。
- `--max-work` / `--max-states` 是 auto 实际观测值的验收上限，**不提高**在线查询预算。Dijkstra 参考进程保持明确记录的放宽预算，以尽量完成 oracle。

参考算法必须实际为 dijkstra；每个测量请求均需完成成功或合法 NoRoute 对照。成本、腿数或可达性不一致、参考预算耗尽、缺少诊断、请求 ID 不匹配、协议异常或门槛超标均非零退出。预热中的非 NoRoute 错误也失败。合法 NoRoute 不会被错误当作引擎故障，但可被 require-reachable 策略拒绝。

## 凭据和进程边界

identity.json 保存基准脚本与二进制 SHA-256、version/build、manifest、完整 MLD 摘要、原样本集和规范化坐标集摘要。每个算法的 JSONL 保留 ready 与原始响应；JSON 回执保存命令、ready、有效查询预算、预热/测量结果、逐请求客户端时间和 stderr。summary schema=2 包含策略、nearest-rank 统计方法、算法、分位数、工作量和失败原因。结束时再次核对工具、样本集、manifest 和 MLD 身份。

进程交互按一问一答复用同一引擎；ready/响应墙钟期限、16 MiB 单行、每进程输出额度（默认 128 MiB）和 1 MiB stderr 上限防止协议卡死或无界捕获。失败会终止并回收子进程，保留已有证据；不会在失败后静默重试。会话正常结束发送 EOF 并检查退出状态及额外输出。

serve 的 ready 增加 graph_sha256、profile、acceleration_sha256 和 effective_query_limits。旧包络和逐查询格式保持兼容。宿主实现独立会话适配时，资源许可必须覆盖进程整个存活期，不能只覆盖启动命令。

按需存储、runtime 字节准入与实际引擎 PID 内存验收参见[按需查询与内存所有权](paged-routing.md)。
