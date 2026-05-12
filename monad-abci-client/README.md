# ABCI 客户端模块 (monad-abci-client)

## 概述

`monad-abci-client` 是一个独立的 ABCI 客户端模块，用于与应用层（ABCI 服务器）通信。它遵循 CometBFT 的接口设计，但使用 Rust 异步/等待模式。

## 架构

### 接口分层（参考 CometBFT）

模块提供四个主要的 trait 接口，分别对应不同的执行阶段：

```
AbciClientConsensus  - 共识阶段（InitChain, PrepareProposal, FinalizeBlock, Commit 等）
AbciClientMempool    - Mempool 阶段（CheckTx）
AbciClientQuery      - 查询阶段（Info, Query, Echo）
AbciClientSnapshot   - 状态同步阶段（ListSnapshots, OfferSnapshot 等）
```

### 传输实现

支持多种传输方式：

- **GRPC**: 推荐生产环境使用
- **Unix Socket**: 用于本地高性能通信
- **TCP Socket**: 支持远程连接

## 使用示例

### 使用 GRPC 客户端

```rust
use monad_abci_client::{GrpcAbciClient, AbciClientMempool, AbciClientConsensus};
use monad_cometbft_proto::cometbft::abci::v1::{CheckTxRequest, InitChainRequest};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 创建 GRPC 客户端
    let client = GrpcAbciClient::new("http://localhost:26658").await?;
    
    // 调用 CheckTx 验证交易
    let req = CheckTxRequest {
        tx: vec![1, 2, 3, 4],
        ..Default::default()
    };
    let resp = client.check_tx(req).await?;
    println!("CheckTx response code: {}", resp.code);
    
    Ok(())
}
```

### 使用 Socket 客户端

```rust
use monad_abci_client::{SocketAbciClient, AbciClientConsensus};
use monad_cometbft_proto::cometbft::abci::v1::InfoRequest;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 创建 Unix socket 客户端
    let client = SocketAbciClient::new("/tmp/abci.sock");
    
    // 调用 Info 查询应用信息
    let resp = client.info(InfoRequest::default()).await?;
    println!("Last block height: {}", resp.last_block_height);
    
    Ok(())
}
```

### 自动选择传输

```rust
use monad_abci_client::AbciTransportConfig;

let config = AbciTransportConfig::from_endpoint("unix:///tmp/abci.sock");
// 或
let config = AbciTransportConfig::from_endpoint("http://localhost:26658");
// 或
let config = AbciTransportConfig::from_endpoint("tcp://localhost:26658");
```

## 接口文档

### AbciClientConsensus

共识阶段的 ABCI 方法：

- `init_chain(req)` - 初始化（仅在创世时调用一次）
- `prepare_proposal(req)` - 提案生成时选择/修改交易
- `process_proposal(req)` - 接收网络提案时验证
- `extend_vote(req)` - 扩展投票数据
- `verify_vote_extension(req)` - 验证投票扩展
- `finalize_block(req)` - 执行区块中的所有交易
- `commit(req)` - 提交状态到磁盘

### AbciClientMempool

Mempool 阶段的方法：

- `check_tx(req)` - 验证单个交易（快速路径）
- `flush()` - 刷新任何待处理数据

### AbciClientQuery

查询阶段的方法：

- `echo(req)` - 回声测试
- `info(req)` - 获取应用信息（高度、app_hash 等）
- `query(req)` - 应用特定的查询接口

### AbciClientSnapshot

状态同步的方法：

- `list_snapshots(req)` - 列出可用的快照
- `offer_snapshot(req)` - 提供快照
- `load_snapshot_chunk(req)` - 加载快照块
- `apply_snapshot_chunk(req)` - 应用快照块

## 与现有代码的关系

### 从 monad-cosmos-integration 迁移

在 `monad-cosmos-integration` 中，ABCI 调用散落在代码中：

```rust
// 旧：分散的函数
pub async fn check_tx_via_transport(endpoint: &str, tx_bytes: &[u8]) -> Result<CheckTxResponse>
pub async fn prepare_proposal_via_transport(endpoint: &str, req: PrepareProposalRequest) -> Result<PrepareProposalResponse>
```

新的模块化方式：

```rust
// 新：集中的接口
let client = SocketAbciClient::new(endpoint);
let resp = client.check_tx(req).await?;
let resp = client.prepare_proposal(req).await?;
```

### 建议的重构步骤

1. 在 `monad-abci-client` 中实现所有 ABCI 接口
2. 在 `monad-cosmos-integration` 的 `Cargo.toml` 中添加依赖：
   ```toml
   monad-abci-client = { path = "../monad-abci-client" }
   ```
3. 将现有的 `check_tx_via_transport` 等函数替换为使用 `monad-abci-client`
4. 删除重复的 transport 实现代码

## 设计对比

### CometBFT (Go) vs monad-abci-client (Rust)

```
CometBFT                           monad-abci-client
├─ AppConnConsensus                ├─ AbciClientConsensus
├─ AppConnMempool                  ├─ AbciClientMempool
├─ AppConnQuery                    ├─ AbciClientQuery
└─ AppConnSnapshot                 └─ AbciClientSnapshot

Transport implementations:          Transport implementations:
├─ GRPC                             ├─ GRPC (tonic)
├─ Socket (CometBFT内置)            ├─ Socket (Unix/TCP)
└─ TCP                              └─ (统一支持)
```

## 错误处理

所有方法返回 `Result<T>` 类型，其中错误是 `AbciClientError`：

```rust
pub enum AbciClientError {
    InvalidEndpoint(String),
    Transport(String),
    GrpcStatus(String),
    Encode(EncodeError),
    Decode(DecodeError),
    ProposalRejected,
    Io(io::Error),
    Timeout,
    ConnectionClosed,
}
```

## 性能考虑

### GRPC vs Socket

| 方面 | GRPC | Socket |
|------|------|--------|
| 吞吐量 | ⭐⭐⭐ | ⭐⭐⭐⭐ |
| 延迟 | ⭐⭐ | ⭐⭐⭐⭐ |
| 跨网络 | ✓ | ✗ |
| 本地通信 | ✓ | ✓ (推荐) |
| 设置复杂度 | 简单 | 简单 |

### CheckTx 优化

对于 Mempool 阶段的 CheckTx，考虑：

1. **连接池**：复用连接降低开销
2. **并发限制**：通过 `AbciClientMempool::flush()` 同步
3. **超时设置**：快速失败避免阻塞

## 扩展点

### 添加新的传输方式

实现上述四个 trait 即可支持新的传输方式。例如，添加 WebSocket 支持：

```rust
pub struct WebSocketAbciClient { ... }

#[async_trait::async_trait]
impl AbciClientConsensus for WebSocketAbciClient {
    // 实现所有方法
}
```

### 添加中间件

可以在现有实现外包装中间件，例如重试逻辑：

```rust
pub struct RetryAbciClient<T: AbciClientConsensus> {
    inner: T,
    max_retries: u32,
}

#[async_trait::async_trait]
impl<T: AbciClientConsensus> AbciClientConsensus for RetryAbciClient<T> {
    async fn check_tx(&self, req: CheckTxRequest) -> Result<CheckTxResponse> {
        // 添加重试逻辑
    }
}
```

## 测试

模块包含基本的实现。建议添加集成测试：

```bash
cargo test --lib monad-abci-client
```

## 许可证

GNU General Public License v3.0 或更高版本

