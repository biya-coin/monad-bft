# monad-performance-monitor

这个模块现在包含一套最小可用的 Loki / Promtail / Grafana 部署示例，用来采集并查看 `monad-performance-monitor` 输出的区块性能日志。

## 部署步骤

1. 启动 Loki 服务

   在项目根目录执行：

   ```bash
   docker compose -f monad-performance-monitor/monitor/server/docker-compose.loki.yml up -d
   ```

2. 打开 Grafana
   - 地址：`http://localhost:3000`
   - 默认账号密码通常是 `admin / admin`，首次登录按提示修改。

3. 添加 Loki 数据源
   - Grafana 中进入 `Connections` → `Data sources` → `Add data source`
   - 选择 `Loki`
   - URL 填 `http://ip:3100`

4. 导入 dashboard

   在仓库根目录执行下面的命令即可通过 Grafana HTTP API 导入：

   ```bash
   python3 -c 'import json,sys; p="monad-performance-monitor/monitor/grafana/grafana-block-performance.json"; print(json.dumps({"dashboard": json.load(open(p, "r", encoding="utf-8")), "folderId": 0, "overwrite": True}))' | curl -u admin:admin -H 'Content-Type: application/json' -X POST http://127.0.0.1:3000/api/dashboards/db -d @-
   ```
