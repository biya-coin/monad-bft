#!/usr/bin/env bash
set -euo pipefail

PROC_PATTERN='monad-node|biyachaind start|monad-stress-bench.sh rpc|cosmos-txpool-feed serve|monad-rpc'

cleanup_sockets() {
  rm -f .monad/monad-{a,b,c,d}/abci.sock .monad/monad-{a,b,c,d}/mempool.sock
}

pkill -f "$PROC_PATTERN" || true

stopped=false
for _ in {1..40}; do
  if ! pgrep -f "$PROC_PATTERN" >/dev/null 2>&1; then
    stopped=true
    break
  fi
  sleep 0.25
done

if [[ "$stopped" != true ]]; then
  pkill -9 -f "$PROC_PATTERN" || true
fi

cleanup_sockets
./scripts/monad-stress-bench.sh setup-ips
rm -f .monad/monad-{a,b,c,d}/validators.toml
./scripts/monad-stress-bench.sh repair-monad
./scripts/monad-stress-bench.sh reset-consensus

echo "biyachaind/monad/rpc 已停止，配置已修复，数据已重置到 genesis"
