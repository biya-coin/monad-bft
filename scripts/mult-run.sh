#!/usr/bin/env bash

# prepare environment

export MONAD_BFT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export WORK="$MONAD_BFT_ROOT/.monad"
export CHAIN_ID="${CHAIN_ID:-biyachain-1}"
export BIYACHAIND_BIN="$MONAD_BFT_ROOT/biyachain-core/bin/biyachaind"
export CHAIN_STRESSER_ROOT="${CHAIN_STRESSER_ROOT:-$(cd "$MONAD_BFT_ROOT/.." && pwd)/chain-stresser}"
export KEYRING="${KEYRING:-test}"
export GENESIS_BALANCE="${GENESIS_BALANCE:-1000000000000000000000byb}"
export GENTX_STAKE="${GENTX_STAKE:-500000000000000000000byb}"
export STRESS_ACCOUNTS_NUM="${STRESS_ACCOUNTS_NUM:-1000}"
export STRESS_ACCOUNTS_DIR="$WORK/instances/0"
export STRESS_ACCOUNT_BALANCE_BYB="${STRESS_ACCOUNT_BALANCE_BYB:-1000000000000000000000000000byb}"
export STRESS_ACCOUNT_BALANCE_USDT="${STRESS_ACCOUNT_BALANCE_USDT:-10000000000000000000peggy0xdAC17F958D2ee523a2206206994597C13D831ec7}"

# 将 devnet 模板写入 $WORK（勿覆盖 $WORK/compose.yaml，多节点 compose 一般在 .monad 内维护）
init_workspace_templates() {
    mkdir -p "$WORK"
    cp -a "$MONAD_BFT_ROOT/docker/devnet/compose.yaml" "$WORK/compose.yaml"
    cp -a "$MONAD_BFT_ROOT/docker/devnet/monad/config/node.toml" "$WORK/node.toml"
    if [[ -e "$MONAD_BFT_ROOT/biyachain-lib" ]]; then
        rm -rf "$WORK/biyachain-lib"
        cp -a "$MONAD_BFT_ROOT/biyachain-lib" "$WORK/biyachain-lib"
    fi
    if [[ -e "$MONAD_BFT_ROOT/docker/devnet/rpc-lib" ]]; then
        rm -rf "$WORK/rpc-lib"
        cp -a "$MONAD_BFT_ROOT/docker/devnet/rpc-lib" "$WORK/rpc-lib"
    fi
}

generate_stress_accounts() {
    echo "--------------------------------"
    echo "generate $STRESS_ACCOUNTS_NUM stress accounts"
    echo "--------------------------------"

    if [[ ! -d "$CHAIN_STRESSER_ROOT" ]]; then
        echo "错误: 找不到 chain-stresser 仓库目录 $CHAIN_STRESSER_ROOT" >&2
        exit 1
    fi

    local keyring_dir="$WORK/stress-keyring"
    rm -rf "$keyring_dir"
    mkdir -p "$STRESS_ACCOUNTS_DIR"

    local gen_bin="$CHAIN_STRESSER_ROOT/bin/gen-accounts"
    if [[ ! -x "$gen_bin" ]]; then
        echo "building gen-accounts..."
        (
            cd "$CHAIN_STRESSER_ROOT"
            go build -o "$gen_bin" ./cmd/gen-accounts/
        )
    fi

    "$gen_bin" generate \
        --num "$STRESS_ACCOUNTS_NUM" \
        --out "$STRESS_ACCOUNTS_DIR"

    if [[ ! -f "$STRESS_ACCOUNTS_DIR/accounts.json" ]] || [[ ! -f "$STRESS_ACCOUNTS_DIR/addresses.json" ]]; then
        echo "错误: gen-accounts 未生成 accounts.json/addresses.json" >&2
        exit 1
    fi

    echo "stress accounts written to $STRESS_ACCOUNTS_DIR/accounts.json"
    echo "stress keyring written to $keyring_dir"
}

ensure_stress_accounts_generated() {
    local accounts_file="$STRESS_ACCOUNTS_DIR/accounts.json"
    local addresses_file="$STRESS_ACCOUNTS_DIR/addresses.json"

    if [[ -f "$accounts_file" ]] && [[ -f "$addresses_file" ]]; then
        local existed_num
        existed_num="$(jq 'length' "$addresses_file")"

        if [[ "$existed_num" == "$STRESS_ACCOUNTS_NUM" ]]; then
            echo "stress accounts already exists: $addresses_file ($existed_num)"
            return 0
        fi

        echo "已存在账户数量($existed_num)与 STRESS_ACCOUNTS_NUM($STRESS_ACCOUNTS_NUM)不一致，重新生成"
    fi

    generate_stress_accounts
}

add_stress_accounts_to_genesis() {
    local addresses_file="$STRESS_ACCOUNTS_DIR/addresses.json"
    if [[ ! -f "$addresses_file" ]]; then
        echo "错误: 未找到压力测试账户地址文件 $addresses_file" >&2
        exit 1
    fi

    echo "--------------------------------"
    echo "add $STRESS_ACCOUNTS_NUM stress accounts to genesis"
    echo "--------------------------------"

    local gen_bin="$CHAIN_STRESSER_ROOT/bin/gen-accounts"
    if [[ ! -x "$gen_bin" ]]; then
        echo "building gen-accounts..."
        (cd "$CHAIN_STRESSER_ROOT" && go build -o "$gen_bin" ./cmd/gen-accounts/)
    fi

    "$gen_bin" genesis-add \
        --addresses "$addresses_file" \
        --genesis "$WORK/biyachain-home-a/config/genesis.json" \
        --balance-byb "$STRESS_ACCOUNT_BALANCE_BYB" \
        --balance-usdt "$STRESS_ACCOUNT_BALANCE_USDT"
}

setup_environment_and_generate_keys() {
  # echo create data directory
  echo "--------------------------------"
  echo "step 1: prepare environment"
  echo "--------------------------------"

  init_workspace_templates
    ensure_stress_accounts_generated

  for node in a b c d; do
      mkdir -p "$WORK/biyachain-home-$node"
      mkdir -p "$WORK/monad-$node"
  done

  # Compose 绑定 ./biyachaind、./monad-*/id-secp；若宿主机上曾是「不存在的文件」被 Docker 建成空目录，会导致
  # "Is a directory" / "secp secret must be encoded in keystore json"。清理后再生成。
  if [[ -d "$WORK/biyachaind" ]]; then
      echo "警告: 删除误建目录 $WORK/biyachaind（将重新 go build）"
      rm -rf "$WORK/biyachaind"
  fi
  for node in a b c d; do
      for f in id-secp id-bls; do
          p="$WORK/monad-$node/$f"
          if [[ -d "$p" ]]; then
              echo "警告: 删除误建目录 $p"
              rm -rf "$p"
          fi
      done
  done

  for node in a b c d; do
    "$MONAD_BFT_ROOT/target/release/monad-keystore" create --keystore-path "$WORK/monad-$node/id-secp" --password "" --key-type secp
    "$MONAD_BFT_ROOT/target/release/monad-keystore" create --keystore-path "$WORK/monad-$node/id-bls" --password "" --key-type bls
  done


    for n in a b c d; do
        echo "=== monad-$n secp ==="
        "$MONAD_BFT_ROOT/target/release/monad-keystore" recover --keystore-path "$WORK/monad-$n/id-secp" --password "" --key-type secp
        echo "=== monad-$n bls ==="
        "$MONAD_BFT_ROOT/target/release/monad-keystore" recover --keystore-path "$WORK/monad-$n/id-bls" --password "" --key-type bls
    done

    "$MONAD_BFT_ROOT/scripts/gen-validators-toml.sh"

    for n in b c d; do
        cp "$WORK/monad-a/validators.toml" "$WORK/monad-$n/validators.toml"
    done

    echo "--------------------------------"
    echo "step 1: prepare environment done."
    echo "--------------------------------"

}

init_biyachaind() {
    echo "--------------------------------"
    echo "step 2: init biyachaind"
    echo "--------------------------------"

    # 支持单独执行 ./scripts/mult-run.sh init-biyachain：缺失时自动生成压测账户 json
    ensure_stress_accounts_generated

    # 若以 root 在容器内写过 $WORK，宿主机上 rm -rf .../* 往往删不干净，biyachaind init 也写不进 genesis.json。
    # 同时 Docker 在缺少源文件时会把 ./genesis.json.reference 建成目录，需删掉再生成文件。
    if command -v docker >/dev/null 2>&1; then
        # 先以 root 删净（含 root 属主残留），再 chown 给宿主机用户以便本机 biyachaind 写入
        docker run --rm \
            -e "HOST_UID=$(id -u)" -e "HOST_GID=$(id -g)" \
            -v "$WORK:/work:rw" alpine:3.19 sh -c \
            'rm -rf /work/genesis.json.reference \
              /work/biyachain-home-a /work/biyachain-home-b /work/biyachain-home-c /work/biyachain-home-d && \
              mkdir -p /work/biyachain-home-a /work/biyachain-home-b /work/biyachain-home-c /work/biyachain-home-d && \
              chown -R "$HOST_UID:$HOST_GID" /work/biyachain-home-a /work/biyachain-home-b /work/biyachain-home-c /work/biyachain-home-d'
    else
        echo "错误: 需要 docker 以清空可能为 root 属主的 biyachain-home-*（见 init_biyachaind 注释）。" >&2
        exit 1
    fi

    for node in a b c d; do
        "$BIYACHAIND_BIN" init monad-$node --chain-id "$CHAIN_ID" --home "$WORK/biyachain-home-$node"
    done

    for node in a b c d; do
    python3 - <<PY "$WORK/biyachain-home-$node/config/app.toml"
    import pathlib, re, sys
    path = pathlib.Path(sys.argv[1])
    text = path.read_text()
    text = re.sub(
        r'^minimum-gas-prices = ".*"$',
        'minimum-gas-prices = "1byb"',
        text,
        flags=re.MULTILINE,
    )
    path.write_text(text)
PY
    done
    echo "--------------------------------"
    echo "add genesis account"
    echo "--------------------------------"
    for node in a b c d; do
        "$BIYACHAIND_BIN" keys add val-$node --home "$WORK/biyachain-home-$node" --keyring-backend "$KEYRING"
    done
    echo "--------------------------------"
    echo "add genesis account to genesis.json."
    echo "--------------------------------"
    for node in a b c d; do
        ADDR="$("$BIYACHAIND_BIN" keys show val-$node -a --home "$WORK/biyachain-home-$node" --keyring-backend "$KEYRING")"
        "$BIYACHAIND_BIN" genesis add-genesis-account "$ADDR" "$GENESIS_BALANCE" \
            --chain-id "$CHAIN_ID" --home "$WORK/biyachain-home-a"
    done

    echo "--------------------------------"
    echo "add test user with large balance"
    echo "--------------------------------"
    TEST_USER_KEY="testuser"
    TEST_USER_MNEMONIC="copper push brief egg scan entry inform record adjust fossil boss egg comic alien upon aspect dry avoid interest fury window hint race symptom"
    NEWLINE=$'\n'
    
    # Add test user key (use same mnemonic as USER1 from setup.sh for consistency)
    if [[ "$KEYRING" == "test" ]]; then
        yes "$TEST_USER_MNEMONIC$NEWLINE" | "$BIYACHAIND_BIN" keys add $TEST_USER_KEY --recover \
            --home "$WORK/biyachain-home-a" --keyring-backend "$KEYRING"
    else
        echo "$TEST_USER_MNEMONIC" | "$BIYACHAIND_BIN" keys add $TEST_USER_KEY --recover \
            --home "$WORK/biyachain-home-a" --keyring-backend "$KEYRING"
    fi
    
    # Add test user with large balance (100M byb, 100M USDT, 10M WBTC)
    TEST_USER_ADDR="$("$BIYACHAIND_BIN" keys show $TEST_USER_KEY -a --home "$WORK/biyachain-home-a" --keyring-backend "$KEYRING")"
    "$BIYACHAIND_BIN" genesis add-genesis-account "$TEST_USER_ADDR" \
        100000000000000000000000000000000000000byb,100000000000000000000000000peggy0xdAC17F958D2ee523a2206206994597C13D831ec7,10000000000000000peggy0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599 \
        --chain-id "$CHAIN_ID" --home "$WORK/biyachain-home-a"
    
    echo "Test user created: $TEST_USER_ADDR"

    add_stress_accounts_to_genesis

    cp -a "$WORK/biyachain-home-a/config/genesis.json" "$WORK/biyachain-home-b/config/genesis.json"
    cp -a "$WORK/biyachain-home-a/config/genesis.json" "$WORK/biyachain-home-c/config/genesis.json"
    cp -a "$WORK/biyachain-home-a/config/genesis.json" "$WORK/biyachain-home-d/config/genesis.json"

    echo "--------------------------------"
    echo "add gentx to genesis.json."
    echo "--------------------------------"
    for node in a b c d; do
        "$BIYACHAIND_BIN" genesis gentx val-$node "$GENTX_STAKE" \
            --chain-id "$CHAIN_ID" --home "$WORK/biyachain-home-$node"  --keyring-backend "$KEYRING"
    done
    mkdir -p "$WORK/biyachain-home-a/config/gentx"
    cp -a "$WORK/biyachain-home-b/config/gentx/"*.json "$WORK/biyachain-home-a/config/gentx/"
    cp -a "$WORK/biyachain-home-c/config/gentx/"*.json "$WORK/biyachain-home-a/config/gentx/"
    cp -a "$WORK/biyachain-home-d/config/gentx/"*.json "$WORK/biyachain-home-a/config/gentx/"

    echo "--------------------------------"
    echo "configure exchange state (markets + denom decimals)"
    echo "--------------------------------"
    INITIAL_GENESIS_DIR="$MONAD_BFT_ROOT/biyachain-core/scripts/local-genesis"
    if [[ -d "$INITIAL_GENESIS_DIR" ]]; then
        # 先替换时间戳占位符到临时文件，再用 jq 读取（同 setup.sh 顺序）
        CURRENT_UNIX_TIMESTAMP=$(date +%s)
        NEXT_FUNDING_TIMESTAMP=$((CURRENT_UNIX_TIMESTAMP + 600))
        EXCHANGE_GENESIS_TMP=$(mktemp)
        sed "s/XXX-FUNDING-TIMESTAMP-PLACEHOLDER-XXX/${NEXT_FUNDING_TIMESTAMP}/g" \
            "$INITIAL_GENESIS_DIR/initial_exchange_genesis.json" > "$EXCHANGE_GENESIS_TMP"
        EXCHANGE_GENESIS_STATE=$(jq -r '.state' "$EXCHANGE_GENESIS_TMP")
        rm -f "$EXCHANGE_GENESIS_TMP"
        cat "$WORK/biyachain-home-a/config/genesis.json" | \
            jq '.app_state["exchange"]='"${EXCHANGE_GENESIS_STATE}" > \
            "$WORK/biyachain-home-a/config/tmp_genesis.json" && \
            mv "$WORK/biyachain-home-a/config/tmp_genesis.json" "$WORK/biyachain-home-a/config/genesis.json"

        # 注入 trading_reward_pool_campaign_schedule（与 biyachain-core/setup.sh 保持一致）。
        # 否则当 trading_reward_campaign_info 非空、schedule 为空时，exchange InitGenesis 在
        # data.TradingRewardPoolCampaignSchedule[0] 处 index out of range，导致 InitChain panic。
        CAMPAIGN_TMP=$(mktemp)
        {
            echo '['
            EPOCH_UNIX_TIMESTAMP=$CURRENT_UNIX_TIMESTAMP
            for i in $(seq 1 35); do
                EPOCH_UNIX_TIMESTAMP=$((EPOCH_UNIX_TIMESTAMP + 600))
                sep=','
                [[ $i -eq 35 ]] && sep=''
                echo '{"start_timestamp": '"$EPOCH_UNIX_TIMESTAMP"', "max_campaign_rewards": [{"denom": "byb", "amount": "1000000000000000000000"}]}'"$sep"
            done
            echo ']'
        } >"$CAMPAIGN_TMP"
        INITIAL_TRADING_CAMPAIGNS=$(cat "$CAMPAIGN_TMP")
        rm -f "$CAMPAIGN_TMP"
        cat "$WORK/biyachain-home-a/config/genesis.json" | \
            jq '.app_state["exchange"]["trading_reward_pool_campaign_schedule"]='"${INITIAL_TRADING_CAMPAIGNS}" > \
            "$WORK/biyachain-home-a/config/tmp_genesis.json" && \
            mv "$WORK/biyachain-home-a/config/tmp_genesis.json" "$WORK/biyachain-home-a/config/genesis.json"

        # devnet 不需要因 downtime 自动进入 post-only 模式（会导致链启动后 ~1000 个块内所有穿透 TOB 的限价单被拒）
        # 不能设为空（校验不通过），改为最大枚举值 DURATION_48H，devnet 运行时间极短，不会触发；
        # 同时将持续块数改为 1，即使触发也会在下一个块立即解除
        cat "$WORK/biyachain-home-a/config/genesis.json" | \
            jq '.app_state["exchange"]["params"]["min_post_only_mode_downtime_duration"]="DURATION_48H"
              | .app_state["exchange"]["params"]["post_only_mode_blocks_amount_after_downtime"]="1"' > \
            "$WORK/biyachain-home-a/config/tmp_genesis.json" && \
            mv "$WORK/biyachain-home-a/config/tmp_genesis.json" "$WORK/biyachain-home-a/config/genesis.json"

        echo "已从 $INITIAL_GENESIS_DIR 加载 exchange genesis state（含现货市场、denom decimals 和 trading rewards）"
    else
        echo "警告: 未找到 $INITIAL_GENESIS_DIR，跳过 exchange genesis 注入" >&2
    fi

    "$BIYACHAIND_BIN" genesis collect-gentxs --home "$WORK/biyachain-home-a"

    # 设置区块最大 gas 为-1
    cat "$WORK/biyachain-home-a/config/genesis.json" | \
        jq '.consensus["params"]["block"]["max_gas"]="2000000000"' > \
        "$WORK/biyachain-home-a/config/tmp_genesis.json" && \
        mv "$WORK/biyachain-home-a/config/tmp_genesis.json" "$WORK/biyachain-home-a/config/genesis.json"

    "$BIYACHAIND_BIN" genesis validate --home "$WORK/biyachain-home-a"

    cp -a "$WORK/biyachain-home-a/config/genesis.json" "$WORK/biyachain-home-b/config/genesis.json"
    cp -a "$WORK/biyachain-home-a/config/genesis.json" "$WORK/biyachain-home-c/config/genesis.json"
    cp -a "$WORK/biyachain-home-a/config/genesis.json" "$WORK/biyachain-home-d/config/genesis.json"
    "$BIYACHAIND_BIN" genesis validate --home "$WORK/biyachain-home-b"
    "$BIYACHAIND_BIN" genesis validate --home "$WORK/biyachain-home-c"
    "$BIYACHAIND_BIN" genesis validate --home "$WORK/biyachain-home-d"

    if [[ -d "$WORK/genesis.json.reference" ]]; then
        docker run --rm -v "$WORK:/work:rw" alpine:3.19 rm -rf /work/genesis.json.reference
    fi
    cp -a "$WORK/biyachain-home-a/config/genesis.json" "$WORK/genesis.json.reference"
    # Compose 会把「文件」./genesis.json.reference 挂到 /monad/genesis.json；若宿主机 monad-* 里误存在
    # 同名目录（Docker 曾对不存在的源路径建过目录），会与文件挂载冲突并报 OCI mount 错。
    if command -v docker >/dev/null 2>&1; then
        docker run --rm -v "$WORK:/work:rw" alpine:3.19 sh -c \
            'for n in a b c d; do p="/work/monad-$n/genesis.json"; [ -d "$p" ] && rm -rf "$p"; done'
    fi
    echo "--------------------------------"
    echo "step 2: init biyachaind done."
    echo "--------------------------------"
}

init_monad_node() {
    echo "--------------------------------"
    echo "step 3: init monad-node"
    echo "--------------------------------"
    if command -v docker >/dev/null 2>&1; then
        docker run --rm -v "$WORK:/work:rw" alpine:3.19 sh -c \
            'for n in a b c d; do p="/work/monad-$n/genesis.json"; [ -d "$p" ] && rm -rf "$p"; done'
    fi
    NODE_TOML_SRC="$WORK/node.toml"
    if [[ ! -f "$NODE_TOML_SRC" ]]; then
        NODE_TOML_SRC="$MONAD_BFT_ROOT/data-monad-multinode/node.toml"
    fi
    if [[ ! -f "$NODE_TOML_SRC" ]]; then
        NODE_TOML_SRC="$MONAD_BFT_ROOT/docker/devnet/monad/config/node.toml"
    fi
    if [[ ! -f "$NODE_TOML_SRC" ]]; then
        echo "错误: 找不到 node.toml 模板（已试 \$WORK/node.toml、.monad/node.toml、docker/devnet/...）" >&2
        exit 1
    fi
    for n in a b c d; do
        M="$WORK/monad-$n"
        mkdir -p "$M/ledger/headers" "$M/ledger/bodies" "$M/ledger/cosmos-commits"
        cp "$NODE_TOML_SRC" "$M/node.toml"
    done

    # Compose --validators-path /monad/validators.toml；清理数据后常丢失，须与各 id-secp 一致。
    if [[ ! -f "$WORK/monad-a/validators.toml" ]]; then
        echo "未找到 $WORK/monad-a/validators.toml，正在从 keystore 生成…"
        KEYSTORE_BIN="$MONAD_BFT_ROOT/target/release/monad-keystore" "$(dirname "$0")/gen-validators-toml.sh" || exit 1
    fi
    for n in a b c d; do
        cp -a "$WORK/monad-a/validators.toml" "$WORK/monad-$n/validators.toml"
    done

    # Compose 使用 --forkpoint-config /monad/forkpoint.toml；缺文件会报 local_err=ENOENT 且 REMOTE 未设时直接退出。
    # 默认用仓库 genesis forkpoint；多验证者生产场景请换与 validators.toml 配套的 forkpoint（见 docs）。
    FP_GENESIS="${FORKPOINT_GENESIS:-$MONAD_BFT_ROOT/docker/devnet/monad/config/forkpoint.genesis.toml}"
    if [[ ! -f "$FP_GENESIS" ]]; then
        echo "错误: 找不到 forkpoint 模板 $FP_GENESIS" >&2
        exit 1
    fi
    for n in a b c d; do
        cp "$FP_GENESIS" "$WORK/monad-$n/forkpoint.toml"
        printf '%s\n' 'peers = []' >"$WORK/monad-$n/peers.toml"
    done

    recover_secp_pubhex() {
        local dir="$1" raw
        raw="$(
            "$MONAD_BFT_ROOT/target/release/monad-keystore" recover --keystore-path "$dir/id-secp" --password "" --key-type secp 2>&1 \
                | awk -F': ' '$0 ~ /Secp public key/ { gsub(/^[[:space:]]+|[[:space:]]+$/, "", $2); print $2; exit }'
        )"
        if [[ -z "$raw" ]]; then
            echo "错误: 无法从 $dir/id-secp 解析 Secp 公钥" >&2
            return 1
        fi
        [[ "$raw" =~ ^0x ]] && raw="${raw#0x}"
        printf '0x%s' "$(printf '%s' "$raw" | tr 'A-F' 'a-f')"
    }

    # 与 compose 中 monad_net 固定 IP 对齐；勿用 echo|od 取字母 ASCII（echo 带换行会把 od 多字节拼成 9710 之类脏数）
    declare -A P2P_ADDR P2P_SIG P2P_PUB
    for n in a b c d; do
        ascii=$(LC_ALL=C printf '%d' "'$n")
        octet=$((10 + (ascii - 97) * 10))
        addr="172.28.0.${octet}:8000"
        MONAD_DIR="$WORK/monad-$n"
        out="$(
            "$MONAD_BFT_ROOT/target/release/sign-name-record" \
                --address "$addr" \
                --authenticated-udp-port 8001 \
                --self-record-seq-num 0 \
                --keystore-path "$MONAD_DIR/id-secp" \
                --password "" 2>&1
        )"
        sig=$(printf '%s\n' "$out" | sed -n 's/^self_name_record_sig = "\(.*\)"$/\1/p')
        if [[ -z "$sig" ]]; then
            echo "$out" >&2
            echo "sign-name-record failed for $n" >&2
            exit 1
        fi
        pub=$(recover_secp_pubhex "$MONAD_DIR") || exit 1
        P2P_ADDR[$n]="$addr"
        P2P_SIG[$n]="$sig"
        P2P_PUB[$n]="$pub"
        python3 -c "
import re, pathlib, sys
path, addr, sig = pathlib.Path(sys.argv[1]), sys.argv[2], sys.argv[3]
t = path.read_text()
t, n1 = re.subn(r'^self_address = .*$', 'self_address = \"' + addr + '\"', t, count=1, flags=re.M)
t, n2 = re.subn(r'^self_name_record_sig = .*$', 'self_name_record_sig = \"' + sig + '\"', t, count=1, flags=re.M)
assert n1 == 1 and n2 == 1, (n1, n2)
path.write_text(t)
" "$MONAD_DIR/node.toml" "$addr" "$sig"
    done

    # 无 [[bootstrap.peers]] 时 peer discovery 难以建立路由，Raptorcast 会持续 WARN unknown address for node …
    for n in a b c d; do
        bootf="$(mktemp)"
        for o in a b c d; do
            [[ "$o" == "$n" ]] && continue
            {
                echo "[[bootstrap.peers]]"
                echo "address = \"${P2P_ADDR[$o]}\""
                echo "record_seq_num = 0"
                echo "secp256k1_pubkey = \"${P2P_PUB[$o]}\""
                echo "name_record_sig = \"${P2P_SIG[$o]}\""
                echo "auth_port = 8001"
                echo ""
            } >>"$bootf"
        done
        python3 -c "
import pathlib, re, sys
path, boot_path = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
boot = boot_path.read_text()
text = path.read_text()
pat = re.compile(r'^\[bootstrap\].*?^\[peer_discovery\]', re.MULTILINE | re.DOTALL)
new = '[bootstrap]\\n\\n' + boot + '[peer_discovery]'
text, c = pat.subn(new, text, count=1)
assert c == 1, ('[bootstrap]…[peer_discovery] replace failed', c)
path.write_text(text)
" "$WORK/monad-$n/node.toml" "$bootf"
        rm -f "$bootf"
    done

    echo "--------------------------------" 
    echo "step 3: init monad-node done."
    echo "--------------------------------"
}

# 防止在未跑完 mult-run 就先 compose，导致 Docker 把绑定源建成目录。
verify_compose_mount_sources() {
    local err=0
    for n in a b c d; do
        for f in id-secp id-bls; do
            p="$WORK/monad-$n/$f"
            if [[ ! -f "$p" ]] || [[ -d "$p" ]]; then
                echo "错误: $p 必须为非空 keystore 文件。" >&2
                err=1
            fi
        done
        v="$WORK/monad-$n/validators.toml"
        if [[ ! -f "$v" ]] || [[ -d "$v" ]]; then
            echo "错误: $v 必须存在（Compose 挂载 --validators-path /monad/validators.toml）。" >&2
            err=1
        fi
    done
    if [[ $err -ne 0 ]]; then
        echo "若曾为目录: rm -rf \"$WORK/biyachaind\" 及各 monad-*/id-secp、id-bls 目录后重新执行本脚本。" >&2
        exit 1
    fi
}

# 调用方法：
#   ./scripts/mult-run.sh                    — 全流程（会重建密钥与链目录）
#   ./scripts/mult-run.sh init-biyachain     — 仅 init_biyachaind
#   ./scripts/mult-run.sh init-monad-node    — 仅 init_monad_node（P2P 自签 + bootstrap，不重建密钥）

if [[ "${1:-}" == "init-biyachain" ]]; then
    init_biyachaind
    exit 0
fi

if [[ "${1:-}" == "init-monad-node" ]]; then
    init_monad_node
    verify_compose_mount_sources
    exit 0
fi

setup_environment_and_generate_keys
echo "--------------------------------"
echo "need edit monad-node validators.toml"
init_biyachaind

init_monad_node

verify_compose_mount_sources
echo "可在 $WORK 执行: docker compose up -d"

