# Monad + Cosmos ABCI 单节点联调说明

## 目标

这份文档用于说明当前仓库里已经做过的 `Monad 共识 -> Cosmos 应用层` 适配改动，以及如何手动启动、调试单节点出块链路。

目标是：

- `biyachaind` 作为独立 ABCI 应用运行
- `monad-node` 通过 ABCI 驱动应用层
- 单节点环境下能够持续正常出块

## 已做改动

### 1. CometBFT ABCI Rust 绑定

新增 crate：

- `monad-cometbft-proto`

作用：

- 从 `cometbft/proto/cometbft/abci/v1` 及依赖生成 Rust types
- 提供 gRPC stub
- 提供 socket ABCI 所需的 `Request` / `Response` protobuf 结构

关键点：

- 使用 vendored `protoc`
- 补了最小 `gogoproto/gogo.proto`

### 2. Cosmos 执行协议类型

新增 crate：

- `monad-cosmos-types`

作用：

- 定义 `CosmosExecutionProtocol`
- 定义 `ProposedCosmosHeader`
- 定义 `CosmosBlockBody`
- 定义 `CosmosFinalizedHeader`

### 3. Monad 侧 Cosmos 集成层

新增 crate：

- `monad-cosmos-integration`

作用：

- `CosmosTxPoolExecutor`
- `CosmosBlockValidator`
- `CosmosBlockPolicy`
- `CosmosCommitStore`
- `CosmosStateBackend`
- `CosmosLedger`

已经补过的关键兼容逻辑：

- 支持 `grpc` 和 `socket` 两种 ABCI 传输
- `socket` 模式补了 varint length-delimited protobuf 流
- `socket` 模式补了 `FlushRequest/FlushResponse`
- 复用持久连接，避免每次请求断开重连
- 启动时执行 `Info -> InitChain`
- 从 Cosmos `genesis.json` 注入 `consensus.params`
- 将 `staking.params.bond_denom` 对齐到 `genutil.gen_txs` 里实际使用的 denom
- 单节点 proposer 的 proposal 本地回环
- 应用层高度闸门，避免共识高度失控
- 首块前置块的特殊处理，避免过早触发 `PrepareProposal/ProcessProposal`

### 4. 节点主路径切换

改动了：

- `monad-node-config`
- `monad-node`

作用：

- 把 `ExecutionProtocolType` 从 `EthExecutionProtocol` 切到 `CosmosExecutionProtocol`
- 把节点运行时替换为 Cosmos 集成层组件

## 关键经验

### 1. 不要复用被污染过的运行目录

联调过程中，以下内容会被运行态写脏：

- `forkpoint.toml`
- `forkpoint.rlp*`
- `validators.toml.*`
- `ledger/*`
- `wal_*`
- `*.sock`
- `peers.json`

如果要验证“从 genesis 冷启动”，建议总是使用新的临时目录。

### 2. 不要直接复用已有 `~/.biyachaind`

如果之前已经做过多轮实验，应用层高度可能已经推进，导致新的 `monad-node` 从 `height=1` 启动时收到：

- `invalid height: 1; expected: 2`

建议每次调试使用新的临时 `BIYAHOME`。

## 系统前置条件

```bash
sudo sysctl -w net.core.rmem_max=62500000
sudo sysctl -w net.core.rmem_default=62500000
sudo sysctl -w net.core.wmem_max=62500000
sudo sysctl -w net.core.wmem_default=62500000
```

如果缺这些，`monad-node` 可能在 dataplane 初始化时直接退出。

## 推荐调试步骤

### 步骤 0：先清理旧进程和旧运行产物

如果你之前已经跑过多轮实验，强烈建议先做一次显式清理，避免旧 socket、旧 `BIYAHOME`、旧 `MONAD_RUN_DIR` 污染当前结果。

```bash
# 停掉旧的 monad-node / biyachaind
pkill -9 -f 'target/debug/monad-node' || true
pkill -9 -f 'biyachaind start' || true

# 删除旧的 ABCI socket
rm -f /tmp/biyachain-abci.sock
rm -f /tmp/biyachain-abci-fresh.sock

# 删除之前创建的临时目录（如果有）
rm -rf /tmp/biyachaind-home-*
rm -rf /tmp/monad-final-run-*
rm -rf /tmp/monad-clean-run-*
```

如果你不想清理所有旧目录，至少要保证本次使用的是全新的：

- `BIYAHOME`
- `MONAD_RUN_DIR`

### 步骤 1：编译 `biyachaind`

```bash
cd "/home/bryce/vs_workplace/monad-bft/biyachain-core"
go build -o ./biyachaind ./cmd/biyachaind
```

### 步骤 2：初始化新的 Cosmos home

```bash
export BIYAHOME="/tmp/biyachaind-home-$(date +%s)"
export BIYACHAIND_BIN="/home/bryce/vs_workplace/monad-bft/biyachain-core/biyachaind"
"/home/bryce/vs_workplace/monad-bft/biyachain-core/scripts/init-standalone-abci.sh"
```

### 步骤 3：启动独立 ABCI 应用

这里建议总是使用一个新的 socket 路径，例如 `/tmp/biyachain-abci-fresh.sock`，不要复用旧的 `/tmp/biyachain-abci.sock`。

```bash
export MONAD_ABCI_SOCKET="unix:///tmp/biyachain-abci-fresh.sock"
rm -f /tmp/biyachain-abci-fresh.sock
"/home/bryce/vs_workplace/monad-bft/biyachain-core/biyachaind" start \
  --home "$BIYAHOME" \
  --with-comet=false \
  --transport socket \
  --address "$MONAD_ABCI_SOCKET" \
  --grpc.enable=true \
  --grpc.address=0.0.0.0:9900 \
  --json-rpc.enable=false \
  --minimum-gas-prices 1byb
```

**gRPC 端口说明（重要）**：本仓库的 `biyachaind` 将 **默认 gRPC 监听设为 `0.0.0.0:9900`**（见 `biyachain-core/cmd/biyachaind/config/config.go` 的 `DefaultGRPCAddress`），**不是** Cosmos 生态里更常见的 `9090`。若 `biyachaind query ... --grpc-addr` 或脚本里仍写 `127.0.0.1:9090`，会连错端口，表现为「grpc 没生效」。请用 **`127.0.0.1:9900`**（或与 `--grpc.address` / `app.toml` 中 `address` 里端口一致）。`--grpc.enable=true` 与默认一致，若 `app.toml` 已 `enable = true`，通常无需再改。

**为何会马上出现 `stopping gRPC server`（你没关终端也会发生）**：默认 **`--json-rpc.enable=true`** 且 **`--json-rpc.enable-indexer=true`** 时，会启动 **EVM 索引器**（日志里 `EVMIndexerService`）。它要通过 CometBFT 的 **RPC（默认 `tcp://localhost:26657`）** 订区块；独立 ABCI 模式**没有** 26657，索引器启动失败会连带取消一组后台任务，**gRPC / API 会被关掉**，进程本身往往还在（仍在等 ABCI）。解决办法：**二选一**——加上 `--json-rpc.enable=false`（若不需要 JSON-RPC），或保留 JSON-RPC 但关闭索引器：`--json-rpc.enable-indexer=false`。若仍要完整 JSON-RPC + 索引，需要可用的 Comet RPC，不适合当前「仅 socket ABCI」的跑法。

预期日志：

- `starting standalone ABCI application`
- `Waiting for new connection...`


### 步骤 5：准备新的 Monad 运行目录

```bash
export MONAD_RUN_DIR="/tmp/monad-final-run-$(date +%s)"
mkdir -p "$MONAD_RUN_DIR/ledger/headers" "$MONAD_RUN_DIR/ledger/bodies" "$MONAD_RUN_DIR/ledger/cosmos-commits"
cp "/home/bryce/vs_workplace/monad-bft/docker/devnet/monad/config/forkpoint.genesis.toml" "$MONAD_RUN_DIR/forkpoint.toml"
cp "/home/bryce/vs_workplace/monad-bft/docker/devnet/monad/config/validators.toml" "$MONAD_RUN_DIR/validators.toml"

# 建议确认这两个文件真的存在
test -f "$MONAD_RUN_DIR/forkpoint.toml"
test -f "$MONAD_RUN_DIR/validators.toml"
```

### 步骤 6：启动 `monad-node`

```bash
cd "/home/bryce/vs_workplace/monad-bft"
export MONAD_ABCI_ENDPOINT="${MONAD_ABCI_SOCKET:-unix:///tmp/biyachain-abci-fresh.sock}"
export MONAD_COSMOS_GENESIS_PATH="$BIYAHOME/config/genesis.json"
export RUST_LOG=info

cargo run -p monad-node --bin monad-node -- \
  --secp-identity "/home/bryce/vs_workplace/monad-bft/docker/devnet/monad/config/id-secp" \
  --bls-identity "/home/bryce/vs_workplace/monad-bft/docker/devnet/monad/config/id-bls" \
  --node-config "/home/bryce/vs_workplace/monad-bft/docker/devnet/monad/config/node.toml" \
  --forkpoint-config "$MONAD_RUN_DIR/forkpoint.toml" \
  --validators-path "$MONAD_RUN_DIR/validators.toml" \
  --wal-path "$MONAD_RUN_DIR/wal" \
  --mempool-ipc-path "$MONAD_RUN_DIR/mempool.sock" \
  --control-panel-ipc-path "$MONAD_RUN_DIR/controlpanel.sock" \
  --ledger-path "$MONAD_RUN_DIR/ledger" \
  --statesync-ipc-path "$MONAD_RUN_DIR/statesync.sock" \
  --triedb-path "$MONAD_RUN_DIR" \
  --persisted-peers-path "$MONAD_RUN_DIR/peers.json" > monad_out.log



```

## 成功标志

### `monad-node` 侧

日志里应该看到：

- `received ABCI InitChain response`
- `done syncing, initializing consensus`
- 连续的 `Created new QC`
- 连续的 `committed cosmos block`

### `biyachaind` 侧

日志里应该看到：

- `InitChain chainID=... initialHeight=1`
- 连续的 `sdk_prepare_timing height=N`
- 连续的 `app_internal_finalize_block height=N`
- 连续的 `app_finalize_block height=N`

### ledger 落盘

检查：

```bash
python3 - <<'PY'
from pathlib import Path
import os
base = Path(os.environ["MONAD_RUN_DIR"]) / "ledger"
for p in [base/'cosmos-commits', base/'headers', base/'bodies']:
    print(p.name, [x.name for x in p.glob('*')][:20], 'count=', len(list(p.glob('*'))))
PY
```

成功时：

- `cosmos-commits` 里会出现 `0.rlp`, `1.rlp`, `2.rlp`, ...
- `headers` 和 `bodies` 也会持续增长

## 常见问题

### 1. `invalid height: 1; expected: 2`

原因：

- 复用了已经跑过的 `BIYAHOME`

处理：

- 重新创建新的 `BIYAHOME`
- 重新初始化 genesis
- 确认 `MONAD_ABCI_ENDPOINT` 指向的是本次新起的 socket，而不是旧 socket

### 2. `no nodes to blocksync from`

原因：

- 运行目录里混入了旧的 `forkpoint.rlp` 或脏的 forkpoint 状态

处理：

- 使用新的 `MONAD_RUN_DIR`
- 只复制 `forkpoint.genesis.toml`
- 不要让 `forkpoint.rlp*`、`forkpoint.toml.*` 混进新的运行目录

### 3. `failed to get consensus params`

原因：

- 首块初始化路径没走通
- 或者没用新的 Monad 侧兼容逻辑

处理：

- 先跑 `debug_abci_first_block`
- 确认它成功后再启动 `monad-node`

## 共识 → 应用层：交易为何有时「进了 monad 仍不改 gRPC 状态」

本节说明 **与 shell 脚本无关** 的链路：`cosmos-txpool-feed` 只负责把 **已签名的 protobuf 交易字节** 送进 `monad-node`；**状态是否变化** 取决于后续 **共识是否把该字节放进块里**，以及 **ABCI 是否在 `FinalizeBlock` 里执行成功**。实现见 `monad-cosmos-integration`（`CosmosTxPoolExecutor`、`CosmosBlockValidator`、`CosmosCommitStore::drain_app_commits`）。

### 端到端路径（简化）

```text
cosmos-txpool-feed
  → monad IPC: InsertForwardedTxs → CosmosTxPoolExecutor.pending_txs
  → 共识发起 CreateProposal：从 pending 取出 candidate_txs（受 tx_limit / proposal_byte_limit 等限制）
  → ABCI PrepareProposal（多数情况）或「应用层仍为 genesis 高度」时的旁路
  → 形成 Proposal → 共识投票、校验
  → 若 seq_num > 4：ABCI ProcessProposal（见下）
  → 块 finalize 后：Ledger → drain_app_commits → ABCI FinalizeBlock + Commit
  → 此时 biyachain 状态才更新；你用的 gRPC `query bank` / `auth` 才应变化
```

### 与 `monad-cosmos-integration` 对齐的几个硬条件

1. **`MONAD_ABCI_ENDPOINT` 必须指向当前正在跑的 `biyachaind` 的 `--address`（同一 unix socket 或同一 gRPC）**  
   否则 PrepareProposal / ProcessProposal / FinalizeBlock 连的是**别的进程或空**，表现就是「monad 日志还在出块，应用状态不对」。

2. **`MONAD_COSMOS_GENESIS_PATH` 与当前 `BIYAHOME/config/genesis.json` 同一份**  
   `CosmosCommitStore` 与 ABCI `Info` 高度对齐；若 genesis 与真实 app 不一致，会出现「共识高度、本地 commit 记录、应用层高度」三者拉扯，表现为 `skipping proposal while application height lags` 或 FinalizeBlock 失败类日志。

3. **应用层高度滞后（`latest_app_height`）与共识 `seq_num` 的关系**  
   当 `seq_num > latest_app_height + COSMOS_MAX_AHEAD_BLOCKS`（当前 `COSMOS_MAX_AHEAD_BLOCKS = 4`）时，**本轮 CreateProposal 会被跳过**（见 `CosmosTxPoolExecutor` 里对应 `warn!`），**pending 里的交易不会在本轮被取出**；若长期滞后，交易会长时间留在 pending 里。

4. **`PrepareProposal` 可以改写、清空候选交易**  
   共识从 pending 取出的 `candidate_txs` 会作为请求发给 ABCI；**返回的 `txs` 列表才是最终进块体**。若 `n_candidates > 0` 且 `n_included = 0`**（monad `RUST_LOG=info` 下会有 WARN）**，说明 **SDK/应用在 PrepareProposal 里丢弃了全部交易**（常见：签名 sequence 错、fee 低于 `minimum-gas-prices`、编码与链不匹配等），此时 **块仍可提交，但 `execute_txs` 几乎为空**，gRPC 不变。

5. **`ProcessProposal`（`seq_num > 4` 时）**  
   `CosmosBlockValidator` 对 **高度大于 4** 的块会调用 ABCI `ProcessProposal`；**`status != 1` 则整票拒绝**。若 PrepareProposal 放进块的交易在 ProcessProposal 被拒，共识路径会失败或换块（具体以 monad 共识状态机为准），**同样不会得到你期望的「已执行」状态**。

6. **前几个高度上的特殊处理**  
   `seq_num <= COSMOS_MAX_AHEAD_BLOCKS` 时，`ProcessProposal` 校验在代码里**旁路**（便于首块/前几块）；**`latest_app_height == GENESIS_SEQ_NUM`（0）时，`PrepareProposal` 会旁路**，直接把 `candidate_txs` 塞进块。高度上去之后，**必须**走真实的 PrepareProposal/ProcessProposal，调试重点应放在 **ABCI 应用** 与 **签名/费用**。

### 排查时优先看什么（不靠脚本）

| 现象 | 优先怀疑 |
|------|----------|
| monad 有 `committed cosmos block`，gRPC 永远不变 | 同一 ABCI 是否连到正在 query 的那条 `biyachaind`；`PrepareProposal` 是否返回空（见 WARN） |
| 有 `skipping proposal while application height lags` | 应用高度与共识高度差过大；`cosmos-commits` 与 ABCI `Info` 是否一致 |
| `n_candidates > 0` 且 `n_included = 0` | 应用层拒单：在 **biyachain** 侧把日志调细，或查 `minimum-gas-prices`、account sequence |
| 从未出现 `cosmos txpool: received forwarded raw txs` | `cosmos-txpool-feed` 未连上当前 `monad-node` 的 `--mempool-ipc-path` |

### 与「脚本」的边界

- **脚本**只负责：本地构造/签名交易、生成 `tx.raw`、提示如何 `feed`。  
- **是否上链** = 上述 **IPC → pending → PrepareProposal → 共识 → FinalizeBlock** 是否全部成功；**应在 monad / biyachain 日志与 ABCI 行为上排查**，而不是在脚本里反复改参数。

## 从 genesis 账户查询与转账（`cosmos-txpool-feed`）

仓库脚本 `scripts/cosmos_monad_genesis_tx_example.sh` 会：

- 从 `$BIYAHOME/config/genesis.json` 读取 `chain_id`、第一条 `bank.balances` 地址与 `denom`、并解析 `account_number`（用于离线签名）。
- **`query`**：仅用 gRPC 打印注资地址与 `recipient` 的 **bank 余额**，以及发送方 **`from_sequence`**（需 `biyachaind` 已运行且 `[grpc] enable = true`，默认连 `127.0.0.1:9900`）。
- **`transfer`**：用与 genesis 注资对应的 key（默认名 `validator`）构造 `bank send` → `tx sign`（`--offline -a/-s`）→ `tx encode` 得到 **base64 再解码为 `tx.raw`**，并打印一条 `cosmos-txpool-feed` 命令。

请先 `chmod +x scripts/cosmos_monad_genesis_tx_example.sh`，并设置与 `monad-node --mempool-ipc-path` 一致的 `MONAD_MEMPOOL_SOCK`。若签名后链上状态不变，多半是 **sequence** 与链上不一致，可先 `export SEQUENCE=…` 为 `query` 里看到的 `from_sequence`，或用 gRPC 查 `auth account`。

链上查询失败时，通常是因为：**未启动 `biyachaind`**，或 `$BIYAHOME/config/app.toml` 里 **`[grpc] enable = false`**，或 **客户端连错端口**（biyachaind 默认为 **9900**，不是 9090）。请保证应用进程已运行，且例如：

```toml
[grpc]
enable = true
address = "0.0.0.0:9900"
```

### `cosmos-txpool-feed` 显示成功，但 `query` 余额 / `from_sequence` 不变

说明 **`feed` 只把字节送进 monad 的 mempool IPC**；**biyachaind 的 gRPC 状态** 只有在 **Monad 出块且 ABCI `FinalizeBlock` / `Commit` 成功** 后才会变。

请逐项核对：

1. **`MONAD_COSMOS_GENESIS_PATH`**（或默认 `~/.biyachaind/config/genesis.json`）必须与 **`biyachaind` 实际使用的 `BIYAHOME/config/genesis.json` 是同一份**；否则 monad 与查询的 app 不是同一套链。
2. **monad-node 的 ABCI 地址**（`MONAD_ABCI_SOCKET` 等）必须与 **正在跑的那条 `biyachaind` 的 ABCI socket** 一致；你用来 `query` 的 gRPC 也必须指向 **该进程**（同一 `BIYAHOME` 数据目录）。
3. **`MONAD_MEMPOOL_SOCK`** 必须与 `monad-node --mempool-ipc-path` **完全一致**（无效路径时 feed 会 ENOENT）。
4. 看 **monad-node 日志**：是否有 `PrepareProposal failed`、`FinalizeBlock`/`Commit` 失败、`skipping proposal while application height lags` 等。可临时提高 `RUST_LOG=info` 或 `debug`。
5. 若 `from_sequence` 与离线签名用的 **`SEQUENCE` 不一致**，交易会在 **CheckTx/DeliverTx** 被拒，状态不变。

**第二行出现收款地址但 `(empty)`**：表示脚本在查本地 key `recipient` 对应地址的链上余额；**有余额才会显示数额**，否则为 `(empty)`，不是「半上链」。

**一键对齐检查**：设置与当前联调相同的 `BIYAHOME`、`MONAD_COSMOS_GENESIS_PATH`、`MONAD_ABCI_ENDPOINT`、`MONAD_MEMPOOL_SOCK` 后执行：

```bash
./scripts/cosmos_monad_genesis_tx_example.sh diagnose
# 或: ./scripts/cosmos_abci_diagnose.sh
```

脚本会比对你本机的 `MONAD_COSMOS_GENESIS_PATH` 与 `$BIYAHOME/config/genesis.json` 是否一致、`MONAD_ABCI_ENDPOINT` 的 unix socket 是否存在、`MONAD_MEMPOOL_SOCK` 是否存在，并提示应在 monad / biyachaind 日志中看到的关键字。

**monad 侧（`RUST_LOG=info`）与「是否上链」相关的日志**：

- `cosmos txpool: received forwarded raw txs`：`cosmos-txpool-feed` 已把交易字节送进 monad pending；若 **从未出现**，说明 feed 未进当前进程或 IPC 路径不对。
- `cosmos txpool: txs in proposal (candidates … vs after PrepareProposal)`：`n_candidates` 为从 pending 取出的笔数，`n_included` 为 **ABCI PrepareProposal 返回**的笔数。若 **`n_candidates > 0` 且 `n_included = 0`**，会再打一条 **WARN**：应用层在 PrepareProposal 阶段把交易全部丢掉（费率、序列号、编码等需在 biyachain 侧查）。
- `committed cosmos block`：共识已 finalize 该高度的 Cosmos 块；若上有 `n_included = 0`，则该块无用户交易，状态不会因你的转账而变。

## 建议

每次联调都用两套新的临时目录：

- 新的 `BIYAHOME`
- 新的 `MONAD_RUN_DIR`

并且建议固定再加一个新的 socket 路径：

- 新的 `MONAD_ABCI_SOCKET`

这样能最大限度避免旧状态污染，把问题限制在当前这次运行本身。

## 多节点共识（单主机 / Docker）

若要在同一台机器上测试多验证者之间的共识出块，并倾向用 Docker 隔离端口与进程，见 [monad-cosmos-multinode-single-host.md](./monad-cosmos-multinode-single-host.md)（说明当前仓库无现成一键脚本、需要自行对齐验证者集 / forkpoint / P2P bootstrap，并给出 compose 骨架与操作清单）。
