//! FIFO mempool keyed by raw-tx hash (SHA-256, CometBFT-style) for dedup and stable handles.

use std::collections::{HashMap, VecDeque};

use bytes::Bytes;
use monad_crypto::hasher::{Hasher, Sha256Hash};

/// 32-byte id for a raw Cosmos / Comet `tx` payload (SHA-256 of bytes).
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

    /// Insert at back if not duplicate. Returns `false` if same raw bytes already queued.
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

    /// Pop FIFO candidates up to limits; removes them from the pool. On PrepareProposal error,
    /// pass `taken` to [`Self::restore_taken_front`].
    #[allow(dead_code)] // proposal path uses app-only `PrepareProposal`; kept for tests / policy tweaks
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

    /// Restore a failed PrepareProposal: same order as historical `push_front` per tx in reverse
    /// of drain order.
    #[allow(dead_code)]
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

    /// Re-queue raw txs at the front (PrepareProposal success path: candidates not in response).
    /// Preserves the same multi-`push_front` semantics as a `VecDeque<Bytes>` loop.
    #[allow(dead_code)]
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

    /// Remove a tx from the pool if present (e.g. after the block containing it commits).
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_same_bytes() {
        let mut m = IndexedCosmosMempool::new();
        assert!(m.try_push(Bytes::from_static(b"tx1")));
        assert!(!m.try_push(Bytes::from_static(b"tx1")));
        assert_eq!(m.pending_len(), 1);
    }

    #[test]
    fn drain_restore_roundtrip() {
        let mut m = IndexedCosmosMempool::new();
        m.try_push(Bytes::from_static(b"a"));
        m.try_push(Bytes::from_static(b"bb"));
        let (cand, taken) = m.drain_for_proposal(10, 10_000);
        assert_eq!(cand.len(), 2);
        assert!(m.pending_len() == 0);
        m.restore_taken_front(taken);
        assert_eq!(m.pending_len(), 2);
        let (cand2, _) = m.drain_for_proposal(10, 10_000);
        assert_eq!(cand2, cand);
    }

    #[test]
    fn remove_by_raw() {
        let mut m = IndexedCosmosMempool::new();
        let raw = Bytes::from_static(b"committed");
        m.try_push(raw.clone());
        assert_eq!(m.pending_len(), 1);
        assert!(m.remove_by_raw(raw.as_ref()));
        assert_eq!(m.pending_len(), 0);
        assert!(!m.remove_by_raw(raw.as_ref()));
    }
}
