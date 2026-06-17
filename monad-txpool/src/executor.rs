use std::{
    collections::{BTreeMap, VecDeque},
    marker::PhantomData,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
};

use bytes::Bytes;
use futures::StreamExt;
use monad_chain_config::{revision::ChainRevision, ChainConfig};
use monad_consensus_types::{
    block::BlockPolicy,
    no_endorsement::FreshProposalCertificate,
    payload::RoundSignature,
    quorum_certificate::QuorumCertificate,
    timeout::TimeoutCertificate,
};
use monad_cosmos_types::
    {CosmosBlockBody, CosmosExecutionProtocol, ProposedCosmosHeader};
use monad_crypto::certificate_signature::{
    CertificateSignaturePubKey, CertificateSignatureRecoverable,
};
use monad_executor::{Executor, ExecutorMetrics, ExecutorMetricsChain};
use monad_executor_glue::{MempoolEvent, MonadEvent, TxPoolCommand};
use monad_state_backend::StateBackend;
use monad_types::{Epoch, Round, SeqNum, GENESIS_SEQ_NUM};
use monad_validator::signature_collection::SignatureCollection;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, info, warn};

use crate::{
    abci::{
        block_on_async, check_tx, info, prepare_proposal,
        prepare_request_from_header,
    },
    mempool::IndexedCosmosMempool,
};

const COSMOS_FORWARD_EGRESS_MAX_BYTES: usize = 1024 * 1024;

pub struct CosmosTxPoolExecutor<
    ST: CertificateSignatureRecoverable,
    SCT: SignatureCollection<NodeIdPubKey = CertificateSignaturePubKey<ST>>,
    BPT: BlockPolicy<ST, SCT, CosmosExecutionProtocol, SBT, CCT, CRT>,
    SBT: StateBackend<ST, SCT, CosmosExecutionProtocol>,
    CCT: ChainConfig<CRT>,
    CRT: ChainRevision,
> {
    endpoint: String,
    pending_txs: IndexedCosmosMempool,
    ipc_checked: Option<ReceiverStream<Vec<Bytes>>>,
    forward_egress: VecDeque<Bytes>,
    events: VecDeque<MonadEvent<ST, SCT, CosmosExecutionProtocol>>,
    waker: Option<Waker>,
    metrics: ExecutorMetrics,
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
        ipc_checked_rx: Option<tokio::sync::mpsc::Receiver<Vec<Bytes>>>,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            pending_txs: IndexedCosmosMempool::new(),
            ipc_checked: ipc_checked_rx.map(ReceiverStream::new),
            forward_egress: VecDeque::new(),
            events: VecDeque::new(),
            waker: None,
            metrics: ExecutorMetrics::default(),
            _phantom: PhantomData,
        }
    }

    fn apply_checked_ingress_batch(&mut self, txs: Vec<Bytes>) {
        let n = txs.len();
        let wire_bytes: usize = txs.iter().map(|b| b.len()).sum();
        let mut accepted = 0usize;
        let mut duplicates = 0usize;
        let mut rejected_full = 0usize;
        for tx in txs {
            if self.pending_txs.is_full() {
                rejected_full += 1;
                continue;
            }
            let fwd = tx.clone();
            if self.pending_txs.try_push(tx) {
                accepted += 1;
                self.forward_egress.push_back(fwd);
            } else {
                duplicates += 1;
            }
        }
        if rejected_full > 0 {
            warn!(
                rejected_full,
                pending = self.pending_txs.pending_len(),
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
                pending_after = self.pending_txs.pending_len(),
                "cosmos txpool: IPC ingress (CheckTx already applied)"
            );
        }
        if accepted > 0 {
            self.wake();
        }
    }

    fn wake(&mut self) {
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
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
                    delayed_execution_results,
                    ..
                } => {
                    let header = ProposedCosmosHeader {
                        height: seq_num.0,
                        max_tx_bytes: proposal_byte_limit,
                        time_ns: timestamp_ns,
                        local_last_commit: crate::abci::encode_message(&monad_cometbft_proto::cometbft::abci::v1::ExtendedCommitInfo::default())
                            .unwrap_or_default(),
                        misbehavior: Default::default(),
                        next_validators_hash: Vec::new(),
                        proposer_address: Vec::new(),
                    };

                    let prepared_txs = {
                        let endpoint = self.endpoint.clone();
                        let prepare_request = prepare_request_from_header(&header, Vec::new());
                        block_on_async(async move {
                            prepare_proposal(&endpoint, prepare_request)
                                .await
                                .map(|resp| resp.txs)
                        })
                    };

                    match prepared_txs {
                        Ok(txs) => {
                            let n_included = txs.len();
                            let included_bytes: usize = txs.iter().map(|t| t.len()).sum();
                            if n_included > 0 {
                                info!(
                                    seq_num = seq_num.0,
                                    n_included,
                                    included_bytes,
                                    mempool_pending = self.pending_txs.pending_len(),
                                    "cosmos txpool: proposal txs from PrepareProposal"
                                );
                            }
                            let body = CosmosBlockBody {
                                txs: txs.into_iter().collect::<Vec<_>>().try_into().unwrap_or_default(),
                            };
                            self.events.push_back(MonadEvent::MempoolEvent(MempoolEvent::Proposal {
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
                                proposed_execution_inputs: monad_consensus_types::block::ProposedExecutionInputs {
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
                    let n = txs.len();
                    let wire_bytes: usize = txs.iter().map(|b| b.len()).sum();
                    let mut accepted = 0usize;
                    let mut duplicates = 0usize;
                    let mut check_reject = 0usize;
                    let endpoint = self.endpoint.clone();
                    let mut to_apply = Vec::new();
                    for tx in txs {
                        match block_on_async(async { check_tx(&endpoint, tx.as_ref()).await }) {
                            Ok(resp) => {
                                if resp.code == 0 {
                                    to_apply.push(tx);
                                } else {
                                    check_reject += 1;
                                }
                            }
                            Err(err) => {
                                warn!(?err, "cosmos txpool: CheckTx failed");
                                check_reject += 1;
                            }
                        };
                    }
                    for tx in to_apply {
                        if self.pending_txs.try_push(tx) {
                            accepted += 1;
                        } else {
                            duplicates += 1;
                        }
                    }

                    if n > 0 {
                        debug!(
                            count = n,
                            accepted,
                            check_reject,
                            duplicates,
                            wire_bytes,
                            pending_after = self.pending_txs.pending_len(),
                            "cosmos txpool: InsertForwardedTxs (P2P / exec path)"
                        );
                    }
                    if accepted > 0 {
                        self.wake();
                    }
                }
                TxPoolCommand::BlockCommit(committed_blocks) => {
                    for block in committed_blocks {
                        for tx in block.body().execution_body.txs.iter() {
                            self.pending_txs.remove_by_raw(tx.as_slice());
                        }
                    }
                }
                TxPoolCommand::EnterRound { .. } | TxPoolCommand::Reset { .. } => {}
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

        if let Some(stream) = this.ipc_checked.as_mut() {
            match stream.poll_next_unpin(cx) {
                Poll::Ready(Some(batch)) => {
                    this.apply_checked_ingress_batch(batch);
                    cx.waker().wake_by_ref();
                }
                Poll::Ready(None) => {
                    this.ipc_checked = None;
                    cx.waker().wake_by_ref();
                }
                Poll::Pending => {}
            }
        }

        if !this.forward_egress.is_empty() {
            let mut batch = Vec::new();
            let mut total = 0usize;
            while let Some(front) = this.forward_egress.front() {
                let next = total.saturating_add(front.len());
                if next > COSMOS_FORWARD_EGRESS_MAX_BYTES && !batch.is_empty() {
                    break;
                }
                let tx = this.forward_egress.pop_front().expect("non-empty");
                total = next;
                batch.push(tx);
            }
            if !batch.is_empty() {
                if !this.forward_egress.is_empty() {
                    cx.waker().wake_by_ref();
                }
                return Poll::Ready(Some(MonadEvent::MempoolEvent(MempoolEvent::ForwardTxs(batch))));
            }
        }

        this.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}
