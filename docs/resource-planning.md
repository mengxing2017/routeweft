# 资源规划契约（新增能力）

`version.capabilities.resource_plan` 声明 schema=1、estimator_version=1、命令名和 build_output。宿主必须检查此能力；旧工具不支持时应报告版本不兼容，不能回退固定区域内存值。

```sh
routeweft resource-plan --dataset dataset --graph-capacity regional \
  --max-plan-workspace-bytes 67108864 --json
```

响应为 CLI schema 1，`resource_plan` 内部 schema 1。规划不构造 Graph、CSR、R-tree、turn trie 或查询索引，不执行引擎加载，也不以 `max-load-bytes` 拒绝返回需求。可选传入该参数以记录宿主拟使用的执行上限；不传时仍记录工具默认上限。规划不自动提高它。实际 coverage/nearest/route/serve/customize 仍执行加载预算检查。

| 字段 | 含义 |
| --- | --- |
| dataset_id / graph_sha256 / source_sha256 / format | 本次验证的产物及源身份 |
| counts | 节点、边、限制序列数量及图字节数 |
| estimator_version | 逻辑估算公式版本；公式或计费含义变化须升级 |
| effective_load_options | 实际传入估算器的图容量、限制引用上限和逻辑加载预算 |
| dataset_load / engine_load | 分阶段 estimated_bytes 及 nodes/edges/restriction/strings/dictionary 字节组成 |
| estimate_kind | conservative_logical_allocations_not_rss |
| included | 图向量容量、字符串/字典、分配开销余量，以及 engine 阶段的 CSR、R-tree 构建、turn trie |
| excluded_unknown | 可选 routing.mld、查询工作区、coverage 工作区、审计/输出缓冲、进程和 OS 开销、构建与 scratch 峰值；这些是未知项，不是零 |
| verification_scope | full_streaming_v2、identity_size_hash_v1_not_semantic 或 build 输出的 published_manifest |
| validation_workspace_budget_bytes | 独立规划验证工作区预算；build 内嵌结果为 null（该结果不重新运行规划验证） |

V2 规划复用完整流式 verifier，校验 manifest 格式/容量/身份、图大小、结构、计数、引用及哈希，验证工作区默认 64 MiB，可显式调整。这是逻辑工作区额度，不是进程 RSS 上限。V1 校验 manifest、图文件大小及哈希，不完整解码语义，按旧 serde visitor 的最大容量保守估算，不相信较小的 manifest 计数。V1 规划结果不等于 verify 成功。输入目录在规划和执行期间必须保持不可变。

`estimate_resident(manifest, LoadOptions, indexes)` 为纯函数；规划、build 内嵌需求和 `check_resident_budget` 共用它。V2 的限制引用计费上界为 `min(restrictions * 256, max_restriction_edges)`；V1 固定使用旧 visitor 上界。估算使用 checked arithmetic；溢出拒绝，决不环绕成小需求。饱和乘法仅用于最终有明确上限的引用/字符串项。

build 的 memory/external 成功响应新增 `resource_plan`，来源是刚通过发布校验的 manifest，不另做长时间加载。build 只描述已经生成的图需求，不包含构建峰值预测。对于尚未生成的图，目前没有可靠的 PBF→RAM 预测能力；宿主 check 必须将其标记为未知，并在图完成后重新准入，不能用压缩 PBF 字节数冒充加载需求。图身份、图文件和数据集格式不因计划中的执行预算改变而变化。

超出 resident 预算的错误仍为 `error.resource`，包含 resource=estimated_resident_bytes、limit、observed、stage（dataset_load/engine_load），新增 estimator_version。其他非估算资源限制沿用原 resource 字段，估算版本不适用时不伪造数值。单纯算术溢出保留既有 LimitExceeded 拒绝语义，不返回虚假的精确 observed。

宿主需独立检测环境、预留并发资源、施加容器上限，把逻辑估算转换为部署所需物理峰值，并在每个阶段重新准入。规划没有实施这些宿主策略，也不保证 optional MLD 或查询工作区能在同一预算中容纳。完整发布仍需实际引擎加载与 coverage/nearest/route 验证；资源规划通过不能替代它们。
