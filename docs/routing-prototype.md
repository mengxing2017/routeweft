# P0 原型历史与当前替代

2026-10-19 的首批 P0 仅实现内存有向图、禁止边序列 Dijkstra、平面投影及 720 组合成节点对验证。当时没有 PBF、磁盘加载和坐标查询，默认 CLI 返回未配置。

后续迁移已引入 dataset/preprocess crate、真实磁盘引擎和新 CLI。原节点求解器保留为 `routeweft_routing::NodeEngine`，用于小图参考回归；正式入口改为 `routeweft_routing::Engine`。旧 domain/daemon 名称也已迁移。

当前能力以 [README](../README.md)、[API](api.md)、[profile](driving-profile.md) 和 [性能记录](performance.md) 为准。原型的测试数量和未实现列表不再代表当前状态。投影函数的第三方许可继续由 [第三方清单](../THIRD_PARTY_NOTICES.md) 维护。
