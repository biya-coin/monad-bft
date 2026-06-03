//! FIFO mempool keyed by raw-tx hash (SHA-256, CometBFT-style) for dedup and stable handles.

use std::collections::{HashMap, VecDeque};

use bytes::Bytes;
use monad_crypto::hasher::{Hasher, Sha256Hash};

pub type CosmosTxId = [u8; 32];

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
}

impl IndexedCosmosMempool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn pending_len(&self) -> usize {
        self.queue.len()
    }

    pub fn try_push(&mut self, raw: Bytes) -> bool {
        if raw.is_empty() {
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
