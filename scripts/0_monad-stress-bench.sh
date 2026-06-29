#!/usr/bin/env bash
# Monad-BFT + chain-stresser 压测编排脚本
#
# 用法:
#   ./scripts/monad-stress-bench.sh setup     # mult-run 初始化（账户 + genesis + monad 配置）
#   ./scripts/monad-stress-bench.sh build       # 最小集：monad-node + keystore + cosmos-txpool-feed
#   ./scripts/monad-stress-bench.sh build-full  # 可选：含 monad-rpc（build 已含 26657 serve）
#   ./scripts/monad-stress-bench.sh run        # 打印本机四节点启动说明（推荐，无需 Docker 镜像）
#   ./scripts/monad-stress-bench.sh biyachaind [a-d]
#   ./scripts/monad-stress-bench.sh monad [a-d]
#   ./scripts/monad-stress-bench.sh start      # 一键启动：清历史数据 → genesis 初始化 → 四节点后台运行
#   ./scripts/monad-stress-bench.sh restart    # 同 start（每次均清数据重启）
#   ./scripts/monad-stress-bench.sh stop       # 停止四节点并清理 socket/pid
#   BENCH_KEEP_DATA=1 ./scripts/... start      # 保留链数据，仅重启进程（调试用）
#   ./scripts/monad-stress-bench.sh up        # 可选：docker compose（需镜像，不推荐）
#   ./scripts/monad-stress-bench.sh stress    # chain-stresser tx-bank-send（可环境变量覆盖）
#   ./scripts/monad-stress-bench.sh verify    # gRPC 查压测账户 sequence
#   ./scripts/monad-stress-bench.sh rpc       # cosmos-txpool-feed serve :26657（单节点 test_run 用）
#   ./scripts/monad-stress-bench.sh down      # docker compose down
#   ./scripts/monad-stress-bench.sh status    # 检查端口与容器
#
# 环境变量（stress 子命令）:
#   STRESS_ACCOUNTS_NUM  压测账户数（setup 时，默认 1000）
#   STRESS_ACCOUNTS      accounts.json 路径
#   STRESS_CMD           bank | spot-limit（默认 bank）
#   STRESS_ACCOUNTS_NUM_RUN  实际参与压测账户数（默认 50）
#   STRESS_TRANSACTIONS  每账户交易数（默认 20）
#   STRESS_RATE_TPS      限速 TPS（默认 30）
#   STRESS_NODE_ADDR     RPC（默认 127.0.0.1:26657）
#   STRESS_GRPC_ADDR     gRPC（默认 127.0.0.1:19900）
#   STRESS_SHARDS        多节点压测分片数 1..4（shard/rpc-all/stress-all，默认 4）
#   CHAIN_ID             默认 biyachain-1
#   NODE_LOG_DIR         节点日志目录（start/stop，默认 ./node-log）
#   BENCH_KEEP_DATA      start 时设为 1 可保留 WAL/ledger/biyachaind data（默认每次清干净）
#   BENCH_NO_RPC         start 时设为 1 可跳过 cosmos-txpool-feed（默认启动 4 路 RPC）
#
# 多节点压测（发到 4 个节点，提高 TPS）:
#   1) ./scripts/monad-stress-bench.sh shard      # 账户切 4 份（避免 nonce 冲突）
#   2) ./scripts/monad-stress-bench.sh rpc-all    # 后台起 4 个 feed（26657/26667/26677/26687）
#   3) STRESS_RATE_TPS=5000 ./scripts/monad-stress-bench.sh stress-all  # 4 路并发
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="${MONAD_BFT_ROOT:-$(cd "$SCRIPT_DIR/.." && pwd)}"
WORK="${MONAD_WORK:-$REPO/.monad}"
MONAD_NODE_BIN="${MONAD_NODE_BIN:-$REPO/target/release/monad-node}"
COSMOS_TXPOOL_FEED_BIN="${COSMOS_TXPOOL_FEED_BIN:-$REPO/target/release/cosmos-txpool-feed}"

# 压测集群：四验证者 a/b/c/d
MONAD_BENCH_NODES=(a b c d)

BIYACHAIND_BIN="${BIYACHAIND_BIN:-$REPO/biyachain-core/bin/biyachaind}"
CHAIN_ID="${CHAIN_ID:-biyachain-1}"
STRESS_ACCOUNTS_NUM="${STRESS_ACCOUNTS_NUM:-1000}"
STRESS_ACCOUNTS="${STRESS_ACCOUNTS:-$WORK/instances/0/accounts.json}"
STRESS_CMD="${STRESS_CMD:-spot-limit}"
# 目标：每个区块约10000笔交易
# 可以根据区块时间（通常是2s）这样估算：STRESS_RATE_TPS * 块时间 = 每块交易数
# 例如 TPS=5000，每2s出块 ~= 10000 txs/块
STRESS_ACCOUNTS_NUM_RUN="${STRESS_ACCOUNTS_NUM_RUN:-1000}"      # 更多账号可减少 nonce 瓶颈
STRESS_TRANSACTIONS="${STRESS_TRANSACTIONS:-1000}"               # 每账号交易数，可选更大
STRESS_RATE_TPS="${STRESS_RATE_TPS:-1200}"                      # 每秒交易数，根据目标动态调整
STRESS_NODE_ADDR="${STRESS_NODE_ADDR:-127.0.0.1:26657}"
STRESS_GRPC_ADDR="${STRESS_GRPC_ADDR:-127.0.0.1:19900}"
MIN_GAS_PRICE="${MIN_GAS_PRICE:-1byb}"
# build 默认清 release 产物再编译，避免 monad-node 仍链接旧的 monad-chain-config 等 crate
BUILD_NO_CACHE="${BUILD_NO_CACHE:-0}"

# SeiDB SS（state store）/ SC（state commitment）启动参数
# 默认启用 SeiDB（SC=memiavl committer.db / SS=pebbledb）。monad standalone bootstrap 兼容性
# 由 app 层处理：executor 等 InitChain、ProcessProposal 首块前不重置 finalize 状态、executor 与
# ABCI 串行化、monad 结果内存缓存（绕过 SeiDB 提交可见性滞后）。回退 iavl：STORE_BACKEND=iavl SEIDB_ENABLED=false
STORE_BACKEND="${STORE_BACKEND:-seidb}"
SEIDB_ENABLED="${SEIDB_ENABLED:-true}"
SEIDB_SC_BACKEND="${SEIDB_SC_BACKEND:-memiavl}"
SEIDB_SS_ENABLE="${SEIDB_SS_ENABLE:-true}"
SEIDB_SS_BACKEND="${SEIDB_SS_BACKEND:-pebbledb}"
SEIDB_KEEP_RECENT="${SEIDB_KEEP_RECENT:-0}"
SEIDB_HOME="${SEIDB_HOME:-}"
# memiavl 异步提交缓冲：<=0 表示同步提交（最新块立即可读，monad 延时执行查询需要）
SEIDB_MEMIAVL_ASYNC="${SEIDB_MEMIAVL_ASYNC:-0}"

SPOT_MARKET_ID="${SPOT_MARKET_ID:-0xb322bce686ec25364be50728812e33741da1d82e9c91c2c89b91b91d26b0e9c5}"
# start/stop 日志与 pid 文件目录（相对当前工作目录）
NODE_LOG_DIR="${NODE_LOG_DIR:-./node-log}"
# 多主机：remote 跳过 setup-ips；BENCH_NODE_ROLE=a|b|c|d 时只操作单节点
BENCH_P2P_MODE="${BENCH_P2P_MODE:-local}"
BENCH_NODE_ROLE="${BENCH_NODE_ROLE:-}"
BENCH_RPC_BIND="${BENCH_RPC_BIND:-127.0.0.1}"
# 多主机每机一节点时各机端口相同，由 Ansible 注入（见 /etc/monad-bench/env）
BENCH_GRPC_PORT="${BENCH_GRPC_PORT:-}"
BENCH_COMET_PORT="${BENCH_COMET_PORT:-}"
BENCH_METRICS_PORT="${BENCH_METRICS_PORT:-}"

usage() {
  cat <<EOF
用法: $0 <command>

  setup       mult-run 初始化（账户 + genesis + monad 配置）
  build       最小编译：monad-node + keystore + cosmos-txpool-feed（默认 BUILD_NO_CACHE=1 全量重编）
  build-full  可选：含 monad-rpc（26657 已由 build 的 cosmos-txpool-feed serve 提供）
  run         打印本机原生启动步骤（推荐，无需 monad-node Docker 镜像）
  start       一键启动四节点 + RPC feed（清数据/genesis 初始化，日志 ./node-log/）
  start-node  启动单节点（BENCH_NODE_ROLE=a|b|c|d，多主机/Ansible 用）
  restart     同 start
  stop        停止四节点进程并清理 socket/pid 残留
  setup-ips   配置四节点 P2P loopback IP（sudo，一次性；start 会自动调用）
  biyachaind  前台启动 biyachaind（默认节点 a）
  monad       前台启动 monad-node（默认节点 a）
  repair-monad  重建 node.toml P2P 签名（密钥与 node.toml 不一致时用）
  reset-consensus  清 WAL/ledger 并恢复 genesis forkpoint（共识卡住时用）
  diagnose       检查高度 4 卡点（cosmos-commits / forkpoint / ABCI）
  rpc [a-d]   前台启动 cosmos-txpool-feed serve（默认 a :26657；b/c/d → :26667/26677/26687）
  rpc-all     后台一键启动每节点一个 feed（多节点压测入口）
  shard       账户分片：accounts.json → accounts.{a..d}.json（多节点压测前置）
  stress-all  4 路分片并发压测到 4 个节点（总吞吐 ~ shards*STRESS_RATE_TPS）
  up          可选：docker compose up（需 monad-node:local 镜像）
  down        docker compose down
  nodes       四节点 biyachaind/monad/P2P 状态一览
  status      端口探测（及 compose 状态）
  stress      chain-stresser 压测（默认 bank send）
  verify      gRPC 查压测账户 sequence

文档: docs/monad-bench-quickstart.md（快速上手）  docs/monad-chain-stresser-bench.md（详细）
EOF
}

die() {
  echo "error: $*" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "未找到命令: $1"
}

build_no_cache_enabled() {
  case "${BUILD_NO_CACHE:-1}" in
    0|false|no|FALSE|NO|off|OFF) return 1 ;;
    *) return 0 ;;
  esac
}

# 清 release 产物并关闭增量编译，保证 vote_pace 等编译期常量会进入 monad-node。
prepare_fresh_release_build() {
  if ! build_no_cache_enabled; then
    echo "==> BUILD_NO_CACHE=0，保留 cargo release 增量缓存"
    return 0
  fi
  export CARGO_INCREMENTAL=0
  echo "==> 强制全量重编 release（BUILD_NO_CACHE=1 → cargo clean --release, CARGO_INCREMENTAL=0）"
  (cd "$REPO" && cargo clean --release)
}

print_release_bin_times() {
  local f
  for f in monad-node monad-keystore cosmos-txpool-feed sign-name-record; do
    if [[ -f "$REPO/target/release/$f" ]]; then
      echo "  $f: $(stat -c '%y' "$REPO/target/release/$f" 2>/dev/null || stat -f '%Sm' "$REPO/target/release/$f")"
    fi
  done
  if [[ -x "$BIYACHAIND_BIN" ]]; then
    echo "  biyachaind: $(stat -c '%y' "$BIYACHAIND_BIN" 2>/dev/null || stat -f '%Sm' "$BIYACHAIND_BIN")"
  fi
}

ensure_bindgen_clang_env() {
  # Ubuntu 22.04 默认 clang-14 不支持 bindgen 里的 -std=c23。
  # gnu2x 足够编 monad-node；monad-rpc 依赖的 exec-events 头文件含 C23 bool/constexpr，需 clang-19。
  if [[ -z "${BINDGEN_EXTRA_CLANG_ARGS:-}" ]]; then
    if command -v clang-19 >/dev/null 2>&1; then
      export LIBCLANG_PATH="${LIBCLANG_PATH:-/usr/lib/llvm-19/lib}"
    elif [[ -d /usr/lib/llvm-14/lib ]]; then
      export LIBCLANG_PATH="${LIBCLANG_PATH:-/usr/lib/llvm-14/lib}"
      export BINDGEN_EXTRA_CLANG_ARGS="-std=gnu2x"
      echo "note: clang-14 — 已设 BINDGEN_EXTRA_CLANG_ARGS=-std=gnu2x（仅够 build，不含 monad-rpc）"
    fi
  fi
}

ensure_execution_build_env() {
  export TRIEDB_TARGET="${TRIEDB_TARGET:-triedb_driver}"
  export ASMFLAGS="${ASMFLAGS:--march=haswell}"
  export CFLAGS="${CFLAGS:--march=haswell}"
  export CXXFLAGS="${CXXFLAGS:--march=haswell}"
}

ensure_c23_compiler_env() {
  # monad-event-ring 的 CMake 用 c_std_23（bool/nullptr）；系统默认 cc（gcc-11）不支持。
  if [[ -n "${CC:-}" && -n "${CXX:-}" ]]; then
    return
  fi
  if command -v clang-19 >/dev/null 2>&1 && command -v clang++-19 >/dev/null 2>&1; then
    export CC=clang-19
    export CXX=clang++-19
  elif command -v gcc-15 >/dev/null 2>&1 && command -v g++-15 >/dev/null 2>&1; then
    export CC=gcc-15
    export CXX=g++-15
  fi
}

ensure_cmake_env() {
  # 系统 cmake 3.22 不满足 monad-event-ring（需 ≥3.23）；优先用 pip 安装的 cmake。
  if [[ -x "${HOME}/.local/bin/cmake" ]]; then
    export PATH="${HOME}/.local/bin:${PATH}"
  fi
  local ver
  ver="$(cmake --version 2>/dev/null | awk '/version/ {print $3; exit}')"
  if [[ -n "$ver" ]]; then
    local major minor
    IFS=. read -r major minor _ <<<"$ver"
    if (( major < 3 || (major == 3 && minor < 23) )); then
      echo "note: cmake $ver < 3.23，尝试 pip install --user 'cmake>=3.23' ..."
      pip3 install --user 'cmake>=3.23' >/dev/null 2>&1 || true
      [[ -x "${HOME}/.local/bin/cmake" ]] && export PATH="${HOME}/.local/bin:${PATH}"
    fi
  fi
}

have_deb_pkg() {
  dpkg -s "$1" >/dev/null 2>&1
}

have_boost_183() {
  # monad_execution 需 Boost 1.83+（含 json 组件）；Ubuntu 22.04 默认仅 1.74
  if have_deb_pkg libboost1.83-dev; then
    return 0
  fi
  local cfg
  for cfg in \
    /usr/lib/x86_64-linux-gnu/cmake/Boost-1.83.0/BoostConfig.cmake \
    /usr/lib/cmake/Boost-1.83.0/BoostConfig.cmake; do
    [[ -f "$cfg" ]] && return 0
  done
  return 1
}

check_build_full_prereqs() {
  local missing=0
  ensure_cmake_env
  ver="$(cmake --version 2>/dev/null | awk '/version/ {print $3; exit}')"
  if [[ -n "$ver" ]]; then
    IFS=. read -r major minor _ <<<"$ver"
    if (( major < 3 || (major == 3 && minor < 23) )); then
      echo "error: cmake $ver < 3.23（pip3 install --user 'cmake>=3.23'）" >&2
      missing=1
    fi
  fi
  if ! command -v clang-19 >/dev/null 2>&1; then
    echo "error: 未找到 clang-19。monad-rpc 的 bindgen 需要 C23（bool/constexpr），clang-14 不够。" >&2
    echo "       安装: sudo bash monad-execution/scripts/ubuntu-build/install-tools.sh" >&2
    missing=1
  fi
  if ! have_deb_pkg libbsd-dev; then
    echo "error: 缺少 libbsd-dev（Ubuntu 22.04 上 strlcpy）→ sudo apt install libbsd-dev" >&2
    missing=1
  fi
  if ! have_deb_pkg libgtest-dev; then
    echo "error: 缺少 libgtest-dev（monad_execution 构建）→ sudo apt install libgtest-dev libgmock-dev" >&2
    missing=1
  fi
  if ! have_deb_pkg libzstd-dev; then
    echo "error: 缺少 libzstd-dev → sudo apt install libzstd-dev" >&2
    missing=1
  fi
  if ! have_deb_pkg libhugetlbfs-dev; then
    echo "error: 缺少 libhugetlbfs-dev → sudo apt install libhugetlbfs-dev" >&2
    missing=1
  fi
  if ! have_boost_183; then
    echo "warn: 未检测到 Boost 1.83（Ubuntu 22.04 默认 1.74 不含 boost::json）" >&2
    echo "      monad-rpc 将尝试 Docker 编译；原生编译需 Ubuntu 24.04+ 或:" >&2
    echo "      sudo bash monad-execution/scripts/ubuntu-build/install-boost.sh" >&2
  fi
  if (( missing != 0 )); then
    echo "" >&2
    echo "或一次性: sudo bash monad-execution/scripts/ubuntu-build/install-tools.sh && sudo bash monad-execution/scripts/ubuntu-build/install-deps.sh" >&2
    echo "仅 monad-node 压测可先: $0 build" >&2
    exit 1
  fi
}

print_build_hints() {
  echo "" >&2
  echo "构建失败常见原因（见 docs/monad-chain-stresser-bench.md#构建故障对照表）：" >&2
  echo "  1. cmake < 3.23  → pip3 install --user 'cmake>=3.23' 且 export PATH=\$HOME/.local/bin:\$PATH" >&2
  echo "  2. libbsd-dev / libzstd-dev / libhugetlbfs-dev → sudo apt install libbsd-dev libzstd-dev libhugetlbfs-dev" >&2
  echo "  3. monad-rpc 需 clang-19（C23 bool/constexpr）→ install-tools.sh" >&2
  echo "  4. monad-event-ring CMake 勿用 gcc-11：需 CC=clang-19 或 gcc-15（build-full 已自动设置）" >&2
  echo "  5. Could not find Boost / boost::json | monad_execution | Ubuntu 22.04: $0 build-rpc-docker" >&2
  echo "  6. 仅压测 monad-node 可先: $0 build（不含 monad-rpc）" >&2
}

cmd_build_rpc_docker() {
  need_cmd docker
  local base_tag="${MONAD_RPC_BASE_IMAGE:-monad-rpc-base:local}"
  local git_ver="${GIT_TAG_VERSION:-dev-local}"
  local cargo_home="${CARGO_HOME:-$HOME/.cargo}"
  local rustup_home="${RUSTUP_HOME:-$HOME/.rustup}"
  [[ -d "$cargo_home" ]] || die "未找到 Rust cargo 目录: $cargo_home"
  [[ -d "$rustup_home" ]] || die "未找到 Rust rustup 目录: $rustup_home"

  echo "==> 构建 monad-rpc 编译环境（ubuntu:25.04 + Boost 1.83）"
  local -a docker_build_args=(--network=host -f "$REPO/docker/rpc/Dockerfile" --target base -t "$base_tag" "$REPO")
  if build_no_cache_enabled; then
    docker_build_args=(--no-cache "${docker_build_args[@]}")
  fi
  docker build "${docker_build_args[@]}"

  echo "==> 容器内编译 monad-rpc（复用宿主机 Rust，避免 sh.rustup.rs DNS 问题）"
  mkdir -p "$REPO/target/release"
  local -a docker_env=(
    -e "TRIEDB_TARGET=triedb_driver"
    -e "MONAD_VERSION=$git_ver"
    -e "BUILD_NO_CACHE=${BUILD_NO_CACHE:-1}"
    -e "RUSTUP_HOME=/root/.rustup"
    -e "CARGO_HOME=/root/.cargo"
    -e "PATH=/root/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
  )
  if [[ -n "${HTTP_PROXY:-}" ]]; then
    docker_env+=( -e "HTTP_PROXY=$HTTP_PROXY" -e "http_proxy=$HTTP_PROXY" )
  fi
  if [[ -n "${HTTPS_PROXY:-}" ]]; then
    docker_env+=( -e "HTTPS_PROXY=$HTTPS_PROXY" -e "https_proxy=$HTTPS_PROXY" )
  fi

  docker run --rm --network=host \
    -v "$REPO:/usr/src/monad-bft" \
    -v "$cargo_home:/root/.cargo" \
    -v "$rustup_home:/root/.rustup" \
    "${docker_env[@]}" \
    -w /usr/src/monad-bft \
    "$base_tag" \
    bash -c 'set -euo pipefail
export ASMFLAGS="-march=haswell"
export CFLAGS="-march=haswell -fno-omit-frame-pointer"
export CXXFLAGS="-march=haswell -fno-omit-frame-pointer"
export RUSTFLAGS="-C target-cpu=haswell -C force-frame-pointers=yes"
export CC=gcc-15 CXX=g++-15
case "${BUILD_NO_CACHE:-1}" in
  0|false|no|FALSE|NO|off|OFF) ;;
  *)
    export CARGO_INCREMENTAL=0
    cargo clean --release
    ;;
esac
cargo build --release -p monad-rpc
triedb_so="$(find target/release -name "libtriedb_driver.so" -type f 2>/dev/null | head -1)"
if [[ -n "$triedb_so" ]]; then
  cp -a "$triedb_so" target/release/
  ldd "$triedb_so" | python3 /usr/src/monad-bft/docker/filter-dependent-shared-objects.py | while read -r lib; do
    [[ -f "$lib" ]] && cp -a "$lib" target/release/
  done
fi
find target/release -maxdepth 4 -name "libmonad_execution.so" -type f -exec cp -a {} target/release/ \; 2>/dev/null || true'

  chmod +x "$REPO/target/release/monad-rpc" 2>/dev/null || true
  [[ -f "$REPO/target/release/monad-rpc" ]] || die "Docker 编译未产出 monad-rpc"
  ln -sf monad-keystore "$REPO/target/release/keystore" 2>/dev/null || true
  echo "build-rpc-docker done: $REPO/target/release/monad-rpc"
}

cmd_build() {
  prepare_fresh_release_build
  echo "==> build biyachaind"
  if build_no_cache_enabled; then
    (cd "$REPO/biyachain-core" && go build -a -o bin/biyachaind ./cmd/biyachaind)
  else
    (cd "$REPO/biyachain-core" && go build -o bin/biyachaind ./cmd/biyachaind)
  fi
  ensure_bindgen_clang_env
  ensure_cmake_env
  ensure_c23_compiler_env
  ensure_execution_build_env
  echo "==> build monad-node / keystore / cosmos-txpool-feed"
  # 多个 -p 与 --bin 混用时 cargo 只编指定 bin，不会产出 monad-node / monad-keystore
  if ! (cd "$REPO" && \
      cargo build --release -p monad-node -p monad-keystore && \
      cargo build --release -p monad-peer-discovery --bin sign-name-record && \
      cargo build --release -p monad-txpool --bin cosmos-txpool-feed); then
    print_build_hints
    exit 1
  fi
  # compose 挂载 target/release/keystore，cargo 产物名为 monad-keystore
  ln -sf monad-keystore "$REPO/target/release/keystore"
  [[ -f "$REPO/target/release/monad-node" ]] || die "monad-node 未生成，请检查编译日志"
  echo "build done（cosmos-txpool-feed serve :26657，可替代 monad-rpc 给 chain-stresser）"
  print_release_bin_times
}

cmd_build_full() {
  check_build_full_prereqs
  cmd_build
  if ! have_boost_183; then
    cmd_build_rpc_docker
    echo "build-full done（monad-rpc 经 Docker 编译）。"
    return
  fi
  # monad-rpc 必须用 clang-19 + C23，勿沿用 clang-14 的 gnu2x 覆盖
  unset BINDGEN_EXTRA_CLANG_ARGS
  export LIBCLANG_PATH="${LIBCLANG_PATH:-/usr/lib/llvm-19/lib}"
  ensure_cmake_env
  ensure_c23_compiler_env
  ensure_execution_build_env
  echo "==> build monad-rpc (LIBCLANG_PATH=$LIBCLANG_PATH CC=${CC:-cc} CXX=${CXX:-c++} TRIEDB_TARGET=$TRIEDB_TARGET)"
  if ! (cd "$REPO" && cargo build --release -p monad-rpc); then
    print_build_hints
    exit 1
  fi
  ln -sf monad-keystore "$REPO/target/release/keystore"
  echo "build-full done."
  print_release_bin_times
  [[ -f "$REPO/target/release/monad-rpc" ]] && \
    echo "  monad-rpc: $(stat -c '%y' "$REPO/target/release/monad-rpc" 2>/dev/null || stat -f '%Sm' "$REPO/target/release/monad-rpc")"
}

need_setup_build() {
  [[ ! -x "$BIYACHAIND_BIN" ]] && return 0
  [[ ! -x "$REPO/target/release/monad-keystore" ]] && return 0
  [[ ! -x "$REPO/target/release/sign-name-record" ]] && return 0
  return 1
}

cmd_setup() {
  need_cmd jq
  if need_setup_build; then
    cmd_build
  fi
  export MONAD_BFT_ROOT="$REPO"
  export WORK
  export STRESS_ACCOUNTS_NUM
  export BIYACHAIND_BIN
  "$REPO/scripts/mult-run.sh"
  apply_monad_sysctl
  echo ""
  echo "setup 完成。工作目录: $WORK"
  echo "压测账户: $STRESS_ACCOUNTS"
  echo "下一步: $0 run   # 按说明本机启动节点后 stress"
}

node_grpc_port() {
  if [[ -n "$BENCH_GRPC_PORT" ]]; then
    echo "$BENCH_GRPC_PORT"
    return
  fi
  case "$1" in
    a) echo 19900 ;;
    b) echo 29900 ;;
    c) echo 39900 ;;
    d) echo 49900 ;;
    *) die "未知节点: $1（a|b|c|d）" ;;
  esac
}

node_metrics_port() {
  if [[ -n "$BENCH_METRICS_PORT" ]]; then
    echo "$BENCH_METRICS_PORT"
    return
  fi
  # 与 biyachain-core/monitor/prometheus/prometheus.yml 的 target 端口一致
  case "$1" in
    a) echo 26660 ;;
    b) echo 26760 ;;
    c) echo 26860 ;;
    d) echo 26960 ;;
    *) die "未知节点: $1（a|b|c|d）" ;;
  esac
}

node_comet_port() {
  if [[ -n "$BENCH_COMET_PORT" ]]; then
    echo "$BENCH_COMET_PORT"
    return
  fi
  case "$1" in
    a) echo 26657 ;;
    b) echo 26667 ;;
    c) echo 26677 ;;
    d) echo 26687 ;;
    *) die "未知节点: $1（a|b|c|d）" ;;
  esac
}

bench_p2p_remote_enabled() {
  case "${BENCH_P2P_MODE:-local}" in
    remote|Remote|REMOTE) return 0 ;;
    *) return 1 ;;
  esac
}

node_monad_ip_local() {
  case "$1" in
    a) echo 172.28.0.10 ;;
    b) echo 172.28.0.20 ;;
    c) echo 172.28.0.30 ;;
    d) echo 172.28.0.40 ;;
    *) die "未知节点: $1（a|b|c|d）" ;;
  esac
}

# 多主机：读 MONAD_P2P_HOST_<n>；本机：172.28.0.x
node_monad_ip() {
  local n="$1" var="MONAD_P2P_HOST_${n}"
  if [[ -n "${!var:-}" ]]; then
    echo "${!var}"
    return
  fi
  if bench_p2p_remote_enabled; then
    die "remote 模式需设置 MONAD_P2P_HOST_$n（Ansible inventory 或 export）"
  fi
  node_monad_ip_local "$n"
}

bench_target_nodes() {
  local n
  if [[ -n "$BENCH_NODE_ROLE" ]]; then
    case "$BENCH_NODE_ROLE" in
      a|b|c|d) echo "$BENCH_NODE_ROLE" ;;
      *) die "无效 BENCH_NODE_ROLE=$BENCH_NODE_ROLE（a|b|c|d）" ;;
    esac
    return
  fi
  for n in "${MONAD_BENCH_NODES[@]}"; do
    echo "$n"
  done
}

node_stress_rpc_addr() {
  local n="$1" var="STRESS_NODE_ADDR_${n}"
  if [[ -n "${!var:-}" ]]; then
    echo "${!var}"
    return
  fi
  echo "${BENCH_RPC_BIND:-127.0.0.1}:$(node_comet_port "$n")"
}

node_stress_grpc_addr() {
  local n="$1" var="STRESS_GRPC_ADDR_${n}"
  if [[ -n "${!var:-}" ]]; then
    echo "${!var}"
    return
  fi
  local host
  host="$(node_monad_ip "$n")"
  echo "${host}:$(node_grpc_port "$n")"
}

ensure_stress_workdir() {
  [[ -d "$WORK" ]] || die "未找到 $WORK，请先: $0 setup"
}

ensure_genesis_reference() {
  if [[ -f "$WORK/genesis.json.reference" ]]; then
    return 0
  fi
  if [[ -d "$WORK/genesis.json.reference" ]]; then
    rm -rf "$WORK/genesis.json.reference"
  fi
  local src="$WORK/biyachain-home-a/config/genesis.json"
  if [[ -f "$src" ]]; then
    cp -a "$src" "$WORK/genesis.json.reference"
    echo "note: 已从 $src 生成 $WORK/genesis.json.reference"
    return 0
  fi
  die "未找到 $WORK/genesis.json.reference，请先: $0 setup"
}

ensure_node_dirs() {
  local n="$1"
  local need_genesis="${2:-}"
  ensure_stress_workdir
  [[ -d "$WORK/biyachain-home-$n" ]] || die "未找到 $WORK/biyachain-home-$n，请先: $0 setup"
  [[ -d "$WORK/monad-$n" ]] || die "未找到 $WORK/monad-$n，请先: $0 setup"
  if [[ -n "$need_genesis" ]]; then
    ensure_genesis_reference
  fi
}

biyachain_lib_path() {
  if [[ -d "$WORK/biyachain-lib" ]]; then
    echo "$WORK/biyachain-lib"
  elif [[ -d "$REPO/biyachain-lib" ]]; then
    echo "$REPO/biyachain-lib"
  fi
}

apply_monad_sysctl() {
  if bench_skip_sysctl_enabled; then
    echo "note: BENCH_SKIP_SYSCTL=1，跳过 monad-node sysctl（大页/UDP buffer）"
    return 0
  fi
  if ! command -v sudo >/dev/null 2>&1; then
    echo "note: 无 sudo，跳过 monad-node sysctl（大页/UDP buffer）"
    return 0
  fi
  # 非交互 sudo，避免 Ansible/CI 在密码提示处永久阻塞
  sudo -n sysctl -w vm.nr_hugepages=2048 >/dev/null 2>&1 || true
  sudo -n sysctl -w net.core.rmem_max=62500000 >/dev/null 2>&1 || true
  sudo -n sysctl -w net.core.rmem_default=62500000 >/dev/null 2>&1 || true
  sudo -n sysctl -w net.core.wmem_max=62500000 >/dev/null 2>&1 || true
  sudo -n sysctl -w net.core.wmem_default=62500000 >/dev/null 2>&1 || true
  sudo -n sysctl -w net.ipv4.tcp_rmem='4096 12582912 12582912' >/dev/null 2>&1 || true
  sudo -n sysctl -w net.ipv4.tcp_wmem='4096 12582912 12582912' >/dev/null 2>&1 || true
}

node_log_dir_abs() {
  local d="$NODE_LOG_DIR"
  mkdir -p "$d"
  (cd "$d" && pwd)
}

kill_pid_gracefully() {
  local pid="$1" label="$2"
  [[ -n "$pid" ]] || return 0
  kill -0 "$pid" 2>/dev/null || return 0
  kill "$pid" 2>/dev/null || true
  local i=0
  while kill -0 "$pid" 2>/dev/null && (( i < 10 )); do
    sleep 0.5
    i=$((i + 1))
  done
  if kill -0 "$pid" 2>/dev/null; then
    kill -9 "$pid" 2>/dev/null || true
    echo "  强制结束 $label (pid=$pid)"
  fi
}

bench_svc_pid_alive() {
  local svc="$1" n="$2" logdir pf pid
  logdir="$(node_log_dir_abs)"
  pf="$logdir/${svc}-${n}.pid"
  [[ -f "$pf" ]] || return 1
  pid="$(tr -d '[:space:]' <"$pf" 2>/dev/null || true)"
  [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null
}

bench_all_nodes_running() {
  local n
  for n in "${MONAD_BENCH_NODES[@]}"; do
    bench_svc_pid_alive biyachaind "$n" || return 1
    bench_svc_pid_alive monad "$n" || return 1
  done
  return 0
}

stop_bench_node_processes() {
  local n logdir pf svc pid
  logdir="$(node_log_dir_abs)"
  if [[ -n "$BENCH_NODE_ROLE" ]]; then
    echo "==> 停止节点 $BENCH_NODE_ROLE"
  else
    echo "==> 停止四节点进程"
  fi
  while IFS= read -r n; do
    for svc in biyachaind monad feed; do
      pf="$logdir/${svc}-${n}.pid"
      if [[ -f "$pf" ]]; then
        pid="$(tr -d '[:space:]' <"$pf" 2>/dev/null || true)"
        kill_pid_gracefully "$pid" "${svc}-${n}"
        rm -f "$pf"
      fi
    done
    pkill -f "biyachaind start --home ${WORK}/biyachain-home-${n}" 2>/dev/null || true
    pkill -f "${WORK}/monad-${n}/id-secp" 2>/dev/null || true
  done < <(bench_target_nodes)
  if [[ -z "$BENCH_NODE_ROLE" ]]; then
    pkill -f 'cosmos-txpool-feed serve' 2>/dev/null || true
  fi
  sleep 1
  while IFS= read -r n; do
    pkill -9 -f "biyachaind start --home ${WORK}/biyachain-home-${n}" 2>/dev/null || true
    pkill -9 -f "${WORK}/monad-${n}/id-secp" 2>/dev/null || true
  done < <(bench_target_nodes)
  if [[ -z "$BENCH_NODE_ROLE" ]]; then
    pkill -9 -f 'cosmos-txpool-feed serve' 2>/dev/null || true
  fi
}

cleanup_bench_stale_files() {
  local n monad_dir logdir
  logdir="$(node_log_dir_abs)"
  echo "==> 清理 socket / pid 残留"
  while IFS= read -r n; do
    monad_dir="$WORK/monad-$n"
    rm -f "$monad_dir/abci.sock" "$monad_dir/mempool.sock" \
      "$monad_dir/controlpanel.sock" "$monad_dir/statesync.sock"
  done < <(bench_target_nodes)
  if [[ -z "$BENCH_NODE_ROLE" ]]; then
    rm -f "$logdir"/biyachaind-*.pid "$logdir"/monad-*.pid "$logdir"/feed-*.pid
  else
    rm -f "$logdir/biyachaind-${BENCH_NODE_ROLE}.pid" \
      "$logdir/monad-${BENCH_NODE_ROLE}.pid" \
      "$logdir/feed-${BENCH_NODE_ROLE}.pid"
  fi
}

prepare_biyachaind_ld_path() {
  local lib
  lib="$(biyachain_lib_path || true)"
  if [[ -n "$lib" ]]; then
    export LD_LIBRARY_PATH="$lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
  fi
}

biyachaind_seidb_args() {
  local -n _out=$1
  _out=(
    --store.backend="$STORE_BACKEND"
    --seidb.enabled="$SEIDB_ENABLED"
    --seidb.sc-backend="$SEIDB_SC_BACKEND"
    --seidb.ss-enable="$SEIDB_SS_ENABLE"
    --seidb.ss-backend="$SEIDB_SS_BACKEND"
    --seidb.keep-recent="$SEIDB_KEEP_RECENT"
    --memiavl.async-commit-buffer="$SEIDB_MEMIAVL_ASYNC"
  )
  if [[ -n "$SEIDB_HOME" ]]; then
    _out+=(--seidb.home="$SEIDB_HOME")
  fi
}

wait_abci_socket() {
  local sock="$1" label="$2" max="${3:-60}"
  local i=0
  while (( i < max )); do
    [[ -S "$sock" ]] && return 0
    sleep 0.5
    i=$((i + 1))
  done
  die "超时: $label ABCI socket 未就绪: $sock"
}

wait_mempool_socket() {
  wait_abci_socket "$1" "$2" "${3:-120}"
}

run_biyachaind_node() {
  local n="$1" mode="${2:-fg}"
  ensure_node_dirs "$n"
  [[ -x "$BIYACHAIND_BIN" ]] || die "未找到 $BIYACHAIND_BIN，请先: $0 build"
  local monad_dir="$WORK/monad-$n"
  local abci_sock="$monad_dir/abci.sock"
  local grpc_port metrics_port logdir logf pidf
  local -a seidb_args=()
  grpc_port="$(node_grpc_port "$n")"
  metrics_port="$(node_metrics_port "$n")"
  rm -f "$abci_sock"
  prepare_biyachaind_ld_path
  biyachaind_seidb_args seidb_args

  if [[ "$mode" == fg ]]; then
    echo "==> biyachaind-$n"
    echo "    home=$WORK/biyachain-home-$n"
    echo "    grpc=0.0.0.0:$grpc_port  abci=unix://$abci_sock"
    echo "    metrics=0.0.0.0:$metrics_port (/metrics)"
    echo "    monad-ledger-path=$monad_dir/ledger"
    echo "    store.backend=$STORE_BACKEND seidb.enabled=$SEIDB_ENABLED sc=$SEIDB_SC_BACKEND ss=$SEIDB_SS_ENABLE/$SEIDB_SS_BACKEND"
    exec "$BIYACHAIND_BIN" start \
      --home "$WORK/biyachain-home-$n" \
      --with-comet=false \
      --transport socket \
      --address "unix://$abci_sock" \
      --grpc.enable=true \
      --grpc.address="0.0.0.0:$grpc_port" \
      --api.enable=false \
      --json-rpc.enable=false \
      --optimistic-execution-enabled="${OPTIMISTIC_EXECUTION_ENABLED:-false}" \
      --monad-ledger-path="$monad_dir/ledger" \
      --metrics-address="0.0.0.0:$metrics_port" \
      "${seidb_args[@]}" \
      --minimum-gas-prices "$MIN_GAS_PRICE"
  fi

  logdir="$(node_log_dir_abs)"
  logf="$logdir/biyachaind-${n}.log"
  pidf="$logdir/biyachaind-${n}.pid"
  : >"$logf"
  nohup "$BIYACHAIND_BIN" start \
    --home "$WORK/biyachain-home-$n" \
    --with-comet=false \
    --transport socket \
    --address "unix://$abci_sock" \
    --grpc.enable=true \
    --grpc.address="0.0.0.0:$grpc_port" \
    --api.enable=false \
    --json-rpc.enable=false \
    --optimistic-execution-enabled="${OPTIMISTIC_EXECUTION_ENABLED:-false}" \
    --monad-ledger-path="$monad_dir/ledger" \
    --metrics-address="0.0.0.0:$metrics_port" \
    "${seidb_args[@]}" \
    --minimum-gas-prices "$MIN_GAS_PRICE" >>"$logf" 2>&1 &
  echo $! >"$pidf"
  echo "    biyachaind-$n pid=$! log=$logf grpc=:$grpc_port"
}

run_monad_node() {
  local n="$1" mode="${2:-fg}"
  ensure_node_dirs "$n" genesis
  [[ -x "$MONAD_NODE_BIN" ]] || die "未找到 monad-node（MONAD_NODE_BIN=$MONAD_NODE_BIN），请先: $0 build"
  local monad_dir="$WORK/monad-$n"
  local abci_sock="$monad_dir/abci.sock"
  local monad_bin="$MONAD_NODE_BIN"
  local logdir logf pidf
  local -a monad_args=(
    --secp-identity "$monad_dir/id-secp"
    --bls-identity "$monad_dir/id-bls"
    --node-config "$monad_dir/node.toml"
    --forkpoint-config "$monad_dir/forkpoint.toml"
    --validators-path "$monad_dir/validators.toml"
    --wal-path "$monad_dir/wal"
    --mempool-ipc-path "$monad_dir/mempool.sock"
    --control-panel-ipc-path "$monad_dir/controlpanel.sock"
    --ledger-path "$monad_dir/ledger"
    --statesync-ipc-path "$monad_dir/statesync.sock"
    --triedb-path "$monad_dir"
    --persisted-peers-path "$monad_dir/peers.toml"
    --otel-endpoint http://127.0.0.1:4317
    --record-metrics-interval-seconds 2
  )

  if [[ "$mode" == fg ]]; then
    [[ -S "$abci_sock" ]] || die "ABCI socket 不存在: $abci_sock（请先: $0 biyachaind $n）"
    export MONAD_ABCI_ENDPOINT="unix://$abci_sock"
    export MONAD_COSMOS_GENESIS_PATH="$WORK/genesis.json.reference"
    export RUST_LOG="${RUST_LOG:-info}"
    echo "==> monad-$n"
    echo "    dir=$monad_dir"
    echo "    P2P 绑定地址见 node.toml（默认 $(node_monad_ip "$n"):8000）"
    echo "    四节点共识请先: $0 setup-ips"
    exec "$monad_bin" "${monad_args[@]}"
  fi

  logdir="$(node_log_dir_abs)"
  logf="$logdir/monad-${n}.log"
  pidf="$logdir/monad-${n}.pid"
  : >"$logf"
  nohup env \
    MONAD_ABCI_ENDPOINT="unix://$abci_sock" \
    MONAD_COSMOS_GENESIS_PATH="$WORK/genesis.json.reference" \
    RUST_LOG="${RUST_LOG:-info}" \
    "$monad_bin" "${monad_args[@]}" >>"$logf" 2>&1 &
  echo $! >"$pidf"
  echo "    monad-$n pid=$! log=$logf p2p=$(node_monad_ip "$n"):8000"
}

ensure_all_bench_nodes() {
  local n missing=()
  ensure_stress_workdir
  for n in "${MONAD_BENCH_NODES[@]}"; do
    if [[ ! -d "$WORK/biyachain-home-$n" ]] || [[ ! -d "$WORK/monad-$n" ]]; then
      missing+=("$n")
    fi
  done
  if ((${#missing[@]} > 0)); then
    die "缺少节点 ${missing[*]} 目录，请先: $0 setup（从三节点升级四节点须重新 setup）"
  fi
}

cmd_stop() {
  stop_bench_node_processes
  cleanup_bench_stale_files
  echo "四节点已停止，残留已清理。"
}

cmd_start() {
  ensure_all_bench_nodes
  ensure_genesis_reference
  [[ -x "$BIYACHAIND_BIN" ]] || die "未找到 $BIYACHAIND_BIN，请先: $0 build"
  [[ -x "$MONAD_NODE_BIN" ]] || die "未找到 monad-node（MONAD_NODE_BIN=$MONAD_NODE_BIN），请先: $0 build"
  if bench_start_rpc_enabled; then
    resolve_comet_rpc_bin >/dev/null || die "未找到 cosmos-txpool-feed，请先: $0 build"
  fi

  local logdir n
  logdir="$(node_log_dir_abs)"

  stop_bench_node_processes
  cleanup_bench_stale_files

  if bench_keep_data_enabled; then
    echo "==> BENCH_KEEP_DATA=1，保留链历史数据"
  else
    reset_bench_node_data
  fi

  for n in "${MONAD_BENCH_NODES[@]}"; do
    rm -f "$logdir/biyachaind-${n}.log" "$logdir/monad-${n}.log" \
      "$logdir/feed-${n}.log" \
      "$logdir/biyachaind-${n}.pid" "$logdir/monad-${n}.pid" "$logdir/feed-${n}.pid"
  done

  echo "==> 一键启动四节点（后台，日志 $logdir）"
  echo "    工作目录: $WORK"
  if bench_p2p_remote_enabled; then
    echo "    BENCH_P2P_MODE=remote，跳过 setup-ips"
  else
    cmd_setup_ips
  fi
  apply_monad_sysctl

  echo "==> 启动 biyachaind x4"
  for n in "${MONAD_BENCH_NODES[@]}"; do
    run_biyachaind_node "$n" bg
  done

  echo "==> 等待 ABCI socket"
  for n in "${MONAD_BENCH_NODES[@]}"; do
    wait_abci_socket "$WORK/monad-$n/abci.sock" "biyachaind-$n"
  done

  echo "==> 启动 monad-node（a 优先，其余 30s 内起齐）"
  run_monad_node a bg
  sleep 2
  for n in b c d; do
    run_monad_node "$n" bg
    sleep 1
  done

  echo "==> 等待 P2P 就绪"
  sleep 3
  if bench_start_rpc_enabled; then
    start_rpc_feeds_bg
  fi
  cmd_nodes
  echo ""
  echo "四节点 + RPC 已在后台运行（genesis 干净启动）。"
  echo "  日志: tail -f $logdir/monad-a.log"
  echo "  RPC:  127.0.0.1:$(node_comet_port a)（feed-a，stress 默认入口）"
  echo "  停止: $0 stop"
  echo "  压测: $0 stress   # 或 shard + stress-all"
}

cmd_restart() {
  cmd_start
}

cmd_start_node() {
  local n="${1:-${BENCH_NODE_ROLE:-}}"
  [[ -n "$n" ]] || die "start-node 需要 BENCH_NODE_ROLE 或参数 a|b|c|d"
  case "$n" in
    a|b|c|d) ;;
    *) die "未知节点: $n（a|b|c|d）" ;;
  esac
  export BENCH_NODE_ROLE="$n"
  ensure_node_dirs "$n" genesis
  [[ -x "$BIYACHAIND_BIN" ]] || die "未找到 $BIYACHAIND_BIN，请先: $0 build"
  [[ -x "$MONAD_NODE_BIN" ]] || die "未找到 monad-node（MONAD_NODE_BIN=$MONAD_NODE_BIN），请先: $0 build"

  local logdir
  logdir="$(node_log_dir_abs)"
  stop_bench_node_processes
  cleanup_bench_stale_files

  if ! bench_keep_data_enabled; then
    local fp monad_dir
    fp="$(forkpoint_genesis_template)"
    [[ -f "$fp" ]] || die "未找到 genesis forkpoint: $fp"
    monad_dir="$WORK/monad-$n"
    rm -rf "$monad_dir/wal"
    rm -rf "$monad_dir/ledger/headers" "$monad_dir/ledger/bodies" "$monad_dir/ledger/cosmos-commits"
    mkdir -p "$monad_dir/ledger/headers" "$monad_dir/ledger/bodies" "$monad_dir/ledger/cosmos-commits"
    rm -f "$monad_dir"/forkpoint.rlp "$monad_dir"/forkpoint.rlp.*
    rm -f "$monad_dir"/forkpoint.toml.*
    cp "$fp" "$monad_dir/forkpoint.toml"
    rm -rf "$WORK/biyachain-home-$n/data"
  fi

  rm -f "$logdir/biyachaind-${n}.log" "$logdir/monad-${n}.log" \
    "$logdir/feed-${n}.log" \
    "$logdir/biyachaind-${n}.pid" "$logdir/monad-${n}.pid" "$logdir/feed-${n}.pid"

  echo "==> 启动单节点 $n（后台，日志 $logdir）"
  echo "    工作目录: $WORK  P2P=$(node_monad_ip "$n"):8000"
  apply_monad_sysctl

  echo "==> 启动 biyachaind-$n"
  run_biyachaind_node "$n" bg
  wait_abci_socket "$WORK/monad-$n/abci.sock" "biyachaind-$n"

  echo "==> 启动 monad-$n"
  run_monad_node "$n" bg
  wait_mempool_socket "$WORK/monad-$n/mempool.sock" "monad-$n"

  if bench_start_rpc_enabled; then
    start_rpc_feed_bg "$n"
  fi

  echo ""
  echo "节点 $n 已在后台运行。"
  echo "  日志: tail -f $logdir/monad-${n}.log"
  echo "  RPC:  ${BENCH_RPC_BIND}:$(node_comet_port "$n")"
  echo "  停止: BENCH_NODE_ROLE=$n $0 stop"
}

cmd_setup_ips() {
  need_cmd sudo
  local ip
  for ip in 172.28.0.10 172.28.0.20 172.28.0.30 172.28.0.40; do
    if ip -4 addr show dev lo | grep -q "${ip}/"; then
      echo "  ok: $ip 已在 lo 上"
    else
      echo "==> sudo ip addr add $ip/32 dev lo"
      sudo ip addr add "$ip/32" dev lo
    fi
  done
  echo "loopback IP 就绪（四节点 monad P2P 需要）。"
}

cmd_biyachaind() {
  run_biyachaind_node "${1:-a}" fg
}

cmd_monad() {
  run_monad_node "${1:-a}" fg
}

cmd_repair_monad() {
  ensure_stress_workdir
  echo "==> 重建 monad node.toml / P2P 签名（不重建 id-secp/id-bls）"
  MONAD_BFT_ROOT="$REPO" WORK="$WORK" "$REPO/scripts/mult-run.sh" init-monad-node
  echo "完成。请重启 biyachaind 与 monad-node（先 biyachaind 再 monad）。"
}

cmd_run() {
  ensure_stress_workdir
  cat <<EOF
==> Monad + chain-stresser 本机原生启动（无需 monad-node Docker 镜像）

推荐一键启动（每次清历史数据 + genesis 初始化 + RPC，后台，日志 ./node-log/）:

   $0 start          # 四节点 + cosmos-txpool-feed（26657/26667/26677/26687）
   $0 stop           # 停止全部

或手动多终端启动:

0) 一次性：四节点 P2P loopback IP
   $0 setup-ips

1) 编译 + 初始化（若尚未完成）
   $0 build && $0 setup

2) 八个终端分别启动 biyachaind + monad-node（共识需要 a/b/c/d 四组，每节点一对）

   终端 1: $0 biyachaind a    # gRPC :19900
   终端 2: $0 monad a

   终端 3: $0 biyachaind b    # gRPC :29900
   终端 4: $0 monad b

   终端 5: $0 biyachaind c
   终端 6: $0 monad c

   终端 7: $0 biyachaind d    # gRPC :49900
   终端 8: $0 monad d

   冒烟可只起 a（biyachaind a + monad a），但四验证者配置下可能无法稳定出块。

3) 终端 9：Comet RPC（chain-stresser 入口）
   MONAD_MEMPOOL_SOCK=$WORK/monad-a/mempool.sock $0 rpc

4) 终端 10：压测与验证
   $0 stress
   $0 verify

数据流:
  chain-stresser → :26657 (cosmos-txpool-feed serve) → mempool.sock → monad-node → biyachaind
  确认上链: gRPC $STRESS_GRPC_ADDR 查 sequence（--await=false）

文档: docs/monad-chain-stresser-bench.md
EOF
}

wait_port() {
  local hostport="$1"
  local label="$2"
  local max="${3:-120}"
  local host="${hostport%:*}"
  local port="${hostport##*:}"
  local i=0
  while (( i < max )); do
    if (echo >/dev/tcp/"$host"/"$port") 2>/dev/null; then
      echo "  ok: $label ($hostport)"
      return 0
    fi
    sleep 1
    i=$((i + 1))
  done
  die "超时: $label ($hostport) 未就绪"
}

cmd_up() {
  need_cmd docker
  [[ -f "$WORK/compose.yaml" ]] || die "未找到 $WORK/compose.yaml，请先: $0 setup"
  if [[ ! -f "$REPO/target/release/monad-node" ]]; then
    echo "note: 未找到 monad-node，正在执行 build…"
    cmd_build
  fi
  if [[ ! -x "$REPO/target/release/cosmos-txpool-feed" ]] \
    && [[ ! -x "$REPO/target/release/monad-rpc" ]]; then
    die "未找到 cosmos-txpool-feed / monad-rpc（compose 26657 需要）。请先: $0 build"
  fi
  [[ -f "$REPO/target/release/keystore" ]] || ln -sf monad-keystore "$REPO/target/release/keystore"
  if ! docker image inspect monad-node:local >/dev/null 2>&1; then
    echo "==> 构建 Docker 镜像 monad-node:local（首次较慢）"
    docker build -t monad-node:local -f "$REPO/docker/devnet/Dockerfile" "$REPO"
  fi

  echo "==> docker compose up -d ($WORK)"
  (cd "$WORK" && docker compose up -d)

  echo "==> 等待服务就绪"
  wait_port "$STRESS_GRPC_ADDR" "biyachaind-a gRPC"
  wait_port "$STRESS_NODE_ADDR" "monad-rpc-a Comet RPC"
  echo "集群已就绪。"
  cmd_status
}

cmd_down() {
  [[ -f "$WORK/compose.yaml" ]] || die "未找到 $WORK/compose.yaml"
  (cd "$WORK" && docker compose down)
}

node_p2p_listening() {
  local ip="$1"
  ss -ulnp 2>/dev/null | grep -q "${ip}:8000.*monad-node"
}

forkpoint_genesis_template() {
  echo "${FORKPOINT_GENESIS:-$REPO/docker/devnet/monad/config/forkpoint.genesis.toml}"
}

bench_keep_data_enabled() {
  case "${BENCH_KEEP_DATA:-0}" in
    1|true|yes|TRUE|YES|on|ON) return 0 ;;
    *) return 1 ;;
  esac
}

bench_start_rpc_enabled() {
  case "${BENCH_NO_RPC:-0}" in
    1|true|yes|TRUE|YES|on|ON) return 1 ;;
    *) return 0 ;;
  esac
}

bench_skip_sysctl_enabled() {
  case "${BENCH_SKIP_SYSCTL:-0}" in
    1|true|yes|TRUE|YES|on|ON) return 0 ;;
    *) return 1 ;;
  esac
}

start_rpc_feed_bg() {
  local n="$1" rpc_bin logdir sock port logf pidf bind
  bind="${BENCH_RPC_BIND:-127.0.0.1}"
  rpc_bin="$(resolve_comet_rpc_bin)" || die "未找到 cosmos-txpool-feed / monad-rpc，请先: $0 build"
  if [[ "$(basename "$rpc_bin")" != "cosmos-txpool-feed" ]]; then
    die "start 默认仅支持 cosmos-txpool-feed，请: $0 build"
  fi
  logdir="$(node_log_dir_abs)"
  sock="$WORK/monad-$n/mempool.sock"
  port="$(node_comet_port "$n")"
  [[ -S "$sock" ]] || die "mempool socket 不存在: $sock（请先启动 monad-node $n）"
  logf="$logdir/feed-${n}.log"
  pidf="$logdir/feed-${n}.pid"
  : >"$logf"
  nohup "$rpc_bin" serve --listen "${bind}:$port" --cosmos-ipc-path "$sock" >>"$logf" 2>&1 &
  echo $! >"$pidf"
  echo "    feed-$n: listen=${bind}:$port pid=$! log=$logf"
}

start_rpc_feeds_bg() {
  local n shards
  shards="$(stress_shard_count)"
  echo "==> 后台启动 cosmos-txpool-feed x${shards}"
  for n in "${MONAD_BENCH_NODES[@]:0:$shards}"; do
    start_rpc_feed_bg "$n"
  done
}

# 清四节点 monad WAL/ledger/forkpoint 快照 + biyachaind data，恢复 genesis forkpoint。
reset_bench_node_data() {
  ensure_stress_workdir
  local fp n monad_dir
  fp="$(forkpoint_genesis_template)"
  [[ -f "$fp" ]] || die "未找到 genesis forkpoint: $fp"
  echo "==> 清四节点历史链数据，恢复 genesis 初始化 ($WORK)"
  for n in "${MONAD_BENCH_NODES[@]}"; do
    monad_dir="$WORK/monad-$n"
    rm -rf "$monad_dir/wal"
    rm -rf "$monad_dir/ledger/headers" "$monad_dir/ledger/bodies" "$monad_dir/ledger/cosmos-commits"
    mkdir -p "$monad_dir/ledger/headers" "$monad_dir/ledger/bodies" "$monad_dir/ledger/cosmos-commits"
    rm -f "$monad_dir"/forkpoint.rlp "$monad_dir"/forkpoint.rlp.*
    rm -f "$monad_dir"/forkpoint.toml.*
    cp "$fp" "$monad_dir/forkpoint.toml"
    rm -f "$monad_dir/abci.sock" "$monad_dir/mempool.sock" \
      "$monad_dir/controlpanel.sock" "$monad_dir/statesync.sock"
    rm -rf "$WORK/biyachain-home-$n/data"
    echo "    monad-$n + biyachain-home-$n: 已清 wal/ledger/data/forkpoint 快照"
  done
}

cmd_reset_consensus() {
  if pgrep -f 'monad-node|biyachaind start' >/dev/null 2>&1; then
    echo "⚠  检测到 monad-node / biyachaind 仍在运行，请先停掉："
    echo "   $0 stop"
    die "进程未停止，拒绝 reset-consensus"
  fi
  reset_bench_node_data
  echo ""
  echo "下一步: $0 start"
}

cmd_diagnose() {
  ensure_stress_workdir
  echo "==> 共识/执行诊断 ($WORK)"
  local n commits fp_round abci by
  for n in "${MONAD_BENCH_NODES[@]}"; do
    commits="$(ls "$WORK/monad-$n/ledger/cosmos-commits/"*.rlp 2>/dev/null | wc -l)"
    fp_round="$(grep -E '^round = ' "$WORK/monad-$n/forkpoint.toml" 2>/dev/null | head -1 | awk '{print $3}' || echo '?')"
    abci=NO
    [[ -S "$WORK/monad-$n/abci.sock" ]] && abci=YES
    by=NO
    pgrep -f "biyachain-home-$n " >/dev/null 2>&1 && by=YES
    echo "  monad-$n: cosmos-commits=${commits} files (latest=$((commits - 1))) forkpoint_round=$fp_round biyachaind=$by abci=$abci"
  done
  echo ""
  echo "若卡在高度 4："
  echo "  - monad 日志 try_propose_seq_num=8 + no proposal result for height 5 → pending 树超前 ledger"
  echo "  - 需重建 monad-node: $0 build && 四节点 reset-consensus 后重启"
  echo "  - biyachaind 终端应有 monad executed block height=N 随 committed cosmos block 增长"
}

cmd_nodes() {
  ensure_stress_workdir
  echo "==> 本机四节点 ($WORK)"
  printf "%-6s %-14s %-8s %-8s %-8s %-6s\n" "节点" "P2P IP" "biyachaind" "monad" "gRPC" "abci"
  local n ip grpc monad_dir abci_sock by monad grpc_ok abci_ok missing=0
  for n in "${MONAD_BENCH_NODES[@]}"; do
    ip="$(node_monad_ip "$n")"
    grpc="$(node_grpc_port "$n")"
    monad_dir="$WORK/monad-$n"
    abci_sock="$monad_dir/abci.sock"
    by=NO
    monad=NO
    grpc_ok=CLOSED
    abci_ok=NO
    pgrep -f "biyachain-home-$n " >/dev/null 2>&1 && by=YES
    if node_p2p_listening "$ip"; then
      monad=YES
    fi
    if (echo >/dev/tcp/127.0.0.1/"$grpc") 2>/dev/null; then
      grpc_ok=LISTEN
    fi
    [[ -S "$abci_sock" ]] && abci_ok=YES
    printf "%-6s %-14s %-8s %-8s %-8s %-6s\n" "$n" "$ip:8000" "$by" "$monad" ":$grpc $grpc_ok" "$abci_ok"
    if [[ "$by" == YES && "$monad" == NO ]] || [[ "$monad" == YES && "$by" == NO ]]; then
      missing=$((missing + 1))
    fi
  done
  echo ""
  if ! node_p2p_listening "172.28.0.10"; then
    echo "⚠  monad-a 未监听 172.28.0.10:8000 — 四节点共识缺 leader，其它节点会报 address unknown"
    echo "   请: $0 biyachaind a  然后  $0 monad a"
  fi
  if node_p2p_listening "172.28.0.20" && pgrep -f 'monad-b/id-secp' >/dev/null 2>&1; then
    echo "note: monad-b 已在运行，勿重复执行 $0 monad b（会 AddrInUse）"
  fi
  if (( missing > 0 )); then
    echo "note: 每组须 biyachaind 与 monad 成对运行，且先 biyachaind 再 monad"
  fi
  local rounds=() r n
  for n in "${MONAD_BENCH_NODES[@]}"; do
    r="$(grep -E '^round = ' "$WORK/monad-$n/forkpoint.toml" 2>/dev/null | head -1 | awk '{print $3}' || echo '?')"
    rounds+=("$n:$r")
  done
  local uniq
  uniq="$(printf '%s\n' "${rounds[@]}" | awk -F: '{print $2}' | sort -u | wc -l)"
  if [[ "$uniq" -gt 1 ]]; then
    echo "⚠  forkpoint round 不一致: ${rounds[*]} — 共识会卡在 still syncing / address unknown"
    echo "   停进程后: $0 reset-consensus，再按 a→b→c→d 重启"
  elif [[ "${rounds[0]#*:}" != "0" && "${rounds[0]#*:}" != "?" ]]; then
    echo "note: forkpoint round=${rounds[0]#*:}（非 genesis）；若长期 still syncing 请 $0 reset-consensus"
  fi
  echo ""
  bench_print_process_summary
}

bench_count_node_procs() {
  local svc="$1" n="$2"
  case "$svc" in
    biyachaind) pgrep -cf "biyachaind start --home ${WORK}/biyachain-home-${n}" 2>/dev/null || echo 0 ;;
    monad) pgrep -cf "${WORK}/monad-${n}/id-secp" 2>/dev/null || echo 0 ;;
    *) echo 0 ;;
  esac
}

bench_print_process_summary() {
  local n by monad by_total=0 monad_total=0 dup=0
  echo "==> 进程汇总（四节点预期: biyachaind=4 + monad-node=4 = 8 个主进程）"
  for n in "${MONAD_BENCH_NODES[@]}"; do
    by="$(bench_count_node_procs biyachaind "$n")"
    monad="$(bench_count_node_procs monad "$n")"
    by_total=$((by_total + by))
    monad_total=$((monad_total + monad))
    if (( by > 1 || monad > 1 )); then
      dup=1
      echo "  ⚠  节点 $n: biyachaind=$by monad=$monad（同节点不应 >1，请 $0 restart）"
    fi
  done
  echo "  当前: biyachaind=$by_total monad-node=$monad_total"
  local feed_total=0
  for n in "${MONAD_BENCH_NODES[@]}"; do
    bench_svc_pid_alive feed "$n" && feed_total=$((feed_total + 1))
  done
  echo "  RPC feed: $feed_total / ${#MONAD_BENCH_NODES[@]}（127.0.0.1:$(node_comet_port a) 等）"
  if (( dup == 0 && by_total == 4 && monad_total == 4 )); then
    echo "  ok: 无重复主进程"
  elif (( by_total != 4 || monad_total != 4 )); then
    echo "  note: 数量异常时可 $0 stop && $0 start 清理"
  fi
  echo "  note: biyachaind 为 Go 程序，htop 展开线程后每节点约 40+ 条，不是重复启动"
}

cmd_status() {
  cmd_nodes
  echo ""
  echo "==> docker compose ps"
  if [[ -f "$WORK/compose.yaml" ]]; then
    (cd "$WORK" && docker compose ps) || true
  else
    echo "  (无 $WORK/compose.yaml)"
  fi
  echo ""
  echo "==> 端口探测"
  for hp in "$STRESS_GRPC_ADDR" "$STRESS_NODE_ADDR"; do
    local host="${hp%:*}" port="${hp##*:}"
    if (echo >/dev/tcp/"$host"/"$port") 2>/dev/null; then
      echo "  LISTEN $hp"
    else
      echo "  CLOSED $hp"
    fi
  done
}

stress_account_address() {
  local dir addr_file
  dir="$(dirname "$STRESS_ACCOUNTS")"
  addr_file="$dir/addresses.json"
  if [[ -f "$addr_file" ]]; then
    jq -r '.[0] // empty' "$addr_file" | head -1
    return
  fi
  jq -r '.[0] // empty' "$STRESS_ACCOUNTS" 2>/dev/null | head -1
}

cmd_verify() {
  need_cmd jq
  [[ -x "$BIYACHAIND_BIN" ]] || die "未找到 $BIYACHAIND_BIN"
  [[ -f "$STRESS_ACCOUNTS" ]] || die "未找到 $STRESS_ACCOUNTS"

  local addr
  addr="$(stress_account_address)"
  [[ -n "$addr" ]] || die "无法从 $STRESS_ACCOUNTS 解析地址"

  local home="$WORK/biyachain-home-a"
  [[ -d "$home" ]] || home="${BIYAHOME:-}"

  echo "==> 查询压测账户 sequence (addr=$addr)"
  local auth_json err
  if ! auth_json="$("$BIYACHAIND_BIN" query auth account "$addr" \
    --grpc-addr "$STRESS_GRPC_ADDR" --grpc-insecure \
    --home "$home" -o json 2>&1)"; then
    echo "$auth_json" >&2
    die "gRPC 查询失败（biyachaind-a 是否在运行？端口 $STRESS_GRPC_ADDR）"
  fi
  # EthAccount: .account.value.base_account；BaseAccount: .account.base_account
  echo "$auth_json" | jq '{
      address: (.account.value.base_account.address // .account.base_account.address // .account.address),
      account_number: (.account.value.base_account.account_number // .account.base_account.account_number // .account.account_number),
      sequence: (.account.value.base_account.sequence // .account.base_account.sequence // .account.sequence)
    }'
}

# 单路 chain-stresser 调用：accounts / accounts-num / node-addr / grpc-addr 参数化，
# 供 cmd_stress（单节点）与 cmd_stress_all（多节点分片）复用。
run_chain_stresser() {
  local accounts="$1" num="$2" node_addr="$3" grpc_addr="$4"
  case "$STRESS_CMD" in
    bank)
      chain-stresser tx-bank-send \
        --accounts "$accounts" \
        --accounts-num "$num" \
        --transactions "$STRESS_TRANSACTIONS" \
        --rate-tps "$STRESS_RATE_TPS" \
        --chain-id "$CHAIN_ID" \
        --min-gas-price "$MIN_GAS_PRICE" \
        --node-addr "$node_addr" \
        --grpc-addr "$grpc_addr" \
        --await=false
      ;;
    spot-limit)
      chain-stresser tx-exchange-spot-limit-orders \
        --accounts "$accounts" \
        --accounts-num "$num" \
        --transactions "$STRESS_TRANSACTIONS" \
        --rate-tps "$STRESS_RATE_TPS" \
        --spot-market-ids "$SPOT_MARKET_ID" \
        --chain-id "$CHAIN_ID" \
        --min-gas-price "$MIN_GAS_PRICE" \
        --node-addr "$node_addr" \
        --grpc-addr "$grpc_addr" \
        --await=false
      ;;
    *)
      die "未知 STRESS_CMD=$STRESS_CMD（bank | spot-limit）"
      ;;
  esac
}

cmd_stress() {
  need_cmd chain-stresser
  [[ -f "$STRESS_ACCOUNTS" ]] || die "未找到 $STRESS_ACCOUNTS，请先: $0 setup"

  echo "==> chain-stresser ($STRESS_CMD)"
  echo "    accounts=$STRESS_ACCOUNTS"
  echo "    accounts-num=$STRESS_ACCOUNTS_NUM_RUN transactions=$STRESS_TRANSACTIONS rate-tps=$STRESS_RATE_TPS"
  echo "    node-addr=$STRESS_NODE_ADDR grpc-addr=$STRESS_GRPC_ADDR chain-id=$CHAIN_ID"
  echo "    await=false (monad-rpc 无 tx 查询)"
  echo ""

  echo "==> 压测前 sequence"
  cmd_verify || true
  echo ""

  local start_ts end_ts
  start_ts="$(date +%s)"

  run_chain_stresser "$STRESS_ACCOUNTS" "$STRESS_ACCOUNTS_NUM_RUN" "$STRESS_NODE_ADDR" "$STRESS_GRPC_ADDR"

  end_ts="$(date +%s)"
  local elapsed=$((end_ts - start_ts))
  local total_tx=$((STRESS_ACCOUNTS_NUM_RUN * STRESS_TRANSACTIONS))

  echo ""
  echo "==> 压测完成 (${elapsed}s, 约 ${total_tx} 笔提交)"
  echo "==> 等待出块 (~5s) 后查 sequence"
  sleep 5
  cmd_verify
}

resolve_comet_rpc_bin() {
  if [[ -n "${MONAD_RPC_BIN:-}" && -x "${MONAD_RPC_BIN}" ]]; then
    echo "${MONAD_RPC_BIN}"
    return
  fi
  if [[ -x "$COSMOS_TXPOOL_FEED_BIN" ]]; then
    echo "$COSMOS_TXPOOL_FEED_BIN"
    return
  fi
  if [[ -x "$REPO/target/release/cosmos-txpool-feed" ]]; then
    echo "$REPO/target/release/cosmos-txpool-feed"
    return
  fi
  if [[ -x "$REPO/target/release/monad-rpc" ]]; then
    echo "$REPO/target/release/monad-rpc"
    return
  fi
  return 1
}

cmd_rpc() {
  # 可选节点参数 a|b|c|d：自动选择对应 monad-<n>/mempool.sock 与监听端口。
  # 显式 MONAD_MEMPOOL_SOCK / COMET_RPC_LISTEN 始终优先。
  local node="${1:-}"
  local monad_dir="${MONAD_RUN_DIR:-}"
  local node_toml="${MONAD_NODE_CONFIG:-$REPO/docker/devnet/monad/config/node.toml}"
  local mempool_sock="${MONAD_MEMPOOL_SOCK:-}"
  local listen="${COMET_RPC_LISTEN:-}"
  if [[ -n "$node" ]]; then
    [[ -z "$mempool_sock" ]] && mempool_sock="$WORK/monad-$node/mempool.sock"
    [[ -z "$listen" ]] && listen="127.0.0.1:$(node_comet_port "$node")"
  fi
  if [[ -z "$mempool_sock" ]]; then
    if [[ -n "$monad_dir" ]]; then
      mempool_sock="$monad_dir/mempool.sock"
    elif [[ -S "$WORK/monad-a/mempool.sock" ]]; then
      mempool_sock="$WORK/monad-a/mempool.sock"
    else
      mempool_sock="$REPO/.monad-home/mempool.sock"
    fi
  fi
  [[ -z "$listen" ]] && listen="127.0.0.1:26657"
  local rpc_bin

  rpc_bin="$(resolve_comet_rpc_bin)" || die "未找到 cosmos-txpool-feed / monad-rpc，请先: $0 build"
  [[ -S "$mempool_sock" ]] || die "mempool socket 不存在: $mempool_sock（请先启动 monad-node）"

  if [[ "$(basename "$rpc_bin")" == "cosmos-txpool-feed" ]]; then
    echo "==> cosmos-txpool-feed serve (listen=$listen, mempool=$mempool_sock)"
    exec "$rpc_bin" serve --listen "$listen" --cosmos-ipc-path "$mempool_sock"
  fi

  echo "==> monad-rpc (comet 26657, mempool=$mempool_sock)"
  exec "$rpc_bin" \
    --node-config "$node_toml" \
    --rpc-addr 127.0.0.1 \
    --rpc-port 8545 \
    --comet-port 26657 \
    --cosmos-ipc-path "$mempool_sock"
}

stress_shard_count() {
  local max="${#MONAD_BENCH_NODES[@]}"
  local n="${STRESS_SHARDS:-$max}"
  (( n >= 1 && n <= max )) || die "STRESS_SHARDS 须为 1..$max（当前 $n）"
  echo "$n"
}

# 把账户文件按节点切片，避免多节点并发时同一账户 nonce 冲突。
# accounts.json / addresses.json 均为顶层数组；输出 accounts.<n>.json / addresses.<n>.json。
cmd_shard() {
  need_cmd jq
  ensure_stress_workdir
  [[ -f "$STRESS_ACCOUNTS" ]] || die "未找到 $STRESS_ACCOUNTS，请先: $0 setup"
  local shards dir acc addr total per
  shards="$(stress_shard_count)"
  dir="$(dirname "$STRESS_ACCOUNTS")"
  acc="$STRESS_ACCOUNTS"
  addr="$dir/addresses.json"
  total="$(jq 'length' "$acc")"
  per=$(( total / shards ))
  (( per > 0 )) || die "账户数 $total 不足以分 $shards 片"
  echo "==> 账户分片: total=$total shards=$shards per=$per ($dir)"
  local nodes=("${MONAD_BENCH_NODES[@]}") n start end i=0
  for n in "${nodes[@]:0:$shards}"; do
    start=$(( i * per ))
    end=$(( start + per ))
    jq -c ".[$start:$end]" "$acc" > "$dir/accounts.$n.json"
    if [[ -f "$addr" ]]; then
      jq -c ".[$start:$end]" "$addr" > "$dir/addresses.$n.json"
    fi
    echo "    $n: [$start:$end) -> accounts.$n.json ($per 账户)"
    i=$(( i + 1 ))
  done
  echo "完成。下一步: $0 stress-all"
}

# 后台一键启动每节点一个 cosmos-txpool-feed（前台 wait 守护，Ctrl-C 全停）。
cmd_rpc_all() {
  ensure_stress_workdir
  local rpc_bin shards
  rpc_bin="$(resolve_comet_rpc_bin)" || die "未找到 cosmos-txpool-feed / monad-rpc，请先: $0 build"
  shards="$(stress_shard_count)"
  local nodes=("${MONAD_BENCH_NODES[@]}") n sock port logf
  trap 'echo; echo "==> 停止所有 feed"; kill 0 2>/dev/null; exit 0' INT TERM
  echo "==> 后台启动 $shards 个 cosmos-txpool-feed（每节点一个）"
  for n in "${nodes[@]:0:$shards}"; do
    sock="$WORK/monad-$n/mempool.sock"
    port="$(node_comet_port "$n")"
    [[ -S "$sock" ]] || die "mempool socket 不存在: $sock（请先启动 monad-node $n）"
    logf="$WORK/feed-$n.log"
    COMET_RPC_LISTEN="127.0.0.1:$port" MONAD_MEMPOOL_SOCK="$sock" \
      "$0" rpc "$n" >"$logf" 2>&1 &
    echo "    feed-$n: listen=127.0.0.1:$port mempool=$sock pid=$! log=$logf"
  done
  echo ""
  echo "压测: $0 shard && $0 stress-all（另开终端）"
  echo "停止: Ctrl-C（或 pkill -f 'cosmos-txpool-feed serve'）"
  wait
}

# 多节点分片并发压测：每片用独立账户子集 + 对应节点的 comet/gRPC 端口。
# 总吞吐约 shards * STRESS_RATE_TPS（每片各自限速 STRESS_RATE_TPS）。
cmd_stress_all() {
  need_cmd chain-stresser
  need_cmd jq
  ensure_stress_workdir
  local shards dir
  shards="$(stress_shard_count)"
  dir="$(dirname "$STRESS_ACCOUNTS")"
  local nodes=("${MONAD_BENCH_NODES[@]}") n need_shard=0
  for n in "${nodes[@]:0:$shards}"; do
    if [[ ! -f "$dir/accounts.$n.json" ]]; then
      need_shard=1
      break
    fi
  done
  if (( need_shard )); then
    echo "==> 未找到分片 accounts.<n>.json，自动执行 shard"
    cmd_shard
  fi
  local per num first="${nodes[0]}"
  per="$(jq 'length' "$dir/accounts.${first}.json")"
  num="$STRESS_ACCOUNTS_NUM_RUN"
  (( num > per )) && num="$per"

  echo "==> stress-all ($STRESS_CMD): shards=$shards 每片 accounts-num=$num transactions=$STRESS_TRANSACTIONS"
  echo "    每片 rate-tps=$STRESS_RATE_TPS  →  总目标 ~$(( shards * STRESS_RATE_TPS )) TPS"
  echo "    实时进度: tail -f $WORK/stress-{a,b,c,d}.log"
  echo ""

  local start_ts end_ts pids=() acc port grpc logf
  start_ts="$(date +%s)"
  for n in "${nodes[@]:0:$shards}"; do
    acc="$dir/accounts.$n.json"
    port="$(node_comet_port "$n")"
    grpc="$(node_grpc_port "$n")"
    logf="$WORK/stress-$n.log"
    echo "    shard-$n: node-addr=$(node_stress_rpc_addr "$n") grpc=$(node_stress_grpc_addr "$n") accounts=$acc log=$logf"
    run_chain_stresser "$acc" "$num" "$(node_stress_rpc_addr "$n")" "$(node_stress_grpc_addr "$n")" >"$logf" 2>&1 &
    pids+=("$!")
  done

  local rc=0 p
  for p in "${pids[@]}"; do
    wait "$p" || rc=1
  done
  end_ts="$(date +%s)"
  local elapsed=$((end_ts - start_ts))
  local total_tx=$(( shards * num * STRESS_TRANSACTIONS ))
  echo ""
  echo "==> stress-all 完成 (rc=$rc, ${elapsed}s, 约 ${total_tx} 笔提交)"
  echo "==> 各片日志末尾:"
  for n in "${nodes[@]:0:$shards}"; do
    echo "  --- stress-$n.log ---"
    tail -n 3 "$WORK/stress-$n.log" 2>/dev/null || true
  done
  echo ""
  echo "==> 等待出块 (~5s) 后查 sequence (节点 a)"
  sleep 5
  cmd_verify || true
}

main() {
  local cmd="${1:-}"
  case "$cmd" in
    setup) cmd_setup ;;
    build) cmd_build ;;
    build-full) cmd_build_full ;;
    build-rpc-docker) cmd_build_rpc_docker ;;
    run) cmd_run ;;
    start) cmd_start ;;
    start-node) cmd_start_node "${2:-}" ;;
    restart) cmd_restart ;;
    stop) cmd_stop ;;
    setup-ips) cmd_setup_ips ;;
    biyachaind) cmd_biyachaind "${2:-a}" ;;
    monad) cmd_monad "${2:-a}" ;;
    repair-monad) cmd_repair_monad ;;
    reset-consensus) cmd_reset_consensus ;;
    diagnose) cmd_diagnose ;;
    up) cmd_up ;;
    down) cmd_down ;;
    stress) cmd_stress ;;
    stress-all) cmd_stress_all ;;
    shard) cmd_shard ;;
    verify) cmd_verify ;;
    rpc) cmd_rpc "${2:-}" ;;
    rpc-all) cmd_rpc_all ;;
    nodes) cmd_nodes ;;
    status) cmd_status ;;
    -h | --help | help | "") usage; [[ -n "$cmd" ]] || exit 0 ;;
    *) die "未知命令: $cmd（$0 --help）" ;;
  esac
}

main "$@"
