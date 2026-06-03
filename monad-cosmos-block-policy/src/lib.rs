use alloy_primitives::Address;
use monad_chain_config::{revision::ChainRevision, ChainConfig};
use monad_consensus_types::{
    block::{
        BlockPolicy, BlockPolicyError, ConsensusFullBlock, PassthruWrappedBlock,
    },
    block_validator::BlockValidator,
    checkpoint::RootInfo,
    metrics::Metrics,
    payload::ConsensusBlockBody,
};
use monad_cosmos_types::{CosmosExecutionProtocol, CosmosFinalizedHeader};
use monad_crypto::certificate_signature::{CertificateSignaturePubKey, CertificateSignatureRecoverable};
use monad_state_backend::{StateBackend, StateBackendError};
use monad_txpool::{
    block_on_async, process_proposal, process_request_from_inputs, query_execution_result,
    CosmosTxPoolError as CosmosBlockPolicyError, CosmosCommitStore,
};
use monad_types::{BlockId, Epoch, SeqNum, Stake, GENESIS_SEQ_NUM};
use monad_validator::signature_collection::{SignatureCollection, SignatureCollectionPubKeyType};

// ---------------------------------------------------------------------------
// CosmosStateBackend
// ---------------------------------------------------------------------------

/// A [`StateBackend`] backed by [`CosmosCommitStore`].
///
/// [`get_execution_result`] queries the commit store by `seq_num` – the
/// `block_id` parameter is intentionally ignored because the Cosmos commit
/// history is linear (no forks).
#[derive(Clone, Debug)]
pub struct CosmosStateBackend {
    store: std::sync::Arc<std::sync::Mutex<CosmosCommitStore>>,
}

impl CosmosStateBackend {
    pub fn new(store: std::sync::Arc<std::sync::Mutex<CosmosCommitStore>>) -> Self {
        Self { store }
    }

    pub fn store(&self) -> std::sync::Arc<std::sync::Mutex<CosmosCommitStore>> {
        self.store.clone()
    }
}

impl<ST, SCT> StateBackend<ST, SCT, CosmosExecutionProtocol> for CosmosStateBackend
where
    ST: CertificateSignatureRecoverable,
    SCT: SignatureCollection<NodeIdPubKey = CertificateSignaturePubKey<ST>>,
{
    fn get_account_statuses<'a>(
        &self,
        _block_id: &BlockId,
        _seq_num: &SeqNum,
        _is_finalized: bool,
        addresses: impl Iterator<Item = &'a Address>,
    ) -> Result<Vec<Option<monad_eth_types::EthAccount>>, StateBackendError> {
        Ok(addresses.map(|_| None).collect())
    }

    fn get_execution_result(
        &self,
        _block_id: &BlockId,
        seq_num: &SeqNum,
        is_finalized: bool,
    ) -> Result<CosmosFinalizedHeader, StateBackendError> {
        let store = self.store.lock().unwrap();
        if let Some(header) = store.get(seq_num) {
            return Ok(header.clone());
        }
        if is_finalized {
            if store.earliest().is_some_and(|earliest| earliest > *seq_num) {
                return Err(StateBackendError::NeverAvailable);
            }
        }
        Err(StateBackendError::NotAvailableYet)
    }

    fn raw_read_earliest_finalized_block(&self) -> Option<SeqNum> {
        self.store.lock().unwrap().earliest()
    }

    fn raw_read_latest_finalized_block(&self) -> Option<SeqNum> {
        self.store.lock().unwrap().latest()
    }

    fn read_valset_at_block(
        &self,
        _block_num: SeqNum,
        _requested_epoch: Epoch,
    ) -> Vec<(SCT::NodeIdPubKey, SignatureCollectionPubKeyType<SCT>, Stake)> {
        Vec::new()
    }

    fn total_db_lookups(&self) -> u64 {
        0
    }
}

// ---------------------------------------------------------------------------
// CosmosBlockPolicy
// ---------------------------------------------------------------------------

/// [`BlockPolicy`] for the Cosmos execution path.
///
/// With `execution_delay = k` each consensus block at seq_num N must carry the
/// [`CosmosFinalizedHeader`] from block N-k as its `delayed_execution_results`.
/// The leader obtains this via [`get_expected_execution_results`] which queries
/// biyachain-core over ABCI; validators verify consistency in [`check_coherency`].
#[derive(Clone, Debug)]
pub struct CosmosBlockPolicy {
    execution_delay: SeqNum,
    abci_endpoint: String,
}

impl CosmosBlockPolicy {
    pub fn new(execution_delay: u64, abci_endpoint: impl Into<String>) -> Self {
        Self {
            execution_delay: SeqNum(execution_delay),
            abci_endpoint: abci_endpoint.into(),
        }
    }
}

impl<ST, SCT, SBT, CCT, CRT>
    BlockPolicy<ST, SCT, CosmosExecutionProtocol, SBT, CCT, CRT> for CosmosBlockPolicy
where
    ST: CertificateSignatureRecoverable,
    SCT: SignatureCollection<NodeIdPubKey = CertificateSignaturePubKey<ST>>,
    SBT: StateBackend<ST, SCT, CosmosExecutionProtocol>,
    CCT: ChainConfig<CRT>,
    CRT: ChainRevision,
{
    type ValidatedBlock = PassthruWrappedBlock<ST, SCT, CosmosExecutionProtocol>;

    fn check_coherency(
        &self,
        block: &Self::ValidatedBlock,
        extending_blocks: Vec<&Self::ValidatedBlock>,
        blocktree_root: RootInfo,
        _state_backend: &SBT,
        _chain_config: &CCT,
    ) -> Result<(), BlockPolicyError> {
        let (extending_seq_num, extending_timestamp) =
            if let Some(extended_block) = extending_blocks.last() {
                (extended_block.get_seq_num(), extended_block.get_timestamp())
            } else {
                (blocktree_root.seq_num, 0)
            };

        if block.get_seq_num() != extending_seq_num + SeqNum(1) {
            return Err(BlockPolicyError::BlockNotCoherent);
        }
        if block.get_timestamp() <= extending_timestamp {
            return Err(BlockPolicyError::TimestampError);
        }

        // 延时执行结果校验（同时起到限速作用）
        let expected_execution_results: Vec<CosmosFinalizedHeader> =
            if block.get_seq_num() <= self.execution_delay + GENESIS_SEQ_NUM {
                Vec::new()
            } else {
                let target = block.get_seq_num() - self.execution_delay;
                let endpoint = self.abci_endpoint.clone();
                match block_on_async(async move { query_execution_result(&endpoint, target.0).await }) {
                    Ok(h) => vec![h],
                    Err(_) => return Err(BlockPolicyError::StateBackendError(StateBackendError::NotAvailableYet)),
                }
            };
        if block.get_execution_results() != &expected_execution_results {
            return Err(BlockPolicyError::ExecutionResultMismatch);
        }

        // 交易合法性验证（ProcessProposal）
        let req = process_request_from_inputs(
            &block.header().execution_inputs,
            &block.body().execution_body,
        );
        let endpoint = self.abci_endpoint.clone();
        let resp = block_on_async(async move { process_proposal(&endpoint, req).await })
            .map_err(|_| BlockPolicyError::BlockNotCoherent)?;
        if resp.status != 1 {
            return Err(BlockPolicyError::BlockNotCoherent);
        }

        Ok(())
    }

    fn get_expected_execution_results(
        &self,
        block_seq_num: SeqNum,
        _extending_blocks: Vec<&Self::ValidatedBlock>,
        _state_backend: &SBT,
    ) -> Result<Vec<CosmosFinalizedHeader>, StateBackendError> {
        if block_seq_num <= self.execution_delay + GENESIS_SEQ_NUM {
            return Ok(Vec::new());
        }
        let target = block_seq_num - self.execution_delay;
        let endpoint = self.abci_endpoint.clone();
        match block_on_async(async move { query_execution_result(&endpoint, target.0).await }) {
            Ok(h) => Ok(vec![h]),
            Err(_) => Err(StateBackendError::NotAvailableYet),
        }
    }

    fn update_committed_block(&mut self, _block: &Self::ValidatedBlock) {}
    fn reset(&mut self, _last_delay_committed_blocks: Vec<&Self::ValidatedBlock>) {}
}

// ---------------------------------------------------------------------------
// CosmosBlockValidator
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct CosmosBlockValidator;

impl CosmosBlockValidator {
    pub fn new(_endpoint: impl Into<String>) -> Self {
        Self
    }
}

#[derive(Debug)]
pub enum CosmosBlockValidationError {
    Abci(CosmosBlockPolicyError),
    HeaderPayloadMismatch,
}

impl From<CosmosBlockPolicyError> for CosmosBlockValidationError {
    fn from(value: CosmosBlockPolicyError) -> Self {
        Self::Abci(value)
    }
}

impl From<monad_consensus_types::block::ConsensusFullBlockError> for CosmosBlockValidationError {
    fn from(_value: monad_consensus_types::block::ConsensusFullBlockError) -> Self {
        Self::HeaderPayloadMismatch
    }
}

impl<ST, SCT, SBT, CCT, CRT>
    BlockValidator<ST, SCT, CosmosExecutionProtocol, CosmosBlockPolicy, SBT, CCT, CRT>
    for CosmosBlockValidator
where
    ST: CertificateSignatureRecoverable,
    SCT: SignatureCollection<NodeIdPubKey = CertificateSignaturePubKey<ST>>,
    SBT: StateBackend<ST, SCT, CosmosExecutionProtocol>,
    CCT: ChainConfig<CRT>,
    CRT: ChainRevision,
{
    type BlockValidationError = CosmosBlockValidationError;

    fn validate(
        &self,
        header: monad_consensus_types::block::ConsensusBlockHeader<
            ST,
            SCT,
            CosmosExecutionProtocol,
        >,
        body: ConsensusBlockBody<CosmosExecutionProtocol>,
        _author_pubkey: Option<&SignatureCollectionPubKeyType<SCT>>,
        _chain_config: &CCT,
        _metrics: &mut Metrics,
    ) -> Result<
        PassthruWrappedBlock<ST, SCT, CosmosExecutionProtocol>,
        Self::BlockValidationError,
    > {
        Ok(PassthruWrappedBlock(ConsensusFullBlock::new(header, body)?))
    }
}
