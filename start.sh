#!/usr/bin/env bash
# 开发调试用
set -euo pipefail

bash stop.sh

LOG_DIR="logs"
mkdir -p "$LOG_DIR"

wait_for_socket() {
  local sock="$1"
  local label="$2"
  local attempts="${3:-120}"

  for _ in $(seq 1 "$attempts"); do
    [[ -S "$sock" ]] && return 0
    sleep 0.5
  done

  echo "error: 等待 $label 超时: $sock" >&2
  return 1
}

nohup ./scripts/monad-stress-bench.sh biyachaind a >"$LOG_DIR/biyachaind-a.log" 2>&1 &

nohup ./scripts/monad-stress-bench.sh biyachaind b >"$LOG_DIR/biyachaind-b.log" 2>&1 &

nohup ./scripts/monad-stress-bench.sh biyachaind c >"$LOG_DIR/biyachaind-c.log" 2>&1 &

nohup ./scripts/monad-stress-bench.sh biyachaind d >"$LOG_DIR/biyachaind-d.log" 2>&1 &

wait_for_socket ".monad/monad-a/abci.sock" "biyachaind-a ABCI"
wait_for_socket ".monad/monad-b/abci.sock" "biyachaind-b ABCI"
wait_for_socket ".monad/monad-c/abci.sock" "biyachaind-c ABCI"
wait_for_socket ".monad/monad-d/abci.sock" "biyachaind-d ABCI"

nohup ./scripts/monad-stress-bench.sh monad a >"$LOG_DIR/monad-a.log" 2>&1 &

wait_for_socket ".monad/monad-a/mempool.sock" "monad-a mempool"

sleep 5

nohup ./scripts/monad-stress-bench.sh monad b >"$LOG_DIR/monad-b.log" 2>&1 &

nohup ./scripts/monad-stress-bench.sh monad c >"$LOG_DIR/monad-c.log" 2>&1 &

nohup ./scripts/monad-stress-bench.sh monad d >"$LOG_DIR/monad-d.log" 2>&1 &

wait_for_socket ".monad/monad-b/mempool.sock" "monad-b mempool"
wait_for_socket ".monad/monad-c/mempool.sock" "monad-c mempool"
wait_for_socket ".monad/monad-d/mempool.sock" "monad-d mempool"

nohup ./scripts/monad-stress-bench.sh rpc a >"$LOG_DIR/rpc-a.log" 2>&1 &

nohup ./scripts/monad-stress-bench.sh rpc b >"$LOG_DIR/rpc-b.log" 2>&1 &

nohup ./scripts/monad-stress-bench.sh rpc c >"$LOG_DIR/rpc-c.log" 2>&1 &

nohup ./scripts/monad-stress-bench.sh rpc d >"$LOG_DIR/rpc-d.log" 2>&1 &

echo "biyachaind/monad/rpc x4 已后台启动，日志目录: $LOG_DIR"
