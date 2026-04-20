#!/usr/bin/env bash
# 联调排查：feed 成功但 biyachaind gRPC 余额/sequence 不变时的对齐检查。
# 用法（与 cosmos_monad_genesis_tx_example.sh 相同的环境变量）：
#   export BIYAHOME=/tmp/biyachaind-home-xxx
#   export MONAD_ABCI_ENDPOINT=unix:///tmp/biyachain-abci.sock   # 须与 biyachaind --address 一致
#   export MONAD_COSMOS_GENESIS_PATH="$BIYAHOME/config/genesis.json"
#   export MONAD_MEMPOOL_SOCK=/tmp/monad-final-run-xxx/mempool.sock
#   ./scripts/cosmos_abci_diagnose.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
BIYACHAIND_BIN="${BIYACHAIND_BIN:-${REPO_ROOT}/biyachain-core/biyachaind}"
BIYAHOME="${BIYAHOME:-${HOME}/.biyachaind}"
DEFAULT_GENESIS="${HOME}/.biyachaind/config/genesis.json"
MONAD_COSMOS_GENESIS_PATH="${MONAD_COSMOS_GENESIS_PATH:-$DEFAULT_GENESIS}"
MONAD_ABCI_ENDPOINT="${MONAD_ABCI_ENDPOINT:-unix:///tmp/biyachain-abci.sock}"

echo "=== 1) genesis：monad 与 biyachaind 必须是同一份 ==="
echo "    MONAD_COSMOS_GENESIS_PATH=$MONAD_COSMOS_GENESIS_PATH"
echo "    BIYAHOME/genesis.json      =$BIYAHOME/config/genesis.json"
if [[ -f "$MONAD_COSMOS_GENESIS_PATH" && -f "$BIYAHOME/config/genesis.json" ]]; then
  if cmp -s "$MONAD_COSMOS_GENESIS_PATH" "$BIYAHOME/config/genesis.json"; then
    echo "    结论: 两份文件一致 (cmp OK)"
  else
    echo "    结论: **不一致** — monad 与 query 用的不是同一条链，请先对齐或改 MONAD_COSMOS_GENESIS_PATH" >&2
  fi
else
  echo "    （跳过 cmp：某路径不存在）"
fi

echo ""
echo "=== 2) ABCI：monad-node 连的地址须与 biyachaind 监听的一致 ==="
echo "    MONAD_ABCI_ENDPOINT=$MONAD_ABCI_ENDPOINT"
case "$MONAD_ABCI_ENDPOINT" in
  unix://*)
    sock="${MONAD_ABCI_ENDPOINT#unix://}"
    if [[ -S "$sock" ]]; then
      echo "    结论: socket 存在: $sock"
    else
      echo "    结论: **无此 socket** — biyachaind 未启动或 --address 不是该路径" >&2
    fi
    ;;
  tcp://* | http://* | https://* | localhost* | 127.0.0.1* | 0.0.0.0* | *:*)
    echo "    （grpc/tcp 端点，请自行确认与 biyachaind 配置一致）"
    ;;
  *)
    echo "    （未知格式，请对照文档 MONAD_ABCI_ENDPOINT）"
    ;;
esac
echo "    biyachaind 启动示例见: biyachain-core/scripts/init-standalone-abci.sh 文末（--address unix://...）"

echo ""
echo "=== 3) mempool IPC：cosmos-txpool-feed 目标 ==="
if [[ -n "${MONAD_MEMPOOL_SOCK:-}" ]]; then
  echo "    MONAD_MEMPOOL_SOCK=$MONAD_MEMPOOL_SOCK"
  if [[ -S "$MONAD_MEMPOOL_SOCK" ]]; then
    echo "    结论: socket 存在（与 monad-node --mempool-ipc-path 一致时 feed 才能进 monad）"
  else
    echo "    结论: **不是有效 socket**" >&2
  fi
else
  echo "    未设置 MONAD_MEMPOOL_SOCK（transfer 打印命令里应含真实路径）"
fi

echo ""
echo "=== 4) 链上可读性：gRPC 须指向「与 ABCI 同一进程」的 biyachaind ==="
if [[ -x "$BIYACHAIND_BIN" && -f "$BIYAHOME/config/genesis.json" ]]; then
  read_app_toml_grpc() {
    local APP_TOML="${BIYAHOME}/config/app.toml"
    GRPC_BIND_ADDR=""
    [[ -f "$APP_TOML" ]] || return 0
    local in_grpc=0
    while IFS= read -r line || [[ -n "$line" ]]; do
      if [[ "$line" =~ ^\[grpc\] ]]; then in_grpc=1; continue; fi
      if [[ "$line" =~ ^\[ ]]; then in_grpc=0; continue; fi
      if [[ $in_grpc -eq 1 && "$line" =~ address[[:space:]]*=[[:space:]]*\"([^\"]+)\" ]]; then
        GRPC_BIND_ADDR="${BASH_REMATCH[1]}"
      fi
    done <"$APP_TOML"
  }
  read_app_toml_grpc
  try_addr="${GRPC_ADDR:-127.0.0.1:9900}"
  FROM_ADDR="$(jq -r '.app_state.bank.balances[0].address // empty' "$BIYAHOME/config/genesis.json")"
  auth_json=""
  if [[ -n "$FROM_ADDR" ]]; then
    auth_json="$("${BIYACHAIND_BIN}" query auth account "$FROM_ADDR" \
      --grpc-addr "$try_addr" --grpc-insecure --home "$BIYAHOME" -o json 2>/dev/null)" || true
  fi
  if [[ -n "$auth_json" ]]; then
    seq="$(echo "$auth_json" | jq -r '.account.value.base_account.sequence // .account.base_account.sequence // empty')"
    echo "    gRPC ${try_addr} 可读 auth，发送方 sequence=$seq（离线 transfer 的 SEQUENCE 须与此一致）"
  else
    echo "    无法通过 gRPC 读 auth（biyachaind 未开 gRPC 或端口不是 ${try_addr}）" >&2
  fi
else
  echo "    跳过（无 biyachaind 或 genesis）"
fi

echo ""
echo "=== 5) 运行时应在日志里看到（否则交易未进块或未 Finalize）==="
echo "    monad-node (RUST_LOG=info): 行含 \"committed cosmos block\""
echo "    biyachaind: sdk_prepare_timing / app_finalize_block 等（见 docs/monad-cosmos-abci-debugging.md）"
echo "    若怀疑 PrepareProposal 丢交易: 在 monad 日志中搜 PrepareProposal failed / skipping proposal"
