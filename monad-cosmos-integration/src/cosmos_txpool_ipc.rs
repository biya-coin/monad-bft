//! Unix socket ingress for **Cosmos SDK raw transaction bytes** into `monad-node`, using the same
//! framing style as `monad-eth-txpool-ipc`: length-delimited frames, first frame is a bincode
//! snapshot, subsequent client→server frames are RLP-encoded [`CosmosTxPoolIpcTx`] (raw protobuf
//! `Tx` / `TxRaw` bytes as produced by `biyachaind tx ... --generate-only` + signing).
//!
//! This is the Cosmos analogue of `EthTxPoolIpcClient` → `EthTxPoolIpcServer` → `InsertForwardedTxs`.

use std::io::{self, ErrorKind};
use std::path::Path;

use alloy_rlp::{RlpDecodable, RlpEncodable};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use tracing::{info, warn};

fn build_length_delimited_codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .max_frame_length(64 * 1024 * 1024)
        .new_codec()
}

/// Handshake payload (mirrors `EthTxPoolSnapshot` role). Empty for now.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CosmosTxPoolSnapshot {}

/// One transaction on the wire: raw bytes as CometBFT / SDK expect in `FinalizeBlock.txs`.
#[derive(Debug, Clone, RlpEncodable, RlpDecodable)]
pub struct CosmosTxPoolIpcTx {
    pub tx: Vec<u8>,
}

/// Connect, read snapshot, send one or more raw txs (same connection semantics as the ETH bridge).
pub async fn feed_raw_txs(path: impl AsRef<Path>, txs: Vec<Vec<u8>>) -> io::Result<()> {
    let path = path.as_ref();
    let stream = UnixStream::connect(path).await?;
    let mut framed = Framed::new(stream, build_length_delimited_codec());

    let snapshot_bytes = framed
        .next()
        .await
        .ok_or_else(|| io::Error::new(ErrorKind::UnexpectedEof, "missing snapshot frame"))??;

    let _: CosmosTxPoolSnapshot = bincode::deserialize(&snapshot_bytes).map_err(|e| {
        io::Error::new(
            ErrorKind::InvalidData,
            format!("invalid cosmos txpool snapshot: {e}"),
        )
    })?;

    for tx in txs {
        let frame = alloy_rlp::encode(CosmosTxPoolIpcTx { tx });
        framed.send(frame.into()).await?;
    }
    framed.flush().await?;
    Ok(())
}

/// Bind `bind_path`, spawn accept loop, return a receiver of raw tx batches.
///
/// Each IPC frame yields one `Vec<Bytes>` of length 1.
pub fn spawn_cosmos_txpool_ipc_server(
    bind_path: std::path::PathBuf,
) -> Result<tokio::sync::mpsc::Receiver<Vec<Bytes>>, io::Error> {
    let (ingress, rx) = tokio::sync::mpsc::channel::<Vec<Bytes>>(1024);
    let listener = UnixListener::bind(&bind_path)?;
    info!(
        path = %bind_path.display(),
        "listening for Cosmos raw tx IPC (same socket role as ETH mempool.sock)"
    );

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let ingress = ingress.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream, ingress).await {
                            warn!(?e, "cosmos txpool ipc connection closed with error");
                        }
                    });
                }
                Err(e) => {
                    warn!(?e, "cosmos txpool ipc accept failed");
                }
            }
        }
    });

    Ok(rx)
}

async fn handle_connection(
    stream: UnixStream,
    ingress: tokio::sync::mpsc::Sender<Vec<Bytes>>,
) -> io::Result<()> {
    let mut framed = Framed::new(stream, build_length_delimited_codec());

    let snapshot = bincode::serialize(&CosmosTxPoolSnapshot::default())
        .map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
    framed.send(snapshot.into()).await?;

    while let Some(frame) = framed.next().await {
        let frame = frame?;
        let Ok(ipc_tx) = alloy_rlp::decode_exact::<CosmosTxPoolIpcTx>(frame.as_ref()) else {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "invalid RLP for CosmosTxPoolIpcTx",
            ));
        };
        if ipc_tx.tx.is_empty() {
            continue;
        }
        ingress
            .send(vec![Bytes::from(ipc_tx.tx)])
            .await
            .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "ingress channel closed"))?;
    }
    Ok(())
}

/// Raw IPC → ABCI `CheckTx` (async) → batches for [`crate::CosmosTxPoolExecutor`] to ingest from its
/// [`futures::Stream`] (`poll_next`), mirroring ETH `EthTxPoolIpcServer` feeding the txpool poll loop.
pub fn spawn_cosmos_txpool_ipc_checked_ingress(
    bind_path: std::path::PathBuf,
    abci_endpoint: String,
) -> Result<tokio::sync::mpsc::Receiver<Vec<Bytes>>, io::Error> {
    let raw_rx = spawn_cosmos_txpool_ipc_server(bind_path)?;
    let (checked_tx, checked_rx) = tokio::sync::mpsc::channel::<Vec<Bytes>>(1024);
    tokio::spawn(cosmos_txpool_ipc_check_bridge(raw_rx, checked_tx, abci_endpoint));
    Ok(checked_rx)
}

async fn cosmos_txpool_ipc_check_bridge(
    mut raw_rx: tokio::sync::mpsc::Receiver<Vec<Bytes>>,
    out: tokio::sync::mpsc::Sender<Vec<Bytes>>,
    endpoint: String,
) {
    use crate::check_tx_via_transport;
    while let Some(batch) = raw_rx.recv().await {
        let mut accepted = Vec::new();
        for tx in batch {
            tracing::info!("check_tx_via_transport: {}", tx.len());
            match check_tx_via_transport(&endpoint, tx.as_ref()).await {
                Ok(resp) if resp.code == 0 => accepted.push(tx),
                Ok(resp) => {
                    tracing::info!(
                        code = resp.code,
                        codespace = %resp.codespace,
                        "cosmos txpool IPC CheckTx rejected"
                    );
                }
                Err(e) => warn!(?e, "cosmos txpool IPC CheckTx transport error"),
            }
        }
        if !accepted.is_empty() && out.send(accepted).await.is_err() {
            break;
        }
    }
}
