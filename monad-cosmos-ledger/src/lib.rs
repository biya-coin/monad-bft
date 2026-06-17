use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fs,
    path::PathBuf,
    pin::Pin,
    task::{Context, Poll},
};

use futures::StreamExt;
use monad_block_persist::{BlockPersist, FileBlockPersist};
use monad_consensus_types::{
    block::{BlockRange, ConsensusFullBlock, OptimisticCommit},
    payload::{ConsensusBlockBody, ConsensusBlockBodyId},
};
use monad_cosmos_types::CosmosExecutionProtocol;
use monad_crypto::certificate_signature::{
    CertificateSignaturePubKey, CertificateSignatureRecoverable,
};
use monad_executor::{Executor, ExecutorMetrics, ExecutorMetricsChain};
use monad_executor_glue::{BlockSyncEvent, LedgerCommand, MonadEvent};
use monad_types::{BlockId, Round, SeqNum};
use monad_validator::signature_collection::SignatureCollection;
use tracing::info;

monad_executor::metric_consts! {
    GAUGE_COSMOS_LEDGER_NUM_COMMITS {
        name: "monad.cosmos_ledger.num_commits",
        help: "Blocks committed to the Cosmos ledger",
    }
    GAUGE_COSMOS_LEDGER_BLOCK_NUM {
        name: "monad.cosmos_ledger.block_num",
        help: "Current block number in the Cosmos ledger",
    }
}

/// Persists BFT blocks to disk and serves blocksync requests for the Cosmos
/// execution path.
///
/// Analogous to the EVM `monad-eth-ledger` crate but backed by
/// [`FileBlockPersist`] rather than an in-memory state.
pub struct CosmosLedger<
    ST: CertificateSignatureRecoverable,
    SCT: SignatureCollection<NodeIdPubKey = CertificateSignaturePubKey<ST>>,
> {
    bft_block_persist: FileBlockPersist<ST, SCT, CosmosExecutionProtocol>,
    metrics: ExecutorMetrics,
    last_commit: Option<(SeqNum, Round)>,
    block_cache_size: usize,
    block_cache: HashMap<BlockId, ConsensusFullBlock<ST, SCT, CosmosExecutionProtocol>>,
    block_payload_cache: HashMap<ConsensusBlockBodyId, ConsensusBlockBody<CosmosExecutionProtocol>>,
    block_cache_index: BTreeMap<Round, (BlockId, ConsensusBlockBodyId)>,
    fetches_tx: tokio::sync::mpsc::UnboundedSender<
        monad_blocksync::messages::message::BlockSyncResponseMessage<
            ST,
            SCT,
            CosmosExecutionProtocol,
        >,
    >,
    fetches: tokio::sync::mpsc::UnboundedReceiver<
        monad_blocksync::messages::message::BlockSyncResponseMessage<
            ST,
            SCT,
            CosmosExecutionProtocol,
        >,
    >,
}

impl<ST, SCT> CosmosLedger<ST, SCT>
where
    ST: CertificateSignatureRecoverable,
    SCT: SignatureCollection<NodeIdPubKey = CertificateSignaturePubKey<ST>>,
{
    pub fn new(ledger_path: PathBuf) -> Self {
        let _ = fs::create_dir_all(&ledger_path);
        let (fetches_tx, fetches) = tokio::sync::mpsc::unbounded_channel();
        Self {
            bft_block_persist: FileBlockPersist::new(ledger_path),
            metrics: ExecutorMetrics::default(),
            last_commit: None,
            block_cache_size: 1_000,
            block_cache: Default::default(),
            block_payload_cache: Default::default(),
            block_cache_index: Default::default(),
            fetches_tx,
            fetches,
        }
    }

    pub fn last_commit(&self) -> Option<SeqNum> {
        self.last_commit.map(|(seq_num, _)| seq_num)
    }

    fn is_cache_hydrated(&self) -> bool {
        self.block_cache_index.len() >= self.block_cache_size
    }

    fn update_cache(&mut self, monad_block: ConsensusFullBlock<ST, SCT, CosmosExecutionProtocol>) {
        let block_id = monad_block.get_id();
        let payload_id = monad_block.get_body_id();
        let block_round = monad_block.get_block_round();

        if let Some((old_block_id, old_payload_id)) =
            self.block_cache_index.insert(block_round, (block_id, payload_id))
        {
            self.block_cache.remove(&old_block_id);
            self.block_payload_cache.remove(&old_payload_id);
        }
        if self.block_cache_index.len() > self.block_cache_size {
            if let Some((_, (old_block_id, old_payload_id))) = self.block_cache_index.pop_first() {
                self.block_cache.remove(&old_block_id);
                self.block_payload_cache.remove(&old_payload_id);
            }
        }
        self.block_payload_cache
            .insert(payload_id, monad_block.body().clone());
        self.block_cache.insert(block_id, monad_block);
    }

    fn write_bft_block(
        &mut self,
        full_block: &ConsensusFullBlock<ST, SCT, CosmosExecutionProtocol>,
    ) {
        self.bft_block_persist.write_bft_body(full_block.body()).unwrap();
        self.bft_block_persist
            .write_bft_header(full_block.header())
            .unwrap();
    }

    fn ledger_fetch_headers(
        &self,
        block_range: BlockRange,
    ) -> monad_blocksync::messages::message::BlockSyncHeadersResponse<
        ST,
        SCT,
        CosmosExecutionProtocol,
    > {
        use monad_blocksync::messages::message::{
            BlockSyncHeadersResponse, BLOCKSYNC_MAX_NUM_HEADERS,
        };

        if block_range.num_blocks.0 > BLOCKSYNC_MAX_NUM_HEADERS as u64 {
            return BlockSyncHeadersResponse::NotAvailable(block_range);
        }

        let mut next_block_id = block_range.last_block_id;
        let mut headers = VecDeque::new();
        while (headers.len() as u64) < block_range.num_blocks.0 {
            let block_header = if let Some(cached_block) = self.block_cache.get(&next_block_id) {
                cached_block.header().clone()
            } else if self.is_cache_hydrated() {
                return BlockSyncHeadersResponse::NotAvailable(block_range);
            } else if let Ok(block) = self.bft_block_persist.read_bft_header(&next_block_id) {
                block
            } else {
                return BlockSyncHeadersResponse::NotAvailable(block_range);
            };
            next_block_id = block_header.get_parent_id();
            headers.push_front(block_header);
        }
        BlockSyncHeadersResponse::Found((block_range, headers.into()))
    }

    fn ledger_fetch_payload(
        &self,
        payload_id: ConsensusBlockBodyId,
    ) -> monad_blocksync::messages::message::BlockSyncBodyResponse<CosmosExecutionProtocol> {
        use monad_blocksync::messages::message::BlockSyncBodyResponse;

        if let Some(cached_payload) = self.block_payload_cache.get(&payload_id) {
            BlockSyncBodyResponse::Found(cached_payload.clone())
        } else if self.is_cache_hydrated() {
            BlockSyncBodyResponse::NotAvailable(payload_id)
        } else if let Ok(payload) = self.bft_block_persist.read_bft_body(&payload_id) {
            BlockSyncBodyResponse::Found(payload)
        } else {
            BlockSyncBodyResponse::NotAvailable(payload_id)
        }
    }
}

impl<ST, SCT> Executor for CosmosLedger<ST, SCT>
where
    ST: CertificateSignatureRecoverable,
    SCT: SignatureCollection<NodeIdPubKey = CertificateSignaturePubKey<ST>>,
{
    type Command = LedgerCommand<ST, SCT, CosmosExecutionProtocol>;

    fn exec(&mut self, commands: Vec<Self::Command>) {
        for command in commands {
            match command {
                LedgerCommand::LedgerCommit(OptimisticCommit::Proposed { block, is_canonical }) => {
                    self.write_bft_block(&block);
                    if is_canonical {
                        self.bft_block_persist
                            .update_proposed_head(&block.get_id())
                            .unwrap();
                    }
                    self.update_cache(block);
                }
                LedgerCommand::LedgerCommit(OptimisticCommit::Voted(block)) => {
                    let block_id = block.get_id();
                    self.update_cache(block);
                    self.bft_block_persist.update_voted_head(&block_id).unwrap();
                }
                LedgerCommand::LedgerCommit(OptimisticCommit::Finalized(block)) => {
                    let block_id = block.get_id();
                    let block_num = block.get_seq_num().0;
                    let tx_count = block.body().execution_body.txs.len();
                    info!(block_num, tx_count, "committed cosmos block");
                    self.metrics[GAUGE_COSMOS_LEDGER_NUM_COMMITS] += 1;
                    self.metrics[GAUGE_COSMOS_LEDGER_BLOCK_NUM] = block_num;
                    self.last_commit = Some((block.get_seq_num(), block.get_block_round()));
                    self.bft_block_persist
                        .update_finalized_head(&block_id)
                        .unwrap();
                }
                LedgerCommand::LedgerFetchHeaders(block_range) => {
                    let _ = self.fetches_tx.send(
                        monad_blocksync::messages::message::BlockSyncResponseMessage::HeadersResponse(
                            self.ledger_fetch_headers(block_range),
                        ),
                    );
                }
                LedgerCommand::LedgerFetchPayload(payload_id) => {
                    let _ = self.fetches_tx.send(
                        monad_blocksync::messages::message::BlockSyncResponseMessage::PayloadResponse(
                            self.ledger_fetch_payload(payload_id),
                        ),
                    );
                }
            }
        }
    }

    fn metrics(&self) -> ExecutorMetricsChain<'_> {
        self.metrics.as_ref().into()
    }
}

impl<ST, SCT> futures::Stream for CosmosLedger<ST, SCT>
where
    ST: CertificateSignatureRecoverable,
    SCT: SignatureCollection<NodeIdPubKey = CertificateSignaturePubKey<ST>>,
{
    type Item = MonadEvent<ST, SCT, CosmosExecutionProtocol>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.fetches.poll_recv(cx).map(|response| {
            let response = response.expect("fetches_tx never dropped");
            Some(MonadEvent::BlockSyncEvent(BlockSyncEvent::SelfResponse {
                response,
            }))
        })
    }
}
