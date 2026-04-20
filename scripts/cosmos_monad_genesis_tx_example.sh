#!/usr/bin/env bash
# 从 genesis.json 读取链 ID、denom、创世账户；演示「查询」与「转账」并生成可交给 monad-node 的 tx.raw。
#
# 前置：
#   - 已用 biyachain-core/scripts/init-standalone-abci.sh（或等价流程）生成 BIYAHOME，且 key 名为 validator（可改 KEY_NAME）
#   - transfer 需要 keyring 里已有名为 recipient 的收款 key（须手动 keys add，勿依赖脚本自动生成）
#   - 已编译：biyachaind、cargo build -p monad-cosmos-integration --bin cosmos-txpool-feed
#
# 用法：
#   export BIYAHOME="${HOME}/.biyachaind"
#   export MONAD_COSMOS_GENESIS_PATH="$BIYAHOME/config/genesis.json"   # 与 monad-node 一致，否则 monad 与 biyachain 状态不是同一条链
#   export MONAD_MEMPOOL_SOCK="/tmp/monad-run/mempool.sock"   # 与 monad-node --mempool-ipc-path 一致
#   ./scripts/cosmos_monad_genesis_tx_example.sh query          # gRPC：注资账户与 recipient 的地址与余额（需 biyachaind）
#   ./scripts/cosmos_monad_genesis_tx_example.sh transfer       # 打印 from/to/数量 与一条上链 feed 命令
#   ./scripts/cosmos_monad_genesis_tx_example.sh diagnose       # 检查 genesis/ABCI/mempool 与文档所列日志要点（feed 成功但链上不变时）
#
# 环境变量（可选）：
#   GRPC_ADDR=127.0.0.1:9900   # biyachaind 默认 gRPC 为 9900（非 Cosmos 常见的 9090）；不设时脚本默认 9900 或读 app.toml
#   ACCOUNT_NUMBER=            # 离线签名；不设时 genesis，transfer 会再用 gRPC 覆盖为链上值
#   SEQUENCE=                  # 离线签名；不设时 transfer 会 gRPC 读链上 sequence（避免与 query 不一致导致拒单）
#   SKIP_LIVE_AUTH=1           # 跳过 gRPC，仅用 genesis 的 ACCOUNT_NUMBER 与 SEQUENCE 回退值
#   AMOUNT=1000000             # 转账数量（仅币数量，不含 denom）

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

BIYACHAIND_BIN="${BIYACHAIND_BIN:-${REPO_ROOT}/biyachain-core/biyachaind}"
BIYAHOME="${BIYAHOME:-${HOME}/.biyachaind}"
GENESIS="${BIYAHOME}/config/genesis.json"
KEY_NAME="${KEY_NAME:-validator}"
KEYRING_BACKEND="${KEYRING_BACKEND:-test}"
AMOUNT="${AMOUNT:-1000000000}"
SEQUENCE="${SEQUENCE:-}"

MONAD_MEMPOOL_SOCK="${MONAD_MEMPOOL_SOCK:-}"
OUT_DIR="${OUT_DIR:-${BIYAHOME}/monad-tx-out}"

die() { echo "error: $*" >&2; exit 1; }

[[ -x "${BIYACHAIND_BIN}" ]] || die "找不到可执行文件 BIYACHAIND_BIN=${BIYACHAIND_BIN}"
[[ -f "$GENESIS" ]] || die "找不到 genesis: $GENESIS（请先 init-standalone-abci.sh）"

CHAIN_ID="$(jq -r '.chain_id' "$GENESIS")"
[[ -n "$CHAIN_ID" && "$CHAIN_ID" != "null" ]] || die "genesis 中无 chain_id"

# 创世注资账户：取 bank balances 第一条（与 init 脚本「单验证人 + add-genesis-account」一致）
FROM_ADDR="$(jq -r '.app_state.bank.balances[0].address // empty' "$GENESIS")"
[[ -n "$FROM_ADDR" ]] || die "无法从 genesis 解析 app_state.bank.balances[0].address"

DENOM="$(jq -r '.app_state.bank.balances[0].coins[0].denom // empty' "$GENESIS")"
[[ -n "$DENOM" ]] || die "无法从 genesis 解析 denom"

# account_number：从 auth 账户里找与 FROM_ADDR 匹配的 BaseAccount（兼容多种 JSON 形态）
if [[ -z "${ACCOUNT_NUMBER:-}" ]]; then
  ACCOUNT_NUMBER="$(
    jq -r --arg a "$FROM_ADDR" '
      .app_state.auth.accounts[]?
      | select(
          (.address // .value.address // .base_account.address) == $a
          or ((.base_account | type == "object") and (.base_account.address == $a))
        )
      | (.account_number // .base_account.account_number // .value.account_number // empty)
    ' "$GENESIS" | head -1
  )"
fi
[[ -n "${ACCOUNT_NUMBER:-}" ]] || die "无法从 genesis 解析 ACCOUNT_NUMBER，请手动 export ACCOUNT_NUMBER=…"

# 从 app.toml 读取 [grpc] enable / address（供提示与默认连接地址）
read_app_toml_grpc() {
  APP_TOML="${BIYAHOME}/config/app.toml"
  GRPC_ENABLE_CFG=""
  GRPC_BIND_ADDR=""
  [[ -f "$APP_TOML" ]] || return 0
  local in_grpc=0
  while IFS= read -r line || [[ -n "$line" ]]; do
    if [[ "$line" =~ ^\[grpc\] ]]; then in_grpc=1; continue; fi
    if [[ "$line" =~ ^\[ ]]; then in_grpc=0; continue; fi
    if [[ $in_grpc -eq 1 ]]; then
      [[ "$line" =~ ^[[:space:]]*enable[[:space:]]*=[[:space:]]*true ]] && GRPC_ENABLE_CFG=1
      [[ "$line" =~ ^[[:space:]]*enable[[:space:]]*=[[:space:]]*false ]] && GRPC_ENABLE_CFG=0
      if [[ "$line" =~ address[[:space:]]*=[[:space:]]*\"([^\"]+)\" ]]; then
        GRPC_BIND_ADDR="${BASH_REMATCH[1]}"
      elif [[ "$line" =~ address[[:space:]]*=[[:space:]]*\'([^\']+)\' ]]; then
        GRPC_BIND_ADDR="${BASH_REMATCH[1]}"
      fi
    fi
  done <"$APP_TOML"
}

# 供 biyachaind query --grpc-addr 使用：监听在 0.0.0.0 时客户端连 127.0.0.1
grpc_client_addr() {
  local a="${1:-}"
  [[ -n "$a" ]] || return 1
  if [[ "$a" == 0.0.0.0:* ]]; then
    echo "127.0.0.1:${a#0.0.0.0:}"
  elif [[ "$a" == \"* ]]; then
    echo "$a" | tr -d '"'
  else
    echo "$a"
  fi
}

# 打印一行地址，随后每行 "  denom amount"（gRPC bank balances JSON）
print_grpc_bank_balances() {
  local addr="$1"
  local try_addr="$2"
  local json
  if ! json="$("${BIYACHAIND_BIN}" query bank balances "$addr" \
    --grpc-addr "$try_addr" --grpc-insecure \
    --home "$BIYAHOME" -o json 2>/dev/null)"; then
    return 1
  fi
  echo "$addr"
  local n
  n="$(echo "$json" | jq '(.balances // []) | length')"
  if [[ "${n:-0}" -eq 0 ]]; then
    echo "  (empty)"
  else
    echo "$json" | jq -r '.balances[] | "  \(.denom) \(.amount)"'
  fi
}

# 发送方 account sequence（链上状态；成功上链一笔后应递增，长期不变则交易可能未执行）
print_grpc_auth_sequence() {
  local addr="$1"
  local try_addr="$2"
  local json
  if ! json="$("${BIYACHAIND_BIN}" query auth account "$addr" \
    --grpc-addr "$try_addr" --grpc-insecure \
    --home "$BIYAHOME" -o json 2>/dev/null)"; then
    return 0
  fi
  local seq
  seq="$(echo "$json" | jq -r '
    .account.value.base_account.sequence //
    .account.base_account.sequence //
    .account.sequence //
    empty
  ')"
  [[ -n "$seq" && "$seq" != "null" ]] || return 0
  echo "from_sequence $seq"
}

query_live_grpc() {
  read_app_toml_grpc
  local try_addr="${GRPC_ADDR:-}"
  if [[ -z "$try_addr" ]]; then
    if [[ -n "${GRPC_BIND_ADDR:-}" ]]; then
      try_addr="$(grpc_client_addr "$GRPC_BIND_ADDR" || true)"
    fi
  fi
  if [[ -z "$try_addr" ]]; then
    try_addr="127.0.0.1:9900"
  fi

  if [[ "${GRPC_ENABLE_CFG:-}" == "0" ]]; then
    echo "gRPC disabled in ${BIYAHOME}/config/app.toml" >&2
    return 0
  fi

  if ! print_grpc_bank_balances "$FROM_ADDR" "$try_addr"; then
    echo "gRPC bank query failed (biyachaind up? ${try_addr})" >&2
    return 0
  fi

  if "${BIYACHAIND_BIN}" keys show recipient --home "$BIYAHOME" --keyring-backend "$KEYRING_BACKEND" &>/dev/null; then
    local rec
    rec="$("${BIYACHAIND_BIN}" keys show recipient -a --home "$BIYAHOME" --keyring-backend "$KEYRING_BACKEND")"
    print_grpc_bank_balances "$rec" "$try_addr" || true
  fi

  print_grpc_auth_sequence "$FROM_ADDR" "$try_addr"
}

# transfer 签名前：与链上 auth 对齐 account_number / sequence（失败则保留 genesis 并回退 SEQUENCE）
sync_auth_from_grpc_for_signing() {
  [[ "${SKIP_LIVE_AUTH:-0}" == "1" ]] && return 0
  read_app_toml_grpc
  [[ "${GRPC_ENABLE_CFG:-}" == "0" ]] && return 0
  local try_addr="${GRPC_ADDR:-}"
  if [[ -z "$try_addr" ]]; then
    if [[ -n "${GRPC_BIND_ADDR:-}" ]]; then
      try_addr="$(grpc_client_addr "$GRPC_BIND_ADDR" || true)"
    fi
  fi
  [[ -z "$try_addr" ]] && try_addr="127.0.0.1:9900"

  local json
  json="$("${BIYACHAIND_BIN}" query auth account "$FROM_ADDR" \
    --grpc-addr "$try_addr" --grpc-insecure \
    --home "$BIYAHOME" -o json 2>/dev/null)" || return 0
  [[ -n "$json" ]] || return 0

  local seq an
  seq="$(echo "$json" | jq -r '.account.value.base_account.sequence // .account.base_account.sequence // empty' 2>/dev/null || true)"
  an="$(echo "$json" | jq -r '.account.value.base_account.account_number // .account.base_account.account_number // empty' 2>/dev/null || true)"
  if [[ -z "${SEQUENCE:-}" ]]; then
    [[ -n "$seq" && "$seq" != "null" ]] && SEQUENCE="$seq"
  fi
  # 勿用「[[ … ]] && ACCOUNT_NUMBER=…」作函数最后一行：[[ 为假时函数返回 1，set -e 会静默退出整个脚本。
  if [[ -n "$an" && "$an" != "null" ]]; then
    ACCOUNT_NUMBER="$an"
  fi
  return 0
}

build_transfer_tx() {
  if ! "${BIYACHAIND_BIN}" keys show "$KEY_NAME" --home "$BIYAHOME" --keyring-backend "$KEYRING_BACKEND" &>/dev/null; then
    die "keyring 里找不到发送方密钥「${KEY_NAME}」: key not found

原因：genesis 里的注资地址来自 init 时生成的 key，但当前 \$BIYAHOME 里没有同名 key。

处理：
  - 确认 export BIYAHOME= 与 init-standalone-abci.sh 时一致（同一目录）
  - 或 export KEY_NAME= 你本机 keys list 里存在的、且对应 genesis 注资地址的 key 名
  - 若换了新 home，需重新 init 并 gentx，或从备份恢复 key"
  fi

  mkdir -p "$OUT_DIR"
  local unsigned="${OUT_DIR}/bank_send_unsigned.json"
  local signed="${OUT_DIR}/bank_send_signed.json"
  local raw="${OUT_DIR}/tx.raw"

  # 收款地址：须事先在 keyring 中创建名为 recipient 的密钥（勿自动 keys add，避免助记词刷到终端）
  if ! "${BIYACHAIND_BIN}" keys show recipient --home "$BIYAHOME" --keyring-backend "$KEYRING_BACKEND" &>/dev/null; then
    die "keyring 里没有 recipient，请先创建后再执行 transfer:

  biyachaind keys add recipient --home \"\$BIYAHOME\" --keyring-backend $KEYRING_BACKEND"
  fi
  local TO_ADDR
  TO_ADDR="$("${BIYACHAIND_BIN}" keys show recipient -a --home "$BIYAHOME" --keyring-backend "$KEYRING_BACKEND")"

  echo "$FROM_ADDR -> $TO_ADDR  ${AMOUNT}${DENOM}"

  sync_auth_from_grpc_for_signing
  if [[ -z "${SEQUENCE:-}" ]]; then
    SEQUENCE="1"
  fi

  echo "sign: account_number=$ACCOUNT_NUMBER sequence=$SEQUENCE BIYAHOME=$BIYAHOME"

  # 使用 key 名（与 genesis 注资对应）；generate-only 写 STDOUT，勿用 -o 当路径（-o 是 text|json）
  "${BIYACHAIND_BIN}" tx bank send "$KEY_NAME" "$TO_ADDR" "${AMOUNT}${DENOM}" \
    --chain-id "$CHAIN_ID" \
    --fees "5000${DENOM}" \
    --gas "200000" \
    --generate-only \
    --home "$BIYAHOME" \
    --keyring-backend "$KEYRING_BACKEND" \
    -o json \
    >"$unsigned" \
    || die "tx bank send --generate-only 失败（检查 chain-id、denom、余额与 key 名）"

  "${BIYACHAIND_BIN}" tx sign "$unsigned" \
    --from "$KEY_NAME" \
    --chain-id "$CHAIN_ID" \
    --home "$BIYAHOME" \
    --keyring-backend "$KEYRING_BACKEND" \
    --offline \
    -a "$ACCOUNT_NUMBER" \
    -s "$SEQUENCE" \
    --output-document "$signed" \
    || die "tx sign 失败（核对 account_number/sequence 是否与 query 的 from_sequence 一致；可 export SKIP_LIVE_AUTH=1 仅用 genesis）"

  # tx encode 输出 base64，需解码为二进制供 cosmos-txpool-feed
  local b64
  b64="$("${BIYACHAIND_BIN}" tx encode "$signed")" || die "tx encode 失败"
  if echo "$b64" | base64 -d >"$raw" 2>/dev/null; then
    :
  else
    base64 --decode <<<"$b64" >"$raw" || die "base64 解码 tx.encode 输出失败"
  fi

  echo "tx.raw -> $raw"
  echo ""
  echo "【必做】仅生成交易不会上链；须执行下面 cargo，把字节送进 monad 的 mempool.sock，再由共识写入 biyachain："

  if [[ -n "${MONAD_MEMPOOL_SOCK:-}" ]]; then
    echo "cargo run -p monad-cosmos-integration --bin cosmos-txpool-feed -- \"$MONAD_MEMPOOL_SOCK\" \"$raw\""
    if [[ ! -S "$MONAD_MEMPOOL_SOCK" ]]; then
      echo "warning: MONAD_MEMPOOL_SOCK 不是有效 unix socket，请与当前 monad-node --mempool-ipc-path 一致" >&2
    fi
  else
    echo "export MONAD_MEMPOOL_SOCK=<与 monad-node --mempool-ipc-path 相同>"
    echo "cargo run -p monad-cosmos-integration --bin cosmos-txpool-feed -- \"$raw\""
  fi
}

case "${1:-}" in
  query)
    query_live_grpc
    ;;
  transfer|tansfer)
    build_transfer_tx
    ;;
  diagnose)
    exec bash "${SCRIPT_DIR}/cosmos_abci_diagnose.sh"
    ;;
  *)
    echo "用法: $0 query | transfer | diagnose"
    exit 1
    ;;
esac
