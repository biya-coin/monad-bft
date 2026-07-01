//! FIFO mempool keyed by raw-tx hash (SHA-256, CometBFT-style) for dedup and stable handles.

use std::collections::{HashMap, VecDeque};

use bytes::Bytes;
use monad_crypto::hasher::{Hasher, Sha256Hash};

pub type CosmosTxId = [u8; 32];

/// Default upper bound on the number of pending txs held in the pool. When the
/// pool reaches this many txs, `try_push` rejects new ones until commits drain
/// it. Override at runtime via the `COSMOS_MEMPOOL_MAX_TXS` env var (0 disables
/// the cap). Chosen to bound memory without throttling healthy throughput.
pub const DEFAULT_MAX_TXS: usize = 100_000;

/// Resolve the configured tx-count cap. `COSMOS_MEMPOOL_MAX_TXS` overrides the
/// default; an explicit `0` disables the cap (unbounded).
fn configured_max_txs() -> usize {
    match std::env::var("COSMOS_MEMPOOL_MAX_TXS") {
        Ok(v) => v.trim().parse::<usize>().unwrap_or(DEFAULT_MAX_TXS),
        Err(_) => DEFAULT_MAX_TXS,
    }
}

#[inline]
pub fn cosmos_raw_tx_id(raw: &[u8]) -> CosmosTxId {
    let mut hasher = Sha256Hash::new();
    hasher.update(raw);
    hasher.hash().0
}

#[derive(Debug, Default)]
pub struct IndexedCosmosMempool {
    queue: VecDeque<CosmosTxId>,
    txs: HashMap<CosmosTxId, Bytes>,
    total_bytes: usize,
    /// Max number of pending txs. `0` means unbounded (preserves `Default`).
    max_txs: usize,
}

impl IndexedCosmosMempool {
    pub fn new() -> Self {
        Self::with_max_txs(configured_max_txs())
    }

    /// Create a pool with an explicit tx-count cap. `max_txs == 0` disables the cap.
    pub fn with_max_txs(max_txs: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            txs: HashMap::new(),
            total_bytes: 0,
            max_txs,
        }
    }

    pub fn pending_len(&self) -> usize {
        self.queue.len()
    }

    /// True when the pool has reached its configured tx-count cap.
    pub fn is_full(&self) -> bool {
        self.max_txs != 0 && self.queue.len() >= self.max_txs
    }

    pub fn try_push(&mut self, raw: Bytes) -> bool {
        if raw.is_empty() {
            return false;
        }
        // Capacity gate: once at the cap, reject new txs outright (no eviction)
        // until committed blocks drain the pool.
        if self.is_full() {
            return false;
        }
        let id = cosmos_raw_tx_id(&raw);
        if self.txs.contains_key(&id) {
            return false;
        }
        self.total_bytes += raw.len();
        self.txs.insert(id, raw);
        self.queue.push_back(id);
        true
    }

    pub fn drain_for_proposal(
        &mut self,
        tx_limit: usize,
        byte_limit: usize,
    ) -> (Vec<Vec<u8>>, Vec<(CosmosTxId, Bytes)>) {
        let mut candidate_txs = Vec::new();
        let mut taken = Vec::new();
        let mut used_bytes = 0usize;

        while candidate_txs.len() < tx_limit {
            let Some(&id) = self.queue.front() else {
                break;
            };
            let Some(tx) = self.txs.get(&id).cloned() else {
                self.queue.pop_front();
                continue;
            };
            let next_bytes = used_bytes.saturating_add(tx.len());
            if next_bytes > byte_limit {
                break;
            }

            self.queue.pop_front();
            self.txs.remove(&id);
            self.total_bytes -= tx.len();
            used_bytes = next_bytes;

            candidate_txs.push(tx.to_vec());
            taken.push((id, tx));
        }

        (candidate_txs, taken)
    }

    pub fn restore_taken_front(&mut self, taken: Vec<(CosmosTxId, Bytes)>) {
        for (id, raw) in taken.into_iter().rev() {
            if self.txs.contains_key(&id) {
                continue;
            }
            self.total_bytes += raw.len();
            self.txs.insert(id, raw);
            self.queue.push_front(id);
        }
    }

    pub fn requeue_front_raw_in_candidate_order(&mut self, raw_txs: &[Vec<u8>]) {
        for raw in raw_txs {
            let b = Bytes::copy_from_slice(raw.as_slice());
            let id = cosmos_raw_tx_id(raw.as_slice());
            if self.txs.contains_key(&id) {
                continue;
            }
            self.total_bytes += b.len();
            self.txs.insert(id, b);
            self.queue.push_front(id);
        }
    }

    pub fn remove_by_raw(&mut self, raw: &[u8]) -> bool {
        if raw.is_empty() {
            return false;
        }
        let id = cosmos_raw_tx_id(raw);
        let Some(removed) = self.txs.remove(&id) else {
            return false;
        };
        self.total_bytes -= removed.len();
        if let Some(pos) = self.queue.iter().position(|q| *q == id) {
            self.queue.remove(pos);
        }
        true
    }
}
