# CLI schema 1

`routeweft --help` 显示命令。除帮助外，所有成功和错误均为单行 JSON，stdout 只含协议数据；日志预留 stderr。`--json` 是显式标记，省略后也输出 JSON。重复/未知选项、位置参数和缺失值拒绝。

| 命令 | 必须参数 | 行为 |
| --- | --- | --- |
| build | --input、--profile driving、--output | 从本地 PBF 构建，不覆盖已有目录 |
| inspect | --dataset | 完整校验后返回 manifest，非不校验的快速读取 |
| verify | --dataset | 完整校验后返回 verified=true 与 manifest |
| nearest | --dataset、--coordinate lon,lat | 返回有限方向候选及 dataset/profile/format |
| route | --dataset、--coordinates lon,lat;lon,lat | 返回几何、米/秒成本、分段、步骤、吸附位置及身份 |

查询选项：`--radius-m` 默认 1000，`--max-work` 默认 2,000,000，`--max-states` 默认 250,000，`--max-queue` 默认 500,000，`--timeout-ms` 默认 10,000。deadline 从数据集加载完成后开始，限制实际查询计算，不包括进程启动/加载；宿主如需请求总时限，应在库外统筹。

成功包络：`{"schema":1,"status":"ok",...}`。错误包络：`{"schema":1,"status":"error","error":{"kind":"NoRoute","message":"NoRoute"}}`。

| 退出码 | 类别 |
| ---: | --- |
| 0 | 成功 |
| 2 | InvalidInput / UnsupportedProfile |
| 3 | NoSegment / NoRoute |
| 4 | Io / InvalidDataset / IncompatibleFormat / 构建及成本错误 |
| 5 | 查询 BudgetExceeded / Cancelled |

构建/加载上限归为数据处理错误退出 4，错误 kind 仍区分 BudgetExceeded；查询预算耗尽退出 5。当前 CLI 没有取消信号到 AtomicBool 的桥接，协作取消由库宿主提供；操作系统终止进程仍遵循系统行为。

Route schema：`dataset_id/profile/format/distance_m/duration_s/geometry/legs/steps/snapped`；Point 为 `{lon,lat}`。Leg 的 geometry_start/end 是包含两端的几何索引。Step 附 maneuver/name/米/秒/几何索引/可空 exit。所有 waypoint 成本累加和全程一致；无路不返回虚构的直线几何。
