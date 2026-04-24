#!/usr/bin/env bash
# 从各 monad-* 目录下的 id-secp / id-bls 加密 keystore 生成 validators.toml。
#
# 说明：keystore 文件体是 ciphertext，无法用 jq 推出公钥；本脚本仅用 jq（若存在）
# 做 JSON 合法性检查，公钥必须依赖 keystore recover（Docker 或本机二进制）。
#
# 用法：
#   MONAD_BFT_ROOT=/path/to/monad-bft ./scripts/gen-validators-toml.sh
#   WORK=/path/to/.monad ./scripts/gen-validators-toml.sh -o /tmp/v.toml
# 环境变量：
#   WORK            多节点数据根目录（默认 $MONAD_BFT_ROOT/data-monad-multinode）
#   MONAD_IMAGE     Docker 镜像（默认 monad-node:local）
#   KEYSTORE_PASSWORD  keystore 密码（默认空）
#   NODES           节点后缀，空格分隔（默认 "a b c d"）
#   EPOCH           validator_sets.epoch（默认 1）
#   STAKE           每条 stake（默认 1）
#   KEYSTORE_BIN    若设置则直接调用该路径的 keystore，跳过 Docker

set -euo pipefail

usage() {
  sed -n '1,22p' "$0" | tail -n +2
  echo "选项: -o FILE  输出路径（默认 \$WORK/monad-a/validators.toml）"
  echo "      -n \"a b c\"  仅处理列出的节点（覆盖 NODES）"
}

MONAD_BFT_ROOT="${MONAD_BFT_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
WORK="${WORK:-$MONAD_BFT_ROOT/.monad}"
MONAD_IMAGE="${MONAD_IMAGE:-monad-node:local}"
KEYSTORE_PASSWORD="${KEYSTORE_PASSWORD:-}"
NODES="${NODES:-a b c d}"
EPOCH="${EPOCH:-1}"
STAKE="${STAKE:-1}"
OUT=""

while getopts "ho:n:" opt; do
  case "$opt" in
    h) usage; exit 0 ;;
    o) OUT="$OPTARG" ;;
    n) NODES=$OPTARG ;;
    *) usage; exit 2 ;;
  esac
done

if [[ -z "$OUT" ]]; then
  OUT="$WORK/monad-a/validators.toml"
fi

recover_output() {
  local key_type="$1"   # secp | bls
  local ks_path="$2"    # host path to keystore file
  local mount_dir
  mount_dir="$(dirname "$ks_path")"
  local base
  base="$(basename "$ks_path")"

  if [[ -n "${KEYSTORE_BIN:-}" ]]; then
    "$KEYSTORE_BIN" recover \
      --keystore-path "$ks_path" \
      --password "$KEYSTORE_PASSWORD" \
      --key-type "$key_type"
  else
    docker run --rm \
      -v "$mount_dir:/k:ro" \
      "$MONAD_IMAGE" \
      keystore recover \
      --keystore-path "/k/$base" \
      --password "$KEYSTORE_PASSWORD" \
      --key-type "$key_type"
  fi
}

extract_hex_line() {
  # $1: label e.g. "Secp public key" or "BLS public key"
  awk -F': ' -v lbl="$1" '$0 ~ lbl { gsub(/^[[:space:]]+|[[:space:]]+$/, "", $2); print $2; exit }'
}

require_hex() {
  local name="$1" val="$2"
  if [[ -z "$val" ]]; then
    echo "错误: 未能解析 $name" >&2
    exit 1
  fi
  if [[ "$val" =~ ^0x ]]; then
    val="${val#0x}"
  fi
  if [[ ! "$val" =~ ^[0-9a-fA-F]+$ ]]; then
    echo "错误: $name 不是合法 hex: $val" >&2
    exit 1
  fi
  printf '%s' "$val" | tr 'A-F' 'a-f'
}

for suf in $NODES; do
  dir="$WORK/monad-$suf"
  if [[ ! -f "$dir/id-secp" || ! -f "$dir/id-bls" ]]; then
    echo "错误: 缺少 $dir/id-secp 或 $dir/id-bls" >&2
    exit 1
  fi
  if command -v jq >/dev/null 2>&1; then
    jq -e . "$dir/id-secp" >/dev/null
    jq -e . "$dir/id-bls" >/dev/null
  fi
done

umask 077
tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

{
  echo "[[validator_sets]]"
  echo "epoch = $EPOCH"
  echo ""
} >"$tmp"

for suf in $NODES; do
  dir="$WORK/monad-$suf"
  secp_raw="$(recover_output secp "$dir/id-secp" 2>&1 | extract_hex_line 'Secp public key')"
  bls_raw="$(recover_output bls "$dir/id-bls" 2>&1 | extract_hex_line 'BLS public key')"
  secp_hex="$(require_hex "monad-$suf Secp public key" "$secp_raw")"
  bls_hex="$(require_hex "monad-$suf BLS public key" "$bls_raw")"
  {
    echo "[[validator_sets.validators]]"
    echo "node_id = \"0x$secp_hex\""
    echo "stake = $STAKE"
    echo "cert_pubkey = \"0x$bls_hex\""
    echo ""
  } >>"$tmp"
done

mkdir -p "$(dirname "$OUT")"
cp "$tmp" "$OUT"
echo "已写入: $OUT"
