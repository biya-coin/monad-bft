# Monad 四节点压测 — 快速上手

从零初始化到跑压测的最短路径。编排脚本：`scripts/0_monad-stress-bench.sh`。

---

## 1. 前置条件

| 依赖 | 用途 |
|------|------|
| Go、Rust/cargo | 编译 `biyachaind`、`monad-node` |
| clang-19（或 clang-14 + gnu2x） | 编译 monad |
| Docker | 仅 `setup` 时清理目录 |
| `jq` | 解析账户 JSON |
| `chain-stresser` | 发压测交易 |

```bash
# 安装 chain-stresser（仓库根目录）
cd chain-stresser && make install && cd ..
```

详细工具链见 [monad-chain-stresser-bench.md](./monad-chain-stresser-bench.md#环境准备)。

---

## 2. 全新环境（首次，按顺序执行）

在 **monad-bft 仓库根目录**：

```bash
# ① 编译二进制（biyachaind + monad-node + cosmos-txpool-feed）
./scripts/0_monad-stress-bench.sh build

# ② 初始化：生成 .monad/、四节点密钥、genesis、压测账户
./scripts/0_monad-stress-bench.sh setup

# ③ 一键启动：清历史数据 → genesis 干净启动 → 四节点 + 4 路 RPC（后台）
./scripts/0_monad-stress-bench.sh start
```

`start` 会自动：

- 配置 P2P loopback IP（`172.28.0.10` ~ `172.28.0.40`，需 sudo）
- 启动 4×`biyachaind` + 4×`monad-node` + 4×`cosmos-txpool-feed`
- 日志写入 `./node-log/`

确认节点正常：

```bash
./scripts/0_monad-stress-bench.sh nodes    # 进程、forkpoint、P2P 一览
./scripts/0_monad-stress-bench.sh status   # 端口探测
```

---

## 3. 压测

### 单节点（发到节点 a）

```bash
./scripts/0_monad-stress-bench.sh stress
```

### 四节点并发（推荐，吞吐更高）

```bash
./scripts/0_monad-stress-bench.sh stress-all
```

- 若无分片文件，会**自动**执行 `shard`（`accounts.json` → `accounts.{a,b,c,d}.json`）
- 压测日志：`.monad/stress-{a,b,c,d}.log`
- 实时查看：`tail -f .monad/stress-a.log`

验证上链（gRPC 查 sequence）：

```bash
./scripts/0_monad-stress-bench.sh verify
```

---

## 4. 日常操作

| 命令 | 说明 |
|------|------|
| `start` | 停旧进程 → **默认清链数据** → genesis 重启四节点 + RPC |
| `restart` | 同 `start` |
| `stop` | 停止全部节点、RPC feed，清理 pid/socket |
| `nodes` | 查看四节点运行状态 |
| `stress` | 单节点压测（节点 a，`:26657`） |
| `stress-all` | 四路分片并发压测 |
| `shard` | 手动切分账户（`stress-all` 缺分片时会自动调用） |
| `verify` | gRPC 查压测账户 sequence |
| `reset-consensus` | 共识卡住时：清 WAL/ledger，恢复 genesis forkpoint |

保留链数据仅重启进程（调试用）：

```bash
BENCH_KEEP_DATA=1 ./scripts/0_monad-stress-bench.sh start
```

跳过 RPC feed：

```bash
BENCH_NO_RPC=1 ./scripts/0_monad-stress-bench.sh start
```

---

## 5. 目录与端口

### 工作目录

```text
.monad/                          # setup 生成，链数据与配置
├── instances/0/accounts.json    # 压测账户私钥
├── biyachain-home-{a,b,c,d}/    # biyachaind 数据
└── monad-{a,b,c,d}/             # monad 共识数据、mempool.sock

node-log/                        # start 产生的运行日志
├── biyachaind-{a,b,c,d}.log
├── monad-{a,b,c,d}.log
└── feed-{a,b,c,d}.log
```

### 端口（节点 a / b / c / d）

| 服务 | a | b | c | d |
|------|---|---|---|---|
| gRPC | 19900 | 29900 | 39900 | 49900 |
| Comet RPC（feed） | 26657 | 26667 | 26677 | 26687 |
| P2P | 172.28.0.10:8000 | .20:8000 | .30:8000 | .40:8000 |

---

## 6. 常用环境变量（压测）

| 变量 | 默认 | 说明 |
|------|------|------|
| `STRESS_CMD` | `spot-limit` | `bank` 或 `spot-limit` |
| `STRESS_ACCOUNTS_NUM` | 1000 | setup 时生成的账户总数 |
| `STRESS_ACCOUNTS_NUM_RUN` | 1000 | 实际参与压测的账户数 |
| `STRESS_TRANSACTIONS` | 800 | 每账户交易数 |
| `STRESS_RATE_TPS` | 3000 | 每片限速 TPS（`stress-all` 总吞吐 ≈ 4×该值） |
| `STRESS_SHARDS` | 4 | 分片数（1~4） |

示例：冒烟压测

```bash
STRESS_CMD=bank \
STRESS_ACCOUNTS_NUM_RUN=50 \
STRESS_TRANSACTIONS=20 \
STRESS_RATE_TPS=100 \
./scripts/0_monad-stress-bench.sh stress-all
```

---

## 7. 完全重来（清空一切再 init）

```bash
./scripts/0_monad-stress-bench.sh stop

# 可选：删除整个工作目录（会丢失密钥与 genesis，需重新 setup）
rm -rf .monad node-log

./scripts/0_monad-stress-bench.sh setup
./scripts/0_monad-stress-bench.sh start
./scripts/0_monad-stress-bench.sh stress-all
```

---

## 8. 常见问题

| 现象 | 处理 |
|------|------|
| 共识卡住 / forkpoint 不一致 | `./scripts/0_monad-stress-bench.sh stop && ./scripts/0_monad-stress-bench.sh start` |
| 仅部分节点在跑 | 必须四节点齐，`nodes` 检查后再压测 |
| RPC connection refused | 确认 `start` 未设 `BENCH_NO_RPC=1`，或手动 `rpc-all` |
| 压测 RPC 成功但 sequence 不变 | `tail -f node-log/monad-a.log` 看是否收到 tx；四节点是否都在出块 |

更多细节：[monad-chain-stresser-bench.md](./monad-chain-stresser-bench.md)
