//! Unix socket ingress for Cosmos SDK raw transactions into monad-node.

use std::io::{self, ErrorKind};
use std::path::Path;
use std::time::Duration;

use alloy_rlp::{RlpDecodable, RlpEncodable};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use monad_cometbft_proto::cometbft::abci::v1::CheckTxResponse;
use prost::Message;
use serde::{Deserialize, Serialize};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use tracing::{info, warn};

fn build_length_delimited_codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .max_frame_length(64 * 1024 * 1024)
        .new_codec()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CosmosTxPoolSnapshot {}

#[derive(Debug, Clone, RlpEncodable, RlpDecodable)]
pub struct CosmosTxPoolIpcTx {
    pub tx: Vec<u8>,
}

#[derive(Debug, Clone, RlpEncodable, RlpDecodable)]
struct CosmosTxPoolIpcRequest {
    pub tx: Vec<u8>,
    pub sync: bool,
}

struct CosmosTxPoolIngressTx {
    tx: Bytes,
    response: Option<oneshot::Sender<CheckTxResponse>>,
}

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

pub async fn feed_raw_tx_sync(path: impl AsRef<Path>, tx: Vec<u8>) -> io::Result<CheckTxResponse> {
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

    let frame = alloy_rlp::encode(CosmosTxPoolIpcRequest { tx, sync: true });
    framed.send(frame.into()).await?;
    framed.flush().await?;

    let response_bytes = framed.next().await.ok_or_else(|| {
        io::Error::new(ErrorKind::UnexpectedEof, "missing CheckTx response frame")
    })??;

    CheckTxResponse::decode(response_bytes.as_ref()).map_err(|e| {
        io::Error::new(
            ErrorKind::InvalidData,
            format!("invalid CheckTx response frame: {e}"),
        )
    })
}

fn spawn_cosmos_txpool_ipc_server(
    bind_path: std::path::PathBuf,
) -> Result<tokio::sync::mpsc::Receiver<Vec<CosmosTxPoolIngressTx>>, io::Error> {
    let (ingress, rx) = tokio::sync::mpsc::channel::<Vec<CosmosTxPoolIngressTx>>(1024);
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
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    });

    Ok(rx)
}

async fn handle_connection(
    stream: UnixStream,
    ingress: tokio::sync::mpsc::Sender<Vec<CosmosTxPoolIngressTx>>,
) -> io::Result<()> {
    let mut framed = Framed::new(stream, build_length_delimited_codec());

    let snapshot = bincode::serialize(&CosmosTxPoolSnapshot::default())
        .map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
    framed.send(snapshot.into()).await?;

    while let Some(frame) = framed.next().await {
        let frame = frame?;
        let ipc_tx = match alloy_rlp::decode_exact::<CosmosTxPoolIpcRequest>(frame.as_ref()) {
            Ok(ipc_tx) => ipc_tx,
            Err(_) => match alloy_rlp::decode_exact::<CosmosTxPoolIpcTx>(frame.as_ref()) {
                Ok(ipc_tx) => CosmosTxPoolIpcRequest {
                    tx: ipc_tx.tx,
                    sync: false,
                },
                Err(_) => {
                    return Err(io::Error::new(
                        ErrorKind::InvalidData,
                        "invalid RLP for CosmosTxPoolIpcTx",
                    ));
                }
            },
        };
        if ipc_tx.tx.is_empty() {
            continue;
        }

        let (response_tx, response_rx) = if ipc_tx.sync {
            let (tx, rx) = oneshot::channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

        ingress
            .send(vec![CosmosTxPoolIngressTx {
                tx: Bytes::from(ipc_tx.tx),
                response: response_tx,
            }])
            .await
            .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "ingress channel closed"))?;

        if let Some(response_rx) = response_rx {
            let response = response_rx.await.map_err(|_| {
                io::Error::new(ErrorKind::BrokenPipe, "CheckTx response channel closed")
            })?;
            framed.send(response.encode_to_vec().into()).await?;
            framed.flush().await?;
        }
    }
    Ok(())
}

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
    mut raw_rx: tokio::sync::mpsc::Receiver<Vec<CosmosTxPoolIngressTx>>,
    out: tokio::sync::mpsc::Sender<Vec<Bytes>>,
    endpoint: String,
) {
    while let Some(batch) = raw_rx.recv().await {
        let mut accepted = Vec::new();
        let mut accepted_responses = Vec::new();
        for ingress_tx in batch {
            tracing::debug!("check_tx: {}", ingress_tx.tx.len());
            match crate::check_tx(&endpoint, ingress_tx.tx.as_ref()).await {
                Ok(resp) if resp.code == 0 => {
                    accepted.push(ingress_tx.tx);
                    if let Some(response_tx) = ingress_tx.response {
                        accepted_responses.push((response_tx, resp));
                    }
                }
                Ok(resp) => {
                    tracing::debug!(
                        code = resp.code,
                        codespace = %resp.codespace,
                        "cosmos txpool IPC CheckTx rejected"
                    );
                    if let Some(response_tx) = ingress_tx.response {
                        let _ = response_tx.send(resp);
                    }
                }
                Err(e) => {
                    warn!(?e, "cosmos txpool IPC CheckTx transport error");
                    let response = CheckTxResponse {
                        code: 1,
                        log: format!("CheckTx transport error: {e}"),
                        codespace: "monad_txpool".to_string(),
                        ..Default::default()
                    };
                    if let Some(response_tx) = ingress_tx.response {
                        let _ = response_tx.send(response);
                    }
                }
            };
        }
        if !accepted.is_empty() {
            if out.send(accepted).await.is_err() {
                for (response_tx, _) in accepted_responses {
                    let _ = response_tx.send(CheckTxResponse {
                        code: 1,
                        log: "txpool ingress channel closed".to_string(),
                        codespace: "monad_txpool".to_string(),
                        ..Default::default()
                    });
                }
                break;
            }
            for (response_tx, response) in accepted_responses {
                let _ = response_tx.send(response);
            }
        }
    }
}
