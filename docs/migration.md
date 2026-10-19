# 命名与接口迁移

领域库和命令行 crate 分别为 `routeweft-domain`、`routeweft-daemon`，Rust 导入使用下划线形式。CLI 二进制统一为 `routeweft`，前端私有包为 `@routeweft/frontend`。

旧版不含数据集参数的位置参数调用不再支持。当前调用必须显式提供数据集：

```bash
routeweft route --dataset dataset/ --coordinates 'lon,lat;lon,lat' --json
```

先执行 build/verify。缺少数据集不会自动使用合成图，也不再将默认未配置适配器当作产品查询实现。

P0 `Engine::new(...).route(node,node,...)` 已改名为 `NodeEngine`；公开磁盘引擎为 `Engine::open(...).route(&points,options,&budget)`。实验版本没有向后兼容保证，未来格式变化将通过版本拒绝，不能直接重解释旧文件。
