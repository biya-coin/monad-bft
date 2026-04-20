FROM ubuntu:25.04 AS base

WORKDIR /usr/src/monad-bft

RUN apt update && apt install -y \
  binutils \
  iproute2 \
  clang \
  curl \
  make \
  ca-certificates \
  pkg-config \
  gnupg \
  software-properties-common \
  wget \
  git \
  python-is-python3 \
  cgroup-tools \
  libstdc++-15-dev \
  gcc-15 \
  g++-15

RUN apt update && apt install -y \
  libboost-atomic1.83.0 \
  libboost-container1.83.0 \
  libboost-fiber1.83.0 \
  libboost-filesystem1.83.0 \
  libboost-graph1.83.0 \
  libboost-json1.83.0 \
  libboost-regex1.83.0 \
  libboost-stacktrace1.83.0

RUN apt update && apt install -y \
  libabsl-dev \
  libarchive-dev \
  libbenchmark-dev \
  libbrotli-dev \
  libcap-dev \
  libcgroup-dev \
  libcli11-dev \
  libcrypto++-dev \
  libgmock-dev \
  libgmp-dev \
  libgtest-dev \
  libhugetlbfs-dev \
  libmagicenum-dev \
  libmimalloc-dev \
  libtbb-dev \
  liburing-dev \
  libzstd-dev

FROM base AS builder

RUN apt update && apt install -y \
  cmake \
  clang \
  libssl-dev

RUN apt update && apt install -y \
  libboost-fiber1.83-dev \
  libboost-graph1.83-dev \
  libboost-json1.83-dev \
  libboost-stacktrace1.83-dev \
  libboost1.83-dev


ENV HTTP_PROXY="http://192.168.2.107:7897" \
  HTTPS_PROXY="http://192.168.2.107:7897" \
  NO_PROXY="localhost,127.0.0.1,::1" \
  http_proxy="http://192.168.2.107:7897" \
  https_proxy="http://192.168.2.107:7897" \
  no_proxy="localhost,127.0.0.1,::1"


# install cargo
ARG CARGO_ROOT="/root/.cargo"
RUN bash -lc "set -o pipefail && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | bash -s -- -y"
ENV PATH="${CARGO_ROOT}/bin:$PATH"
RUN rustup toolchain install 1.91.1-x86_64-unknown-linux-gnu
ARG TRIEDB_TARGET=triedb_driver

# CI 可传 --build-arg GIT_TAG_VERSION=$(git describe --tags)；本地省略时用默认值
ARG GIT_TAG_VERSION=dev-local
ENV MONAD_VERSION=$GIT_TAG_VERSION

ARG GIT_COMMIT=""
ARG GIT_BRANCH=""
ARG GIT_TAG=""
ARG GIT_MODIFIED="false"

ENV GIT_COMMIT=$GIT_COMMIT
ENV GIT_BRANCH=$GIT_BRANCH
ENV GIT_TAG=$GIT_TAG
ENV GIT_MODIFIED=$GIT_MODIFIED

# Builder
COPY . .
RUN ASMFLAGS="-march=haswell" CFLAGS="-march=haswell -fno-omit-frame-pointer" CXXFLAGS="-march=haswell -fno-omit-frame-pointer" \
    RUSTFLAGS="-C target-cpu=haswell -C force-frame-pointers=yes" CC=gcc-15 CXX=g++-15 cargo build --release \
      -p monad-node --bin monad-node --example ledger-tail \
      -p monad-keystore --bin monad-keystore \
      -p monad-debug-node --bin monad-debug-node \
      -p monad-peer-discovery --bin sign-name-record \
      -p monad-wal --example wal2json \
      -p monad-triedb-utils --example triedb-bench && \
    mv target/release/monad-node monad-node && \
    mv target/release/monad-keystore keystore && \
    mv target/release/monad-debug-node monad-debug-node && \
    mv target/release/sign-name-record sign-name-record && \
    mv target/release/examples/ledger-tail ledger-tail && \
    mv target/release/examples/wal2json wal2json && \
    mv target/release/examples/triedb-bench triedb-bench && \
    cp `ls -Lt $(find target/release | grep -e "libtriedb_driver.so")` . && \
    ldd $(ls -Lt $(find target/release | grep -e "libtriedb_driver.so")) | docker/filter-dependent-shared-objects.py | xargs -I {} cp {} .

# Runner
FROM base AS runner

WORKDIR /usr/src/monad-bft
# 不展开 $LD_LIBRARY_PATH：base 镜像未定义时会触发 BuildKit UndefinedVar 警告
ENV LD_LIBRARY_PATH=/usr/local/lib
ENV RUST_LOG=info
ENV RUST_BACKTRACE=1

COPY --from=builder /usr/src/monad-bft/monad-node/monad-node /usr/local/bin/monad-node
COPY --from=builder /usr/src/monad-bft/keystore /usr/local/bin/keystore
COPY --from=builder /usr/src/monad-bft/monad-debug-node/monad-debug-node /usr/local/bin/monad-debug-node
COPY --from=builder /usr/src/monad-bft/ledger-tail /usr/local/bin/ledger-tail
COPY --from=builder /usr/src/monad-bft/wal2json /usr/local/bin/wal2json
COPY --from=builder /usr/src/monad-bft/triedb-bench /usr/local/bin/triedb-bench
COPY --from=builder /usr/src/monad-bft/sign-name-record /usr/local/bin/sign-name-record
COPY --from=builder /usr/src/monad-bft/*.so /usr/local/lib
COPY --from=builder /usr/src/monad-bft/*.so.* /usr/local/lib
