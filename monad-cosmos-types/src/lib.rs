use alloy_rlp::{RlpDecodable, RlpEncodable};
use monad_cometbft_proto::cometbft::abci::v1::{CommitResponse, FinalizeBlockResponse};
use monad_types::{ExecutionProtocol, FinalizedHeader, LimitedVec, SeqNum};
use prost::Message;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use sha2::{Digest, Sha256};

pub const MAX_COSMOS_TXS_PER_BLOCK: usize = 10_000;
pub const MAX_COSMOS_MISBEHAVIOR: usize = 128;

#[serde_as]
#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, RlpEncodable, RlpDecodable,
)]
pub struct ProposedCosmosHeader {
    #[serde_as(as = "DisplayFromStr")]
    pub height: u64,
    #[serde_as(as = "DisplayFromStr")]
    pub max_tx_bytes: u64,
    #[serde_as(as = "DisplayFromStr")]
    pub time_ns: u128,
    #[serde_as(as = "serde_with::hex::Hex")]
    pub local_last_commit: Vec<u8>,
    pub misbehavior: LimitedVec<Vec<u8>, MAX_COSMOS_MISBEHAVIOR>,
    #[serde_as(as = "serde_with::hex::Hex")]
    pub next_validators_hash: Vec<u8>,
    #[serde_as(as = "serde_with::hex::Hex")]
    pub proposer_address: Vec<u8>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, RlpEncodable, RlpDecodable,
)]
pub struct CosmosBlockBody {
    pub txs: LimitedVec<Vec<u8>, MAX_COSMOS_TXS_PER_BLOCK>,
}

#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, RlpEncodable, RlpDecodable)]
pub struct CosmosFinalizedHeader {
    #[serde_as(as = "DisplayFromStr")]
    pub height: u64,
    #[serde_as(as = "serde_with::hex::Hex")]
    pub app_hash: Vec<u8>,
    #[serde_as(as = "serde_with::hex::Hex")]
    pub tx_results_hash: Vec<u8>,
    #[serde_as(as = "serde_with::hex::Hex")]
    pub validator_updates_hash: Vec<u8>,
    #[serde_as(as = "serde_with::hex::Hex")]
    pub finalize_block_response: Vec<u8>,
    #[serde_as(as = "serde_with::hex::Hex")]
    pub commit_response: Vec<u8>,
    #[serde_as(as = "DisplayFromStr")]
    pub retain_height: u64,
}

impl CosmosFinalizedHeader {
    pub fn from_abci_responses(
        height: u64,
        finalize_block: &FinalizeBlockResponse,
        commit: &CommitResponse,
    ) -> Result<Self, prost::EncodeError> {
        let mut finalize_block_response = Vec::new();
        finalize_block.encode(&mut finalize_block_response)?;

        let mut commit_response = Vec::new();
        commit.encode(&mut commit_response)?;

        let tx_results_hash = hash_messages(&finalize_block.tx_results)?;
        let validator_updates_hash = hash_messages(&finalize_block.validator_updates)?;

        Ok(Self {
            height,
            app_hash: finalize_block.app_hash.clone(),
            tx_results_hash,
            validator_updates_hash,
            finalize_block_response,
            commit_response,
            retain_height: commit.retain_height.max(0) as u64,
        })
    }

    /// Lightweight copy for embedding in consensus proposals
    /// (`delayed_execution_results`).
    ///
    /// The raw `finalize_block_response` / `commit_response` blobs can each be
    /// ~MBs for a full block and are NOT needed cross-node: `app_hash`,
    /// `tx_results_hash` and `validator_updates_hash` are cryptographic
    /// commitments to that content, and every validator re-derives the same
    /// digest from its own local execution. Shipping the full blobs only bloats
    /// the proposal past raptorcast's `MAX_MESSAGE_SIZE` (3 MiB). Dropping them
    /// keeps coherency verification intact while shrinking the proposal by an
    /// order of magnitude.
    pub fn consensus_digest(&self) -> Self {
        Self {
            height: self.height,
            app_hash: self.app_hash.clone(),
            tx_results_hash: self.tx_results_hash.clone(),
            validator_updates_hash: self.validator_updates_hash.clone(),
            finalize_block_response: Vec::new(),
            commit_response: Vec::new(),
            retain_height: self.retain_height,
        }
    }
}

fn hash_messages<M: Message>(messages: &[M]) -> Result<Vec<u8>, prost::EncodeError> {
    let mut hasher = Sha256::new();
    for message in messages {
        let mut encoded = Vec::new();
        message.encode(&mut encoded)?;
        hasher.update(encoded);
    }
    Ok(hasher.finalize().to_vec())
}

impl FinalizedHeader for CosmosFinalizedHeader {
    fn seq_num(&self) -> SeqNum {
        SeqNum(self.height)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable, RlpDecodable, Serialize, Deserialize)]
pub struct CosmosExecutionProtocol;

impl ExecutionProtocol for CosmosExecutionProtocol {
    type ProposedHeader = ProposedCosmosHeader;
    type Body = CosmosBlockBody;
    type FinalizedHeader = CosmosFinalizedHeader;
}
