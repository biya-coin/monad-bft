#!/usr/bin/env bash

pkill -9 -f 'target/debug/monad-node' || true
pkill -9 -f 'biyachaind start' || true

# 删除旧的 ABCI socket
rm -f /tmp/biyachain-abci.sock
rm -f /tmp/biyachain-abci-fresh.sock

# 删除之前创建的临时目录（如果有）
rm -rf /tmp/biyachaind-home-*
rm -rf /tmp/monad-final-run-*
rm -rf /tmp/monad-clean-run-*


export BIYAHOME="/home/bryce/vs_workplace/monad-bft/biyachaind"
export BIYACHAIND_BIN="/home/bryce/vs_workplace/monad-bft/biyachain-core/biyachaind"
"/home/bryce/vs_workplace/monad-bft/biyachain-core/scripts/init-standalone-abci.sh"


export MONAD_ABCI_SOCKET="unix:///tmp/biyachain-abci-fresh.sock"
rm -f /tmp/biyachain-abci-fresh.sock
echo "Starting biyachaind..."
"/home/bryce/vs_workplace/monad-bft/biyachain-core/biyachaind" start \
  --home "$BIYAHOME" \
  --with-comet=false \
  --transport socket \
  --address "$MONAD_ABCI_SOCKET" \
  --grpc.enable=true \
  --grpc.address=0.0.0.0:9900 \
  --json-rpc.enable=false \
  --minimum-gas-prices 1byb \
  --log-color=false \
  --log-level info

export MONAD_RUN_DIR="/tmp/monad-final-run-$(date +%s)"
mkdir -p "$MONAD_RUN_DIR/ledger/headers" "$MONAD_RUN_DIR/ledger/bodies" "$MONAD_RUN_DIR/ledger/cosmos-commits"
cp "/home/bryce/vs_workplace/monad-bft/docker/devnet/monad/config/forkpoint.genesis.toml" "$MONAD_RUN_DIR/forkpoint.toml"
cp "/home/bryce/vs_workplace/monad-bft/docker/devnet/monad/config/validators.toml" "$MONAD_RUN_DIR/validators.toml"

# 建议确认这两个文件真的存在
test -f "$MONAD_RUN_DIR/forkpoint.toml"
test -f "$MONAD_RUN_DIR/validators.toml"

echo "Starting monad-node..."
cd "/home/bryce/vs_workplace/monad-bft"
export MONAD_ABCI_ENDPOINT="${MONAD_ABCI_SOCKET:-unix:///tmp/biyachain-abci-fresh.sock}"
export MONAD_COSMOS_GENESIS_PATH="$BIYAHOME/config/genesis.json"
export RUST_LOG=info







# Hugepages allocation
sudo sysctl -w vm.nr_hugepages=2048
# UDP buffer sizes
sudo sysctl -w net.core.rmem_max=62500000
sudo sysctl -w net.core.rmem_default=62500000
sudo sysctl -w net.core.wmem_max=62500000
sudo sysctl -w net.core.wmem_default=62500000
# TCP buffer sizes
sudo sysctl -w net.ipv4.tcp_rmem='4096 12582912 12582912'
sudo sysctl -w net.ipv4.tcp_wmem='4096 12582912 12582912'




RUST_LOG=info cargo run -p monad-node --bin monad-node -- \
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
  --persisted-peers-path "$MONAD_RUN_DIR/peers.json" 

echo "Monad-node started. Logs: monad_out.log"
echo "Biyachaind logs: biyachaind.log"