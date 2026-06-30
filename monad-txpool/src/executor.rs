use std::{
    collections::{HashSet, VecDeque},
    marker::PhantomData,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
};

use monad_chain_config::{revision::ChainRevision, ChainConfig};
use monad_consensus_types::block::BlockPolicy;
use monad_cosmos_types::{CosmosBlockBody, CosmosExecutionProtocol, ProposedCosmosHeader};
use monad_crypto::certificate_signature::{
    CertificateSignaturePubKey, CertificateSignatureRecoverable,
};
use monad_executor::{Executor, ExecutorMetrics, ExecutorMetricsChain};
use monad_executor_glue::{MempoolEvent, MonadEvent, TxPoolCommand};
use monad_state_backend::StateBackend;
use monad_validator::signature_collection::SignatureCollection;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

monad_executor::metric_consts! {
    GAUGE_COSMOS_TXPOOL_PROPOSAL_TOTAL_US {
        name: "monad.cosmos_txpool.proposal_total_us",
        help: "Total microseconds spent in PrepareProposal per block",
    }
    GAUGE_COSMOS_TXPOOL_PROPOSAL_COUNT {
        name: "monad.cosmos_txpool.proposal_count",
        help: "Number of completed PrepareProposal calls",
    }
}

use crate::{
    abci::{block_on_async, prepare_proposal, prepare_request_from_header},
    forward::{ForwardEgress, spawn_mempool_ingress_consumer, spawn_p2p_insert_bridge},
    mempool::{cosmos_raw_tx_id, CosmosTxId, IndexedCosmosMempool},
};

pub struct CosmosTxPoolExecutor<
    ST: CertificateSignatureRecoverable,
    SCT: SignatureCollection<NodeIdPubKey = CertificateSignaturePubKey<ST>>,
    BPT: BlockPolicy<ST, SCT, CosmosExecutionProtocol, SBT, CCT, CRT>,
    SBT: StateBackend<ST, SCT, CosmosExecutionProtocol>,
    CCT: ChainConfig<CRT>,
    CRT: ChainRevision,
> {
    endpoint: String,
    pending_txs: Arc<Mutex<IndexedCosmosMempool>>,
    p2p_insert_tx: UnboundedSender<Vec<bytes::Bytes>>,
    /// Only block-production events enter the consensus state machine.
    events: VecDeque<MonadEvent<ST, SCT, CosmosExecutionProtocol>>,
    waker: Option<Waker>,
    metrics: ExecutorMetrics,
    committed_seen_txs: Arc<Mutex<HashSet<CosmosTxId>>>,
    forward_egress: Arc<ForwardEgress>,
    _phantom: PhantomData<(BPT, SBT, CCT, CRT)>,
}

impl<ST, SCT, BPT, SBT, CCT, CRT> CosmosTxPoolExecutor<ST, SCT, BPT, SBT, CCT, CRT>
where
    ST: CertificateSignatureRecoverable,
    SCT: SignatureCollection<NodeIdPubKey = CertificateSignaturePubKey<ST>>,
    BPT: BlockPolicy<ST, SCT, CosmosExecutionProtocol, SBT, CCT, CRT>,
    SBT: StateBackend<ST, SCT, CosmosExecutionProtocol>,
    CCT: ChainConfig<CRT>,
    CRT: ChainRevision,
{
    pub fn new(
        endpoint: impl Into<String>,
        ipc_checked_rx: Option<tokio::sync::mpsc::Receiver<Vec<bytes::Bytes>>>,
    ) -> Self {
        let endpoint = endpoint.into();
        let pending_txs = Arc::new(Mutex::new(IndexedCosmosMempool::new()));
        let committed_seen_txs = Arc::new(Mutex::new(HashSet::new()));
        let forward_egress = ForwardEgress::new_shared();
        let (p2p_insert_tx, p2p_checked_rx) = spawn_p2p_insert_bridge(endpoint.clone());

        if let Some(rx) = ipc_checked_rx {
            spawn_mempool_ingress_consumer(
                pending_txs.clone(),
                committed_seen_txs.clone(),
                Some(forward_egress.clone()),
                rx,
                "ipc",
            );
        }
        spawn_mempool_ingress_consumer(
            pending_txs.clone(),
            committed_seen_txs.clone(),
            None,
            p2p_checked_rx,
            "p2p",
        );

        Self {
            endpoint,
            pending_txs,
            p2p_insert_tx,
            events: VecDeque::new(),
            waker: None,
            metrics: ExecutorMetrics::default(),
            committed_seen_txs,
            forward_egress,
            _phantom: PhantomData,
        }
    }

    fn wake(&mut self) {
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }

    fn pending_len(&self) -> usize {
        self.pending_txs
            .lock()
            .map(|pool| pool.pending_len())
            .unwrap_or(0)
    }
}

impl<ST, SCT, BPT, SBT, CCT, CRT> Executor
    for CosmosTxPoolExecutor<ST, SCT, BPT, SBT, CCT, CRT>
where
    ST: CertificateSignatureRecoverable,
    SCT: SignatureCollection<NodeIdPubKey = CertificateSignaturePubKey<ST>>,
    BPT: BlockPolicy<ST, SCT, CosmosExecutionProtocol, SBT, CCT, CRT>,
    SBT: StateBackend<ST, SCT, CosmosExecutionProtocol>,
    CCT: ChainConfig<CRT>,
    CRT: ChainRevision,
{
    type Command = TxPoolCommand<ST, SCT, CosmosExecutionProtocol, BPT, SBT, CCT, CRT>;

    fn exec(&mut self, commands: Vec<Self::Command>) {
        for command in commands {
            match command {
                TxPoolCommand::CreateProposal {
                    epoch,
                    round,
                    seq_num,
                    high_qc,
                    round_signature,
                    last_round_tc,
                    fresh_proposal_certificate,
                    tx_limit: _,
                    proposal_byte_limit,
                    timestamp_ns,
                    extending_blocks,
                    delayed_execution_results,
                    ..
                } => {
                    let proposal_start = std::time::Instant::now();
                    let header = ProposedCosmosHeader {
                        height: seq_num.0,
                        max_tx_bytes: proposal_byte_limit,
                        time_ns: timestamp_ns,
                        local_last_commit: crate::abci::encode_message(
                            &monad_cometbft_proto::cometbft::abci::v1::ExtendedCommitInfo::default(),
                        )
                        .unwrap_or_default(),
                        misbehavior: Default::default(),
                        next_validators_hash: Vec::new(),
                        proposer_address: Vec::new(),
                    };

                    let prepared_txs = {
                        let endpoint = self.endpoint.clone();
                        let reserved_parent_txs = extending_blocks
                            .iter()
                            .flat_map(|block| block.body().execution_body.txs.iter().cloned())
                            .collect::<Vec<_>>();
                        let prepare_request =
                            prepare_request_from_header(&header, reserved_parent_txs);
                        block_on_async(async move {
                            prepare_proposal(&endpoint, prepare_request)
                                .await
                                .map(|resp| resp.txs)
                        })
                    };

                    match prepared_txs {
                        Ok(txs) => {
                            self.metrics
                                .gauge(GAUGE_COSMOS_TXPOOL_PROPOSAL_TOTAL_US)
                                .add(proposal_start.elapsed().as_micros() as u64);
                            self.metrics
                                .gauge(GAUGE_COSMOS_TXPOOL_PROPOSAL_COUNT)
                                .inc();

                            let txs = match self.committed_seen_txs.lock() {
                                Ok(seen) => txs
                                    .into_iter()
                                    .filter(|tx| {
                                        !seen.contains(&cosmos_raw_tx_id(tx.as_slice()))
                                    })
                                    .collect::<Vec<_>>(),
                                Err(poisoned) => {
                                    let seen = poisoned.into_inner();
                                    txs.into_iter()
                                        .filter(|tx| {
                                            !seen.contains(&cosmos_raw_tx_id(tx.as_slice()))
                                        })
                                        .collect::<Vec<_>>()
                                }
                            };
                            let n_included = txs.len();
                            let included_bytes: usize = txs.iter().map(|t| t.len()).sum();
                            if n_included > 0 {
                                info!(
                                    seq_num = seq_num.0,
                                    n_included,
                                    included_bytes,
                                    mempool_pending = self.pending_len(),
                                    "cosmos txpool: proposal txs from PrepareProposal"
                                );
                            }
                            let body = CosmosBlockBody {
                                txs: txs
                                    .into_iter()
                                    .collect::<Vec<_>>()
                                    .try_into()
                                    .unwrap_or_default(),
                            };
                            self.events
                                .push_back(MonadEvent::MempoolEvent(MempoolEvent::Proposal {
                                    epoch,
                                    round,
                                    seq_num,
                                    high_qc,
                                    timestamp_ns,
                                    round_signature,
                                    base_fee: 0,
                                    base_fee_trend: 0,
                                    base_fee_moment: 0,
                                    delayed_execution_results,
                                    proposed_execution_inputs:
                                        monad_consensus_types::block::ProposedExecutionInputs {
                                            header,
                                            body,
                                        },
                                    last_round_tc,
                                    fresh_proposal_certificate,
                                }));
                            self.wake();
                        }
                        Err(err) => {
                            warn!(?err, "PrepareProposal failed");
                        }
                    }
                }
                TxPoolCommand::InsertForwardedTxs { txs, sender: _ } => {
                    if !txs.is_empty() {
                        if let Err(err) = self.p2p_insert_tx.send(txs) {
                            warn!(?err, "cosmos txpool: P2P insert channel closed");
                        }
                    }
                }
                TxPoolCommand::BlockCommit(committed_blocks) => {
                    if let (Ok(mut seen), Ok(mut pool)) = (
                        self.committed_seen_txs.lock(),
                        self.pending_txs.lock(),
                    ) {
                        for block in committed_blocks {
                            for tx in block.body().execution_body.txs.iter() {
                                seen.insert(cosmos_raw_tx_id(tx.as_slice()));
                                pool.remove_by_raw(tx.as_slice());
                            }
                        }
                    } else {
                        warn!("cosmos txpool: lock poisoned during BlockCommit");
                    }
                }
                TxPoolCommand::EnterRound { .. } => {}
                TxPoolCommand::Reset { .. } => {}
            }
        }
    }

    fn metrics(&self) -> ExecutorMetricsChain<'_> {
        self.metrics.as_ref().into()
    }
}

impl<ST, SCT, BPT, SBT, CCT, CRT> futures::Stream
    for CosmosTxPoolExecutor<ST, SCT, BPT, SBT, CCT, CRT>
where
    ST: CertificateSignatureRecoverable,
    SCT: SignatureCollection<NodeIdPubKey = CertificateSignaturePubKey<ST>>,
    BPT: BlockPolicy<ST, SCT, CosmosExecutionProtocol, SBT, CCT, CRT>,
    SBT: StateBackend<ST, SCT, CosmosExecutionProtocol>,
    CCT: ChainConfig<CRT>,
    CRT: ChainRevision,
    Self: Unpin,
{
    type Item = MonadEvent<ST, SCT, CosmosExecutionProtocol>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        if let Some(event) = this.events.pop_front() {
            return Poll::Ready(Some(event));
        }

        let (batch, has_more) = this.forward_egress.drain_batch();
        if !batch.is_empty() {
            if has_more {
                cx.waker().wake_by_ref();
            }
            return Poll::Ready(Some(MonadEvent::MempoolEvent(
                MempoolEvent::ForwardTxs(batch),
            )));
        }

        this.forward_egress.register_waker(cx.waker().clone());
        this.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}
