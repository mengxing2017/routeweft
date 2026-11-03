# 导航资源规划契约

`version.capabilities.resource_plan` 现在声明命令 schema=2、estimator_version=2，支持 MLD 定制、已有 MLD 加载和定制计划绑定。CLI 包络仍为 schema 1，基础图及 MLD 文件格式不变。宿主可以直接按此能力实现接入，无需等待其他消费端完成升级。

```sh
routeweft resource-plan --dataset staging/ --graph-capacity regional \
  --max-plan-workspace-bytes 67108864 \
  --leaf-nodes 256 --fanout 8 --levels 3 --max-boundary 128 \
  --max-shortcuts 20000000 --max-index-bytes 4294967296 --json > plan.json
```

规划不构造 Graph、限制自动机、CSR、R-tree 或查询引擎，不受拟执行的 `max-load-bytes` 驻留预算拒绝。V2 验证基础图后扫描出度和限制引用，计算限制状态及转移的保守上界；不为每个样本或每个前缀复制整张图。规划工作区默认 64 MiB，验证、出度扫描和 MLD 检查依次复用预算，分配前检查。已有 MLD 的检查暂存叶单元、offsets 和当前层可用标记，不持有捷径数组。

| 字段 | 语义 |
| --- | --- |
| schema / estimator_version | 完整导航计划均为 2；计费公式或含义改变时升级 |
| base_estimator_version | 基础 dataset/engine 估算器仍为 1，保留已有基础预算契约 |
| dataset_id / graph_sha256 / source_sha256 / format / profile | 已校验的基础图绑定；MLD 变化不改变 dataset_id |
| effective_load_options | 拟执行的容量和加载限额；不是自动提高预算的授权 |
| dataset_load / engine_load | **仅基础图**阶段的既有估算，不含 MLD；勿用它代替下方完整峰值 |
| topology_upper_bound | 物理节点/边、限制引用、状态及转移数量的上界 |
| customization | 本次拟执行的 leaf_nodes、fanout、levels、max_boundary、max_shortcuts、max_work、max_bytes |
| customization_timeout_ms | CLI 记录的拟定制超时，默认 1800000；不是规划本身的硬中断期限 |
| acceleration_present / acceleration | 已有文件存在状态；不存在时为 false/null |
| acceleration.sha256 / format / bytes | 完整文件（包括尾部校验和）的 SHA-256、RWFTML02 和实际字节数 |
| acceleration.states / fanout / cells / shortcuts | 从当前文件实际读取的结构；与拟重新定制参数分开 |
| acceleration.verification_scope | checksum_layout_base_binding_not_topology_or_metric |
| customize.peak_bytes | 基础引擎计费加上定制阶段逻辑峰值上界 |
| customize.accelerator | topology、正反向索引、分区和搜索工作区、定制/加载峰值及输出字节上界 |
| accelerated_load | 已有 MLD 的加载计划；文件不存在时为 null，不假造零成本 |
| accelerated_load.peak_bytes / resident_bytes | 含基础图的完整加载峰值和常驻逻辑需求 |
| engine_resident_bytes | 当前目录实际存在状态对应的基础图或含 MLD 常驻需求 |
| included / excluded_unknown | 显式列出覆盖与未知项；所有字节均非 RSS 硬上限 |

定制上界根据状态数量、层级、单元边界限制及最大捷径数计算。分区和逐源最短路工作区取较大值，因其生命周期不重叠；同时保留的拓扑、正反向 CSR、向量容量和读写缓冲另计。优先队列包括重复松弛入队的容量，不能只按节点数计算。上界可能高于实际需求，尤其是大 `max-shortcuts` 或大量共享限制前缀时。计划独立于执行限额返回需求，不保证当前 `max-index-bytes` 足以容纳上界。

自动准入可预留 `customize.peak_bytes`，同时确保 `max-index-bytes >= customize.accelerator.customization_peak_bytes`、`max-load-bytes >= customize.peak_bytes`。若调整定制参数或索引预算，重新生成计划。定制后再运行 resource-plan，按 **accelerated_load.peak_bytes** 准入实际引擎加载。不同阶段的完整峰值不用相加；并发任务或新旧引擎重叠驻留必须由宿主单独累计。

## 计划绑定与定制回执

```sh
routeweft customize --dataset staging/ --graph-capacity regional \
  --max-load-bytes 12884901888 --max-index-bytes 4294967296 \
  --leaf-nodes 256 --fanout 8 --levels 3 --max-boundary 128 \
  --max-shortcuts 20000000 --resource-plan plan.json --json > customization.json
```

`--resource-plan` 接受完整命令响应或内部 resource_plan 对象，最大 1 MiB。执行前重新校验并比较计划版本、基础图、profile、定制参数、资源需求、已有 MLD 身份及超时；不一致返回 InvalidInput，开始定制前即失败。允许宿主按需求设置执行驻留预算，计费公式不会随 `max-load-bytes` 改变。实际执行仍进行预算检查，不靠回执绕过限制。无计划参数的原有 customize 调用保留兼容行为。

成功回执包含 dataset_id、graph_sha256、profile、customization、超时、tool_build、acceleration_format 和完整 acceleration_sha256。RWFTML02 本身只保存分区/指标结构，无法恢复所有原始定制参数；`acceleration.customization_provenance` 明确标记这一点。应将定制回执与不可变产物一起冻结，不能从文件猜测原始 leaf_nodes 或工作限额。

## 校验范围与兼容性

V2 基础图使用完整流式语义校验；V1 保留大小、身份、哈希校验及旧 visitor 容量估算，verification_scope=identity_size_hash_v1_not_semantic。V1 拓扑上界根据成功加载时必须匹配的 manifest 数量和限制引用限额计算，通常比 V2 的出度扫描更保守。它不等价于 V1 verify 成功。两种格式均需足够的规划工作区。

MLD 检查覆盖格式、类型、图绑定、数组计数、offset 单调性、单元归属、端点、正成本和完整摘要；不重建限制状态拓扑，不验证所有边界语义，也不证明捷径最优性。因此计划成功仍需 Engine/serve 加载及真实差分验收。损坏、旧格式、错图文件明确失败，不忽略现有加速文件。输入在规划、绑定检查和执行间须保持不可变。

原 `routeweft_dataset::resource_plan` / `resource_plan_from_manifest` 和 build 内嵌结果继续提供 **schema 1、estimator 1 的基础图计划**。能力声明明确给出 build_output_schema=1、build_output_scope=base_graph_only；基础计划保留 optional_routing_mld 未知项。完整命令和库调用方应改用 `routeweft_routing::resources::resource_plan`，不能把 build 基础计划当作 MLD 准入回执。基础预算错误的 estimator_version 仍为 1；MLD 预算不足返回 BudgetExceeded。

计费共享于实际拓扑构造、定制、MLD 解码和反向 CSR 构建。计费使用 checked arithmetic，溢出拒绝。未知项仍包括在线查询/coverage 工作区、审计和输出缓冲、进程与 OS 开销，以及 PBF 构建和 scratch 峰值；它们不是零。宿主负责物理内存预留、并发准入和容器上限，并分别记录逻辑估算与实际 RSS。
