# RouteWeft 后端

独立 Rust workspace。`routeweft-domain`、`routeweft-dataset`、`routeweft-preprocess`、`routeweft-routing` 和 `routeweft-daemon` 实现离线 PBF 到库/CLI 查询链路。

在本目录运行：

```bash
cargo run --bin routeweft -- --help
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

完整用法见 [根 README](../README.md)、[API](../docs/api.md)、[CLI](../docs/cli.md) 和 [架构](../docs/architecture.md)。命名已从旧名迁移，旧位置参数 CLI 不再兼容。

daemon 中原有 `RoutingAppService` 和未配置适配器保留为历史编排接口，其默认装配仍明确返回未配置；实际 `routeweft` CLI 使用磁盘数据集引擎，不能以旧服务状态判断新引擎能力。当前没有 HTTP 服务。
