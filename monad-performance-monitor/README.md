# monad-performance-monitor

这个模块现在包含一套最小可用的 Loki / Promtail / Grafana 部署示例，用来采集并查看 `monad-performance-monitor` 输出的区块性能日志。

当前日志格式示例：

```text
msg=consensus height=123 new_height=4.228 new_round=8.120 propose=11.004 prevote=17.553 precommit=22.331 commit=28.108 total=87.421
msg=txs height=123 txs=42
```

## 目录说明

- [monitor/server/docker-compose.loki.yml](monitor/server/docker-compose.loki.yml)：启动 Loki、Promtail、Grafana
- [monitor/server/loki.yaml](monitor/server/loki.yaml)：Loki 本地存储配置
- [monitor/server/promtail.yaml](monitor/server/promtail.yaml)：Promtail 日志采集配置
- [monitor/grafana/grafana-block-performance.json](monitor/grafana/grafana-block-performance.json)：Grafana dashboard

## 部署步骤

1. 修改 [monitor/server/promtail.yaml](monitor/server/promtail.yaml)
   - 如果你的节点日志不在 `/home/ubuntu/biyachain/chain-stresser/node-log`，先把挂载路径改成实际路径。
   - 如果你的 Docker Compose project 名不是 `monad`，把 `compose_project` 的过滤条件一起改掉。
   - 如果服务名不符合 `(biyachaind|monad|monad-rpc)-.*`，同步调整正则。

2. 启动 Loki 全套服务

   在 [monitor/server](monitor/server) 目录执行：

   ```bash
   docker compose -f docker-compose.loki.yml up -d
   ```

3. 打开 Grafana
   - 地址：`http://localhost:3000`
   - 默认账号密码通常是 `admin / admin`，首次登录按提示修改。

4. 添加 Loki 数据源
   - Grafana 中进入 `Connections` → `Data sources` → `Add data source`
   - 选择 `Loki`
   - URL 填 `http://loki:3100`（如果 Grafana 不在同一个 compose 网络里，改成宿主机可访问地址）

5. 导入 dashboard

   在仓库根目录执行下面的命令即可通过 Grafana HTTP API 导入：

   ```bash
   python3 -c 'import json,sys; p="/home/cyyu/monad-workspace/monad-bft/monad-performance-monitor/monitor/grafana/grafana-block-performance.json"; print(json.dumps({"dashboard": json.load(open(p, "r", encoding="utf-8")), "folderId": 0, "overwrite": True}))' | curl -u admin:admin -H 'Content-Type: application/json' -X POST http://192.168.25.128:3000/api/dashboards/db -d @-
   ```

## Dashboard 内容

这个 dashboard 基于 `msg=consensus` 和 `msg=txs` 日志，主要展示：

- 观测窗口内区块数
- 观测窗口内总交易数
- 平均出块时间 `total`
- 平均 TPS
- 共识阶段耗时趋势（包含 `new_height / new_round / propose / prevote / precommit / commit / total`）

## 验证采集是否成功

如果日志已经打到容器标准输出，可以在 Grafana Explore 里先跑这条 Loki 查询：

```logql
{job="monad-docker", service=~"monad-a"} |= "msg=consensus"
```

如果能看到类似下面的日志，说明链路是通的：

```text
msg=consensus height=123 new_height=4.228 new_round=8.120 propose=11.004 prevote=17.553 precommit=22.331 commit=28.108 total=87.421
msg=txs height=123 txs=42
```

## 说明

- `msg=consensus` 采用固定字段结构化日志，Grafana 可直接按字段出图。
- TPS 不需要单独埋点计算，只依赖 `msg=txs height=... txs=...` 日志在 Grafana 中聚合统计。
- 阶段耗时与总耗时的展示风格对齐 [monad-bft/biyachain-core/monitor/grafana/grafana-loki.json](monad-bft/biyachain-core/monitor/grafana/grafana-loki.json) 中的参考面板。
