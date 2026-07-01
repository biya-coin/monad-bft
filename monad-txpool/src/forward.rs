//! Async tx ingress / forward side path — kept off the consensus event loop.

use std::{
    collections::{HashSet, VecDeque},
    sync::{Arc, Mutex},
    task::Waker,
};

use bytes::Bytes;
use monad_types::NodeId;
use monad_validator::signature_collection::SignatureCollection;
use tokio::sync::mpsc::{self};
use tracing::{debug, warn};

use crate::mempool::{cosmos_raw_tx_id, CosmosTxId, IndexedCosmosMempool};

/// Max wire bytes per `MempoolEvent::ForwardTxs` batch.
pub const COSMOS_FORWARD_EGRESS_MAX_BYTES: usize = 1024 * 1024;

/// One batch of raw txs to publish to upcoming leaders (drained by monad-node → router).
#[derive(Debug, Clone)]
pub struct CosmosTxForwardJob<SCT: SignatureCollection> {
    pub targets: Vec<NodeId<SCT::NodeIdPubKey>>,
    pub txs: Vec<Bytes>,
}

/// Local IPC txs queued for P2P egress; drained by [`CosmosTxPoolExecutor`] on poll.
#[derive(Default)]
pub struct ForwardEgress {
    queue: Mutex<VecDeque<Bytes>>,
    waker: Mutex<Option<Waker>>,
}

impl ForwardEgress {
    pub fn new_shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn is_empty(&self) -> bool {
        self.queue
            .lock()
            .map(|q| q.is_empty())
            .unwrap_or(true)
    }

    pub fn push(&self, tx: Bytes) {
        let Ok(mut queue) = self.queue.lock() else {
            warn!("cosmos txpool: forward egress queue lock poisoned");
            return;
        };
        queue.push_back(tx);
        if let Ok(mut waker) = self.waker.lock() {
            if let Some(w) = waker.take() {
                w.wake();
            }
        }
    }

    pub fn register_waker(&self, waker: Waker) {
        if let Ok(mut slot) = self.waker.lock() {
            *slot = Some(waker);
        }
    }

    /// Drain up to [`COSMOS_FORWARD_EGRESS_MAX_BYTES`] wire bytes for one forward batch.
    pub fn drain_batch(&self) -> (Vec<Bytes>, bool) {
        let Ok(mut queue) = self.queue.lock() else {
            warn!("cosmos txpool: forward egress queue lock poisoned");
            return (Vec::new(), false);
        };
        if queue.is_empty() {
            return (Vec::new(), false);
        }

        let mut batch = Vec::new();
        let mut total = 0usize;
        while let Some(front) = queue.front() {
            let next = total.saturating_add(front.len());
            if next > COSMOS_FORWARD_EGRESS_MAX_BYTES && !batch.is_empty() {
                break;
            }
            let tx = queue.pop_front().expect("non-empty");
            total = next;
            batch.push(tx);
        }
        let has_more = !queue.is_empty();
        (batch, has_more)
    }
}

/// Apply CheckTx-validated txs directly into the shared mempool (background task path).
pub fn apply_checked_batch(
    mempool: &Mutex<IndexedCosmosMempool>,
    committed_seen: &Mutex<HashSet<CosmosTxId>>,
    forward_egress: Option<&ForwardEgress>,
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
    let Ok(seen) = committed_seen.lock() else {
        warn!(source, "cosmos txpool: committed_seen_txs lock poisoned");
        return;
    };

    for tx in txs {
        if seen.contains(&cosmos_raw_tx_id(tx.as_ref())) {
            duplicates += 1;
            continue;
        }
        if pool.is_full() {
            rejected_full += 1;
            continue;
        }
        let fwd = forward_egress.map(|_| tx.clone());
        if pool.try_push(tx) {
            accepted += 1;
            if let (Some(egress), Some(fwd)) = (forward_egress, fwd) {
                egress.push(fwd);
            }
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
    committed_seen: Arc<Mutex<HashSet<CosmosTxId>>>,
    forward_egress: Option<Arc<ForwardEgress>>,
    mut rx: mpsc::Receiver<Vec<Bytes>>,
    source: &'static str,
) {
    tokio::spawn(async move {
        while let Some(batch) = rx.recv().await {
            apply_checked_batch(
                &mempool,
                &committed_seen,
                forward_egress.as_deref(),
                batch,
                source,
            );
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
