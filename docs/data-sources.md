# 数据来源与复现

合成 fixture 由 `backend/testdata/generate_fixture.py` 从项目自有整数坐标和道路规则生成，无真实地图数据；支持普通/dense 和 raw/zlib 四种组合。普通测试不下载或读取任何兄弟目录。

## 固定真实城市样本

- 内容：OSM Monaco extract 历史固定样本；验证时使用本地已有 PBF 文件。
- SHA-256：`36c0f0029ed838897322382611b67e8e5e7bcb07d5b17a25f3574dec54f1c23c`。
- 字节数：1,759,101。
- PBF replication timestamp：1457137562，即 2016-03-05 00:26:02 UTC。它是历史固定快照，不能用于声称当前道路状态。
- 本次验证获取方式：2026-10-19 从已存在的本地样本复制到临时数据目录，并实际计算哈希；本轮外网 DNS 不可用，没有声称重新在线下载。

地图数据与代码采用不同许可。署名：© OpenStreetMap contributors。数据许可及说明见 https://www.openstreetmap.org/copyright 与 https://opendatacommons.org/licenses/odbl/1-0/ 。使用/分发原始或派生路网时应保留来源、署名与适用 ODbL 条件，不能将道路数据改标为本项目代码许可证。

PBF、图和完整基准原始结果置于显式非 Git 数据目录；不要把临时目录作为产品运行依赖。manifest 和测量摘要保留原始哈希与身份。复现需自行提供与上述哈希一致的 PBF，并在构建前校验；本仓库不提供该历史样本的下载镜像，不得用 latest 文件冒充同一快照。

OD 清单由城市基准程序使用种子 20261019，在固定方向边端点/中点采样 1,000 对。分布包含多个连通分量，NoRoute 是需要报告的结果，不是必须丢弃的坏样本；该采样不是所有城市使用模式的代表。


## 南宁固定切片与限制诊断

本轮输入为用户提供的广西原始 OSM PBF，未重新在线下载。原始文件 52,621,919 bytes，SHA-256 `eea0eb47bba0640b2c6468a1b82cb2c5f8ea1261a5333f47420e6d569f0b769d`，replication timestamp 为 2026-08-03T20:21:36Z。原始数据、切片和派生图不随源码分发；复现需自行准备一致输入并保留 OSM 署名和适用 ODbL 条件。

南宁 bbox 为 `108.25,22.75,108.42,22.88`，由 osmium complete_ways 提取，未删除道路或限制标签。已验证切片 SHA-256 为 `e7eee17e89b65bdff18bdc1be45e59952a1a3774fc9845a9488e67968f31c4ba`。不同提取工具版本或头部元数据可能改变文件字节；不能仅凭 bbox 声称得到了同一哈希。

```bash
osmium extract --strategy complete_ways --bbox 108.25,22.75,108.42,22.88 \
  guangxi.osm.pbf --output nanning.osm.pbf
routeweft preflight --input nanning.osm.pbf --profile driving --json
osmium getid guangxi.osm.pbf r6843783 r6843784 r6843785 r8700340 r8700341 \
  --output original-relations.osm
osmium getid nanning.osm.pbf r6843783 r6843784 r6843785 r8700340 r8700341 \
  --output extracted-relations.osm
```

本轮比较确认，6843783/6843784/6843785 在原始 PBF 和切片中都缺 via；8700340 缺 to/via，8700341 缺 from/via。不能把它们误诊为合法混合 via 或提取时丢失 via。默认 reject 保持失败；显式 block_ways 将相关 8 条道路排除，不猜测缺失转向语义。成功结果与覆盖损失见 [性能与验证](performance.md)。

complete_ways 不保证所有 restriction 及引用闭合。遇 MissingReference 时分别核对原始/切片关系和引用节点、道路，必要时使用 `osmium getid --add-referenced` 导出完整关系上下文以排查。修复提取闭包后必须重建并复验；不要通过删除关系或道路标签、补造连接点来获得成功。即使引用完整，成员形态、模式、方向和成本也需要预检与真实查询验收。

合成生成器的 `--restriction-case missing-via|missing-reference|valid` 提供原创建图数据，复现成员形态与错误分类；不捆绑真实 OSM 几何。普通测试不依赖 osmium 或用户文件。

本次切片头部没有 replication timestamp，因此 manifest.source_snapshot 保持 null。父文件的 2026-08-03 快照来自原始头部，属于外部来源证据，不冒充切片自身元数据；下游配对以实际消费的同一切片 SHA-256 为准。
