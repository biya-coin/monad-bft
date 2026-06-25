//! Async tx ingress / forward side path — kept off the consensus event loop.

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use monad_types::NodeId;
use monad_validator::signature_collection::SignatureCollection;
use tokio::sync::mpsc::{self};
use tracing::{debug, warn};

use crate::mempool::IndexedCosmosMempool;

/// One batch of raw txs to publish to upcoming leaders (drained by monad-node → router).
#[derive(Debug, Clone)]
pub struct CosmosTxForwardJob<SCT: SignatureCollection> {
    pub targets: Vec<NodeId<SCT::NodeIdPubKey>>,
    pub txs: Vec<Bytes>,
}

/// Apply CheckTx-validated txs directly into the shared mempool (background task path).
pub fn apply_checked_batch(
    mempool: &Mutex<IndexedCosmosMempool>,
    txs: Vec<Bytes>,
    source: &'static str,
) {
    let n = txs.len();
    let wire_bytes: usize = txs.iter().map(|b| b.len()).sum();
    let mut accepted = 0usize;
    let mut duplicates = 0usize;
    let mut rejected_full = 0usize;

    let Ok(mut pool) = mempool.lock() else {
        warn!(source, "cosmos txpool: mempool lock poisoned");
        return;
    };

    for tx in txs {
        if pool.is_full() {
            rejected_full += 1;
            continue;
        }
        if pool.try_push(tx) {
            accepted += 1;
        } else {
            duplicates += 1;
        }
    }

    if rejected_full > 0 {
        warn!(
            rejected_full,
            pending = pool.pending_len(),
            source,
            "cosmos txpool: mempool full, rejecting new txs"
        );
    }
    if n > 0 {
        debug!(
            count = n,
            accepted,
            duplicates,
            rejected_full,
            wire_bytes,
            source,
            pending_after = pool.pending_len(),
            "cosmos txpool: applied checked ingress batch"
        );
    }
}

/// Consumes CheckTx-validated batches and inserts into mempool without touching the main loop.
pub fn spawn_mempool_ingress_consumer(
    mempool: Arc<Mutex<IndexedCosmosMempool>>,
    mut rx: mpsc::Receiver<Vec<Bytes>>,
    source: &'static str,
) {
    tokio::spawn(async move {
        while let Some(batch) = rx.recv().await {
            apply_checked_batch(&mempool, batch, source);
        }
    });
}

/// Returns `(p2p_insert_tx, p2p_checked_rx)`.
/// P2P forwarded txs are CheckTx-validated on a background task; accepted batches are
/// applied to the shared mempool by [`spawn_mempool_ingress_consumer`].
pub fn spawn_p2p_insert_bridge(
    endpoint: String,
) -> (
    mpsc::UnboundedSender<Vec<Bytes>>,
    mpsc::Receiver<Vec<Bytes>>,
) {
    let (raw_tx, mut raw_rx) = mpsc::unbounded_channel::<Vec<Bytes>>();
    let (checked_tx, checked_rx) = mpsc::channel::<Vec<Bytes>>(1024);

    tokio::spawn(async move {
        while let Some(batch) = raw_rx.recv().await {
            let mut accepted: Vec<Bytes> = Vec::new();
            for tx in batch {
                match crate::check_tx(&endpoint, &tx).await {
                    Ok(resp) if resp.code == 0 => accepted.push(tx),
                    Ok(resp) => {
                        tracing::debug!(
                            code = resp.code,
                            codespace = %resp.codespace,
                            "cosmos txpool P2P CheckTx rejected"
                        );
                    }
                    Err(e) => warn!(?e, "cosmos txpool P2P CheckTx transport error"),
                }
            }
            if !accepted.is_empty() && checked_tx.send(accepted).await.is_err() {
                break;
            }
        }
    });

    (raw_tx, checked_rx)
}
