# monad-performance-monitor

这个模块现在包含一套最小可用的 Loki / Promtail / Grafana 部署示例，用来采集并查看 `monad-performance-monitor` 输出的区块性能日志。

当前日志格式示例：

```text
msg=block_total_duration height=123 total_ms=87.421 committed_ms=84.115 stage_durations=prepare_proposal:10.002,process_proposal:12.337,finalize_block:61.776
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

这个 dashboard 基于 `msg=block_total_duration` 日志，主要展示：

- 观测窗口内区块数
- 平均总耗时 `total_ms`
- 平均提交耗时 `committed_ms`
- 最大总耗时
- 总耗时趋势
- 提交耗时趋势
- 原始区块耗时日志（包含 `height` 和 `stage_durations`）

## 验证采集是否成功

如果日志已经打到容器标准输出，可以在 Grafana Explore 里先跑这条 Loki 查询：

```logql
{job="monad-docker", service=~"monad-a"} |= "msg=block_total_duration"
```

如果能看到类似下面的日志，说明链路是通的：

```text
msg=block_total_duration height=123 total_ms=87.421 committed_ms=84.115 stage_durations=prepare_proposal:10.002,process_proposal:12.337,finalize_block:61.776
```

## 说明

- `stage_durations` 当前是字符串字段，dashboard 里保留了日志明细面板用于逐块查看各阶段耗时。
- 如果后续希望把每个阶段单独做成曲线，建议把阶段耗时改成结构化日志字段，或者为每个阶段单独输出一条日志。
