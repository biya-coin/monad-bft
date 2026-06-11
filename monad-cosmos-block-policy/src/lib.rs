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
use monad_types::{BlockId, Epoch, SeqNum, Stake, GENESIS_BLOCK_ID, GENESIS_SEQ_NUM};
use monad_validator::signature_collection::{SignatureCollection, SignatureCollectionPubKeyType};
use tracing::{info, warn};

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
            if !header.finalize_block_response.is_empty() {
                return Ok(header.clone());
            }
            // InitChain seeds genesis with app_hash only (no FinalizeBlock at height 0).
            if *seq_num == GENESIS_SEQ_NUM && !header.app_hash.is_empty() {
                return Ok(header.clone());
            }
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

fn execution_result_cached(store: &CosmosCommitStore, height: u64) -> bool {
    store.get(&SeqNum(height)).is_some_and(|h| {
        !h.finalize_block_response.is_empty()
            || (height == GENESIS_SEQ_NUM.0 && !h.app_hash.is_empty())
    })
}

fn persist_execution_result(
    store: &mut CosmosCommitStore,
    header: CosmosFinalizedHeader,
) -> Result<(), CosmosBlockPolicyError> {
    store
        .commit(header)
        .map_err(|err| CosmosBlockPolicyError::Transport(err.to_string()))
}

/// Persist an execution result fetched at runtime (primary cache path).
pub fn cache_execution_result_from_abci(
    store: &mut CosmosCommitStore,
    header: CosmosFinalizedHeader,
) -> Result<(), CosmosBlockPolicyError> {
    persist_execution_result(store, header)
}

/// Startup compensation only: refresh recent placeholder rows from ABCI Query.
///
/// Scans `[app_height - execution_delay, app_height]` — the window needed for the
/// next delayed-execution proposals. Does not walk the full chain; non-full nodes
/// may lack ancient results on the app (those queries fail harmlessly).
pub fn compensate_recent_execution_results_from_abci(
    store: &mut CosmosCommitStore,
    endpoint: &str,
    app_height: u64,
    execution_delay: u64,
) -> Result<(), CosmosBlockPolicyError> {
    if app_height == 0 {
        return Ok(());
    }
    let from = app_height.saturating_sub(execution_delay).max(1);
    for height in from..=app_height {
        if execution_result_cached(store, height) {
            continue;
        }
        match block_on_async(async { query_execution_result(endpoint, height).await }) {
            Ok(header) => {
                info!(
                    height,
                    from,
                    app_height,
                    "compensated recent cosmos execution result from ABCI query"
                );
                persist_execution_result(store, header)?;
            }
            Err(err) => {
                tracing::debug!(
                    height,
                    ?err,
                    "execution result not available for startup compensation"
                );
            }
        }
    }
    Ok(())
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
    commit_store: std::sync::Arc<std::sync::Mutex<CosmosCommitStore>>,
}

impl CosmosBlockPolicy {
    pub fn new(
        execution_delay: u64,
        abci_endpoint: impl Into<String>,
        commit_store: std::sync::Arc<std::sync::Mutex<CosmosCommitStore>>,
    ) -> Self {
        Self {
            execution_delay: SeqNum(execution_delay),
            abci_endpoint: abci_endpoint.into(),
            commit_store,
        }
    }

    fn lookup_delayed_execution_result<ST, SCT, SBT>(
        &self,
        target: SeqNum,
        state_backend: &SBT,
    ) -> Result<CosmosFinalizedHeader, StateBackendError>
    where
        ST: CertificateSignatureRecoverable,
        SCT: SignatureCollection<NodeIdPubKey = CertificateSignaturePubKey<ST>>,
        SBT: StateBackend<ST, SCT, CosmosExecutionProtocol>,
    {
        if let Ok(header) = state_backend.get_execution_result(&GENESIS_BLOCK_ID, &target, true) {
            return Ok(header);
        }

        let endpoint = self.abci_endpoint.clone();
        let header = match block_on_async(async move { query_execution_result(&endpoint, target.0).await })
        {
            Ok(h) => h,
            Err(err) => {
                tracing::warn!(
                    target_height = target.0,
                    ?err,
                    "delayed execution result not available (state backend + ABCI query)"
                );
                return Err(StateBackendError::NotAvailableYet);
            }
        };

        if let Ok(mut store) = self.commit_store.lock() {
            match persist_execution_result(&mut store, header.clone()) {
                Ok(()) => tracing::debug!(
                    target_height = target.0,
                    "cached delayed execution result to commit store"
                ),
                Err(err) => tracing::warn!(
                    target_height = target.0,
                    ?err,
                    "failed to cache delayed execution result to commit store"
                ),
            }
        }

        Ok(header)
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
        state_backend: &SBT,
        _chain_config: &CCT,
    ) -> Result<(), BlockPolicyError> {
        let (extending_seq_num, extending_timestamp) =
            if let Some(extended_block) = extending_blocks.last() {
                (extended_block.get_seq_num(), extended_block.get_timestamp())
            } else {
                (blocktree_root.seq_num, 0)
            };

        if block.get_seq_num() != extending_seq_num + SeqNum(1) {
            warn!(
                seq_num = block.get_seq_num().0,
                extending_seq_num = extending_seq_num.0,
                "coherency fail: seq_num not consecutive"
            );
            return Err(BlockPolicyError::BlockNotCoherent);
        }
        if block.get_timestamp() <= extending_timestamp {
            warn!(
                seq_num = block.get_seq_num().0,
                block_ts = block.get_timestamp(),
                extending_ts = extending_timestamp,
                "coherency fail: timestamp not increasing"
            );
            return Err(BlockPolicyError::TimestampError);
        }

        // 延时执行结果校验（同时起到限速作用）
        let expected_execution_results: Vec<CosmosFinalizedHeader> =
            if block.get_seq_num() <= self.execution_delay + GENESIS_SEQ_NUM {
                Vec::new()
            } else {
                let target = block.get_seq_num() - self.execution_delay;
                vec![self
                    .lookup_delayed_execution_result(target, state_backend)
                    .map_err(BlockPolicyError::StateBackendError)?]
            };
        if block.get_execution_results() != &expected_execution_results {
            warn!(
                seq_num = block.get_seq_num().0,
                "coherency fail: execution result mismatch"
            );
            return Err(BlockPolicyError::ExecutionResultMismatch);
        }

        // 交易合法性验证（ProcessProposal）
        let req = process_request_from_inputs(
            &block.header().execution_inputs,
            &block.body().execution_body,
        );
        let endpoint = self.abci_endpoint.clone();
        let resp = match block_on_async(async move { process_proposal(&endpoint, req).await }) {
            Ok(resp) => resp,
            Err(err) => {
                warn!(
                    seq_num = block.get_seq_num().0,
                    ?err,
                    "coherency fail: process_proposal ABCI call errored"
                );
                return Err(BlockPolicyError::BlockNotCoherent);
            }
        };
        if resp.status != 1 {
            warn!(
                seq_num = block.get_seq_num().0,
                status = resp.status,
                "coherency fail: process_proposal returned non-accept status"
            );
            return Err(BlockPolicyError::BlockNotCoherent);
        }

        Ok(())
    }

    fn get_expected_execution_results(
        &self,
        block_seq_num: SeqNum,
        _extending_blocks: Vec<&Self::ValidatedBlock>,
        state_backend: &SBT,
    ) -> Result<Vec<CosmosFinalizedHeader>, StateBackendError> {
        if block_seq_num <= self.execution_delay + GENESIS_SEQ_NUM {
            return Ok(Vec::new());
        }
        let target = block_seq_num - self.execution_delay;
        Ok(vec![self.lookup_delayed_execution_result(target, state_backend)?])
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
