# Cosmos txpool 防止流水线重复打包完整开发方案

## 背景

Monad-BFT 使用流水线共识时，当前区块打包完成后，共识会继续为后续 round 创建新区块。
如果上一个区块尚未 finalized/committed，`biyachain-core` / `block-sdk` 应用层 mempool 中的交易
还没有被 `FinalizeBlock` / `Commit` 删除。此时下一次 `PrepareProposal` 仍可能从应用层 mempool
选出同一批交易，导致连续两个 proposal 打包重复交易。

当前 Cosmos 路径：

1. `monad-consensus-state` 发出 `CreateProposal`，里面已经带有 `extending_blocks`。
2. `monad-txpool/src/executor.rs` 收到后调用 ABCI `PrepareProposal`。
3. `prepare_request_from_header(&header, Vec::new())` 当前把 `PrepareProposalRequest.txs` 置为空。
4. `block-sdk/abci/abci.go` 的 `PrepareProposalHandler` 忽略 `req.Txs`，从应用层 laned mempool 打包。

问题的本质是：应用层 mempool 不知道当前新区块要扩展的未提交父链里已经用了哪些账户 nonce。

## 设计目标

一次性完成完整方案，不做临时 raw tx hash 过滤，不分阶段。

目标是把 EVM txpool 的核心思路迁移到 Cosmos/block-sdk 路径：

- Monad-BFT 在创建 proposal 时，把 `extending_blocks` 中的交易作为“父链已占用交易集合”传给 ABCI
  `PrepareProposal`。
- 应用层解析这些父链交易的 signer/nonce，得到 reserved nonce 集合。
- block-sdk 仍然从自己的应用层 mempool 打包新区块交易，但打包时跳过已经被父链占用的 nonce。
- `req.Txs` 只作为父链上下文输入，不直接进入新区块 proposal body。

## EVM txpool 参考

EVM 路径不是等 commit 后才避免重复，而是在 proposal 创建时就参考未提交父链：

- `monad-consensus-state/src/lib.rs` 在 `CreateProposal` 里传递 `extending_blocks`。
- `monad-eth-txpool-executor/src/lib.rs` 调用 `EthTxPool::create_proposal(..., extending_blocks, ...)`。
- `monad-eth-txpool/src/pool/sequencer.rs` 的 `ProposalSequencer::new` 调
  `extending_blocks.get_nonce_usages()`。
- `monad-eth-block-policy/src/nonce_usage.rs` 合并未提交父链上的 `NonceUsageMap`。
- txpool 排序时基于合并后的账户 nonce 继续选下一笔交易。

Cosmos 路径也应该在创建 proposal 时使用 `extending_blocks`，只是 nonce usage 由应用层解析
Cosmos tx 得到，而不是 Monad Rust 侧解析 EVM tx。

## 总体方案

### 1. Monad 侧：把 extending_blocks 交易填入 PrepareProposalRequest.txs

修改 `monad-txpool/src/executor.rs` 的 `TxPoolCommand::CreateProposal` 分支。

当前代码：

```rust
...
let prepare_request = prepare_request_from_header(&header, Vec::new());
```

改为实际接收 `extending_blocks`，并把待扩展父链交易放入 ABCI 请求：

```rust
extending_blocks,
...
let reserved_parent_txs = collect_extending_block_txs(&extending_blocks);
let prepare_request = prepare_request_from_header(&header, reserved_parent_txs);
```

`collect_extending_block_txs` 逻辑：

```rust
fn collect_extending_block_txs<...>(extending_blocks: &[BPT::ValidatedBlock]) -> Vec<Vec<u8>> {
    extending_blocks
        .iter()
        .flat_map(|block| block.body().execution_body.txs.iter().cloned())
        .collect()
}
```

这些 tx 是“当前新区块要扩展的未提交父链交易”。它们不是新区块候选交易，只是传给应用层做
reserved nonce 分析。

Monad 侧不对 `PrepareProposalResponse.Txs` 做 `tx_limit` / `proposal_byte_limit` 二次裁剪。
交易数量、字节数、gas、lane limit 等打包限制由应用层 `PrepareProposal` / block-sdk 根据
`req.MaxTxBytes` 和链上参数负责。共识层本地创建 proposal 时只负责传递父链上下文并使用应用层
返回的 tx 列表；验证路径仍通过 `ProcessProposal` / coherency 检查拒绝非法超限 proposal。

注意：ABCI `PrepareProposalRequest.txs` 原本在 CometBFT 语义里是共识层 mempool 预选 tx。
现在 `biyachain` 使用应用层 mempool，原字段为空且未使用。这里复用该字段作为“父链 reserved
txs”，需要在 block-sdk/biyachain 侧明确改变语义：`req.Txs` 不再表示新区块候选交易。

### 2. block-sdk 侧：PrepareProposal 从 req.Txs 构建 reserved nonce 集

修改 `block-sdk/abci/abci.go` 的 `PrepareProposalHandler`：

当前创建空 proposal：

```go
proposal := proposals.NewProposal(h.logger, req.MaxTxBytes, maxGasLimit)
prepareLanesHandler := ChainPrepareLanes(h.mempool.Registry())
finalProposal, err := prepareLanesHandler(ctx, proposal)
```

新增 reserved nonce 构建：

```go
reserved, err := BuildReservedNonceSet(ctx, h.mempool.Registry(), req.Txs)
if err != nil {
    h.logger.Error("failed to build reserved nonce set", "err", err, "height", req.Height)
    return &abci.PrepareProposalResponse{Txs: make([][]byte, 0)}, err
}

ctx = WithReservedNonceSet(ctx, reserved)
proposal := proposals.NewProposal(h.logger, req.MaxTxBytes, maxGasLimit)
prepareLanesHandler := ChainPrepareLanes(h.mempool.Registry())
finalProposal, err := prepareLanesHandler(ctx, proposal)
```

`BuildReservedNonceSet` 做三件事：

1. 对 `req.Txs` 中每个父链 tx 解码。
2. 用 lane 的 `SignerExtractor` 或 Cosmos SDK `GetSigners/GetSignaturesV2` 解析 signer/sequence。
3. 写入 `map[sender]ReservedNonceUsage`。

推荐数据结构：

```go
type ReservedNonceSet struct {
    bySender map[string]map[uint64]struct{}
}

func (r ReservedNonceSet) Contains(sender string, nonce uint64) bool
func (r ReservedNonceSet) HighestContiguousFrom(sender string, base uint64) uint64
func (r ReservedNonceSet) Empty() bool
```

完整实现必须支持 `ApplyToExpected(sender, expected)`：从 committed account sequence 开始，
连续跳过父链 reserved nonce，得到当前新区块应该选择的第一个 nonce。这样才能在父链已经使用
nonce 10 时，让新区块继续选择 nonce 11，而不是只过滤 nonce 10 后出空或跳过整个 sender。

### 3. PrepareLane：从应用层 mempool 打包，但跳过 reserved nonce

修改 `block-sdk/block/base/proposals.go` 中两个 prepare handler：

- `exchangePrepareLaneHandler`
- `DefaultProposalHandler.PrepareLaneHandler`

它们当前都从 mempool 迭代：

```go
iterator := h.lane.Select(ctx, nil)
```

并通过 `senderInfoGetter` 拿到 mempool index 中的 sender/nonce：

```go
if sig, ok := iterator.(senderInfoGetter); ok {
    senderStr, senderNonce = sig.SenderInfo()
}
```

新增逻辑：对每个候选 tx，用 committed state、reserved nonce set、以及本 proposal 已选择 nonce
共同计算 expected nonce。父链中已经出现的同 sender/nonce 视为已消耗，不放入新区块；父链之后的
连续 nonce 可以继续打包。

```go
reserved := ReservedNonceSetFromContext(ctx)
expected := expectedNonce(ctx, senderStr, selectedNonceBySender)
expected = reserved.ApplyToExpected(senderStr, expected)

switch {
case senderNonce < expected:
    h.lane.Logger().Debug(
        "skip tx already consumed by committed or extending blocks",
        "lane", h.lane.Name(),
        "sender", senderStr,
        "nonce", senderNonce,
        "tx_hash", utils.TxHash(txInfo.TxBytes),
    )
    continue
case senderNonce > expected:
    markSkippedSender(senderStr)
    continue
}
```

这里不能简单地在命中 reserved nonce 后 `markSkippedSender(senderStr)`，否则父链 nonce 10 已经被
占用时，新区块会连 nonce 11 也跳过，吞吐下降且没有完整复刻 EVM 的行为。

### 4. fastNonceVerifier 必须叠加 reserved nonce

当前 block-sdk 已经有 `FastNonceVerifier`：

```go
type FastNonceVerifier func(ctx sdk.Context, sender string, nonce uint64) (cmp int, err error)
```

它通常基于 committed state 判断：

- `cmp < 0`：tx nonce 小于当前账户 sequence，是 stale。
- `cmp > 0`：tx nonce 大于当前账户 sequence，是 future。
- `cmp == 0`：可选。

为了让新区块能在父链 nonce 后继续选交易，必须在 verifier 内叠加 reserved nonce set：

```go
func WrapFastNonceVerifier(base FastNonceVerifier) FastNonceVerifier {
    return func(ctx sdk.Context, sender string, nonce uint64) (int, error) {
        if sender == FastNonceFlushSender {
            return base(ctx, sender, nonce)
        }

        cmp, err := base(ctx, sender, nonce)
        if err != nil || cmp < 0 {
            return cmp, err
        }

        reserved := ReservedNonceSetFromContext(ctx)
        expected := nonce
        if cmp > 0 {
            // base expected nonce is below candidate nonce. Advance it through reserved parent nonces.
            baseExpected, ok := ExpectedNonceFromVerifierOrState(ctx, sender)
            if !ok {
                return cmp, nil
            }
            expected = reserved.ApplyToExpected(sender, baseExpected)
        } else {
            expected = reserved.ApplyToExpected(sender, nonce)
        }

        switch {
        case nonce < expected:
            return -1, nil
        case nonce > expected:
            return 1, nil
        default:
            return 0, nil
        }
    }
}
```

更简单、更稳定的实现方式：不强行从 `base` 推导 expected nonce，而是在 biyachain-core 中实现一个
aware verifier，直接读取账户 committed sequence，再调用：

```go
expected := accountKeeper.GetSequence(ctx, senderAddr)
expected = reserved.ApplyToExpected(sender, expected)
```

`ApplyToExpected`：

```go
func (r ReservedNonceSet) ApplyToExpected(sender string, expected uint64) uint64 {
    used := r.bySender[sender]
    for {
        if _, ok := used[expected]; !ok {
            return expected
        }
        expected++
    }
}
```

这样：

- committed expected = 10。
- extending parent 已有 nonce 10。
- 当前新区块打包时 expected 被推进到 11。
- mempool 中 nonce 10 会被视为 stale/reserved，不进入新区块。
- mempool 中 nonce 11 可通过校验并进入新区块。

这才是完整复刻 EVM `extending_blocks.get_nonce_usages()` 的关键。

### 5. 交易选择时还要维护本区块内 selected nonce

除了父链 reserved nonce，还要继续保留 block-sdk 现有的本区块内连续 nonce 逻辑。

当前 `DefaultProposalHandler.PrepareLaneHandler` 已经通过以下机制处理一部分：

- `skippedSenders`
- `fastNonceVerifier`
- `proposal.Contains(txInfo.Key())`

但引入 reserved 后，推荐显式维护：

```go
selectedNonceBySender := map[string]uint64{}
```

对每个候选 tx：

1. `expected := account sequence + reserved contiguous usage + selected contiguous usage`。
2. 如果 `senderNonce < expected`：stale，移除或跳过。
3. 如果 `senderNonce > expected`：future，`markSkippedSender(sender)`。
4. 如果相等：可选，成功加入 proposal 后 `selectedNonceBySender[sender] = senderNonce`。

这能同时处理：

- committed state。
- extending_blocks 父链交易。
- 当前 proposal 已选交易。

如果继续复用 `fastNonceVerifier`，则它需要感知 reserved + selected；否则多笔同 sender 交易可能在父链
nonce 后不能连续打包。推荐最终实现以显式 expected nonce 计算为准，`fastNonceVerifier` 只作为
读取 committed sequence / 清理缓存的辅助。

### 6. req.Txs 不能进入 finalProposal.Txs

`req.Txs` 在新语义下仅用于 reserved nonce 分析。`PrepareProposalResponse.Txs` 必须只包含
block-sdk 从应用层 mempool 选出来的新交易。

需要特别检查：

- `req.Height <= 1` 的 genesis/首块特殊逻辑可以保持原样，或明确只在没有 Monad reserved 语义时使用。
- `NoOpPrepareProposal` 或没有 laned mempool 的 fallback 不能在 Monad 路径下把 `req.Txs` 原样返回，
  否则会把父链交易再次打包。

推荐在 `biyachain` 使用 Monad-BFT 时增加配置开关：

```go
UsePrepareProposalTxsAsReserved bool
```

开启后：

- `req.Txs` 仅作为 reserved 输入。
- fallback 不返回 `req.Txs`。
- 如果 block-sdk mempool 不可用，返回空 proposal，而不是返回父链 tx。

## 代码改动清单

### monad-txpool

文件：`monad-txpool/src/executor.rs`

- `CreateProposal` match 中保留 `extending_blocks`。
- 新增 `collect_extending_block_txs`。
- 调 `prepare_request_from_header(&header, reserved_parent_txs)`。
- `PrepareProposal` 返回后不做二次交易限制，直接构造 `CosmosBlockBody`。
- 日志增加：
  - `reserved_parent_blocks`
  - `reserved_parent_txs`
  - `n_prepare_returned`
  - `returned_bytes`

### block-sdk

文件：`block-sdk/abci/abci.go`

- `PrepareProposalHandler` 解析 `req.Txs` 为 `ReservedNonceSet`。
- 将 reserved set 写入 `ctx`。
- Monad reserved 模式下，fallback 不返回 `req.Txs`。

文件：`block-sdk/block/base/proposals.go`

- `exchangePrepareLaneHandler` 支持 `ReservedNonceSetFromContext(ctx)`。
- `DefaultProposalHandler.PrepareLaneHandler` 支持 reserved nonce。
- fast nonce verifier 或显式 expected nonce 计算叠加 reserved + selected。

文件：`block-sdk/block/base/reserved_nonce.go`（新增）

- `ReservedNonceSet`。
- context helper。
- tx 解码和 signer/sequence 提取 helper。
- `ApplyToExpected`。

### biyachain-core

- 构造 block-sdk proposal handler 时打开 Monad reserved 语义。
- 如果当前 fast nonce verifier 在 biyachain-core 中实现，则改为读取 `ReservedNonceSetFromContext(ctx)`，
  将父链 nonce 应用到账户 expected sequence 上。
- 确保 PrepareProposal 不写 finalize-only 状态，不污染 mempool 以外的应用状态。

## ProcessProposal / FinalizeBlock 影响

`ProcessProposal` 不需要读取 `req.Txs`，因为网络 proposal body 只包含新区块交易。

验证者在收到新区块时，会按区块本身执行 `ProcessProposal`。父链交易已经包含在 block tree 的
extending chain 中，不需要随 proposal body 再传。只要 proposer 的打包逻辑正确跳过父链 nonce，
验证者执行新区块交易时就不会遇到重复 nonce。

`FinalizeBlock` 仍按最终提交的 block body 执行和删除 mempool 交易。父链未提交时不删除应用层
mempool；防重由 PrepareProposal 的 reserved nonce 上下文解决。

## 边界条件

### 分叉

`extending_blocks` 是当前 proposal 实际要扩展的父链。因此不同分叉会传入不同父链 tx 集。
应用层每次 PrepareProposal 只根据本次请求的 `req.Txs` 构建临时 reserved set，不保存全局
in-flight 状态，所以天然支持分叉，不需要 release/rollback。

### 多 signer 交易

父链 tx 解析 reserved nonce 时必须处理多 signer：

- 每个 signer 的 sequence 都加入 reserved set。
- 新 tx 如果包含多个 signer，所有 signer 的 expected nonce 都必须通过。

如果性能担心，可以先对单 signer 走 fast path，多 signer 回退完整 signer 解析。

### 父链 tx 解码失败

理论上 extending block 已通过共识校验，tx 应可解码。若解码失败，说明本地应用和共识状态不一致。
推荐直接让 `PrepareProposal` 返回错误或空块并打 error 日志，不要忽略，否则可能重复打包。

### req.Txs 大小

`extending_blocks` 交易数量受流水线深度约束，通常是最近若干未提交块。仍需打日志观察
`reserved_parent_txs` 和请求字节数。

如果 ABCI 请求过大，可以只传 reserved nonce metadata，而不是完整 tx。但当前用户前提是
`PrepareProposal` 可以传完整区块交易，且字段未使用，所以先传完整 tx，避免扩 proto。

### 首块 / 无 mempool fallback

首块 `req.Height <= 1` 当前直接返回 `req.Txs`。在 Monad reserved 模式下要谨慎：

- 如果首块没有 extending tx，行为不变。
- 如果任何情况下 `req.Txs` 被用作 reserved，都不能原样返回。

## 测试计划

### Rust / monad-txpool

1. 构造两个 `extending_blocks`，每个包含若干 tx。
2. 触发 `CreateProposal`。
3. 断言传给 `prepare_request_from_header` 的 txs 等于 extending blocks tx 合集。
4. 断言 `PrepareProposalResponse.Txs` 不被 Monad 侧二次裁剪，原样进入 `CosmosBlockBody`。

### Go / block-sdk

1. `ReservedNonceSet` 单测：
   - 单 signer tx 提取 sender/nonce。
   - 多 signer tx 提取所有 signer/nonce。
   - `ApplyToExpected` 能跳过连续 reserved nonce。
2. `PrepareProposalHandler` 单测：
   - `req.Txs` 中包含父链 tx A(sender S, nonce 10)。
   - mempool 中有 A 和 B(sender S, nonce 11)。
   - committed sequence 为 10。
   - `PrepareProposalResponse.Txs` 不包含 A，包含 B。
3. 分叉单测：
   - 请求 1 的 `req.Txs` 包含 nonce 10，能选 nonce 11。
   - 请求 2 的 `req.Txs` 为空，只能选 nonce 10。
   - 两次请求互不污染。
4. fallback 单测：
   - Monad reserved 模式开启时，没有 app mempool 不返回 `req.Txs`。

### 集成测试

1. 启动 Monad-BFT + biyachain-core。
2. 提交同一账户连续 nonce 交易。
3. 制造流水线连续 proposal，前一个 block 未 finalized 时创建后一个 block。
4. 断言后一个 block 不重复前一个 block 的 tx，而是继续选择后续 nonce。
5. 观察日志：
   - `reserved_parent_blocks > 0`
   - `reserved_parent_txs > 0`
   - `PrepareProposalResponse.Txs` 中无父链重复 tx hash。

## 结论

这个方案可行，而且比 Monad 侧维护全局 in-flight raw hash 更干净。

关键点是：`PrepareProposalRequest.txs` 在当前 biyachain 应用层 mempool 模式下没有作为候选交易使用，
可以复用为 `extending_blocks` 父链交易输入；应用层只解析这些交易的 signer/nonce，作为临时
reserved nonce 上下文参与本次 mempool 打包，不把这些 tx 放入新区块。

完整开发必须同时改 Monad 侧传参和 block-sdk/biyachain 侧 PrepareProposal 逻辑。只传
`extending_blocks` 但不改 block-sdk 选择逻辑没有效果；只在 Monad 侧过滤 raw hash 又不能选择父链
nonce 后续交易。最终形态应与 EVM 一致：新区块基于未提交父链的 nonce usage 继续打包。
