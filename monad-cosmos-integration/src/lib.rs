use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fs,
    io::{self, Read, Write},
    marker::PhantomData,
    net::TcpStream,
    os::unix::net::UnixStream,
    path::Path,
    path::PathBuf,
    pin::Pin,
    sync::Mutex,
    task::{Context, Poll, Waker},
};

use alloy_primitives::Address;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use monad_block_persist::{BlockPersist, FileBlockPersist};
use monad_chain_config::{revision::ChainRevision, ChainConfig};
use monad_cometbft_proto::cometbft::abci::v1::{
    abci_service_client::AbciServiceClient, request, response, CheckTxRequest, CheckTxType,
    CommitInfo, CommitRequest, ExtendedCommitInfo, FinalizeBlockRequest, InfoRequest,
    InitChainRequest, Misbehavior, PrepareProposalRequest, ProcessProposalRequest, Request,
    Response,
};
use monad_cometbft_proto::cometbft::types::v1::{
    BlockParams, ConsensusParams, EvidenceParams, FeatureParams, SynchronyParams, ValidatorParams,
    VersionParams,
};
use monad_consensus_types::{
    block::{
        BlockPolicy, BlockPolicyError, BlockRange, ConsensusFullBlock, OptimisticCommit,
        PassthruWrappedBlock,
    },
    block_validator::BlockValidator,
    checkpoint::RootInfo,
    metrics::Metrics,
    payload::{ConsensusBlockBody, ConsensusBlockBodyId},
};
use monad_cosmos_types::{
    CosmosBlockBody, CosmosExecutionProtocol, CosmosFinalizedHeader, ProposedCosmosHeader,
};
use monad_crypto::hasher::Hasher;
use monad_crypto::certificate_signature::{
    CertificateSignaturePubKey, CertificateSignatureRecoverable,
};
use monad_executor::{Executor, ExecutorMetrics, ExecutorMetricsChain};
use monad_executor_glue::{BlockSyncEvent, LedgerCommand, MempoolEvent, MonadEvent, TxPoolCommand};
use monad_performance_monitor as performance_monitor;
use monad_state_backend::{StateBackend, StateBackendError};
use monad_types::{BlockId, Epoch, FinalizedHeader, Round, SeqNum, Stake, GENESIS_SEQ_NUM};
use monad_validator::signature_collection::{SignatureCollection, SignatureCollectionPubKeyType};
use once_cell::sync::Lazy;
use prost::Message;
use prost_types::Timestamp;
use serde_json::Value;
use futures::StreamExt;
use thiserror::Error;
use tokio::runtime::{Handle, Runtime};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tracing::{info, warn};

pub mod cosmos_txpool_ipc;

mod indexed_cosmos_mempool;
use indexed_cosmos_mempool::IndexedCosmosMempool;

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

const COSMOS_MAX_AHEAD_BLOCKS: u64 = 4;

const COSMOS_FORWARD_EGRESS_MAX_BYTES: usize = 1024 * 1024;

#[derive(Debug, Error)]
pub enum CosmosIntegrationError {
    #[error("invalid ABCI endpoint: {0}")]
    InvalidEndpoint(String),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("grpc status: {0}")]
    GrpcStatus(String),
    #[error("encode error: {0}")]
    Encode(#[from] prost::EncodeError),
    #[error("decode error: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("proposal rejected by ABCI application")]
    ProposalRejected,
    #[error("failed to build consensus full block")]
    FullBlock,
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

fn block_on_async<F, T>(future: F) -> Result<T, CosmosIntegrationError>
where
    F: std::future::Future<Output = Result<T, CosmosIntegrationError>>,
{
    if let Ok(handle) = Handle::try_current() {
        tokio::task::block_in_place(|| handle.block_on(future))
    } else {
        Runtime::new()
            .map_err(CosmosIntegrationError::Io)?
            .block_on(future)
    }
}

async fn connect_client(
    endpoint: &str,
) -> Result<AbciServiceClient<Channel>, CosmosIntegrationError> {
    let endpoint = tonic::transport::Endpoint::from_shared(endpoint.to_owned())
        .map_err(|err| CosmosIntegrationError::InvalidEndpoint(err.to_string()))?;
    let channel = endpoint
        .connect()
        .await
        .map_err(|err| CosmosIntegrationError::Transport(err.to_string()))?;
    Ok(AbciServiceClient::new(channel))
}

fn parse_i64_str(value: Option<&Value>) -> i64 {
    value
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or_default()
}

fn parse_u64_str(value: Option<&Value>) -> u64 {
    value
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or_default()
}

fn parse_duration_nanos(value: Option<&Value>) -> Option<prost_types::Duration> {
    let nanos = value
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<i64>().ok())?;
    Some(prost_types::Duration {
        seconds: nanos / 1_000_000_000,
        nanos: (nanos % 1_000_000_000) as i32,
    })
}

fn parse_timestamp_rfc3339(value: &str) -> Result<Timestamp, CosmosIntegrationError> {
    let parsed = DateTime::parse_from_rfc3339(value)
        .map_err(|err| CosmosIntegrationError::Transport(format!("invalid genesis_time: {err}")))?;
    let utc = parsed.with_timezone(&Utc);
    Ok(Timestamp {
        seconds: utc.timestamp(),
        nanos: utc.timestamp_subsec_nanos() as i32,
    })
}

fn parse_consensus_params(genesis: &Value) -> ConsensusParams {
    let params = genesis
        .get("consensus")
        .and_then(|v| v.get("params"))
        .cloned()
        .unwrap_or(Value::Null);

    let block = params.get("block");
    let evidence = params.get("evidence");
    let validator = params.get("validator");
    let version = params.get("version");
    let synchrony = params.get("synchrony");
    let feature = params.get("feature");

    ConsensusParams {
        block: Some(BlockParams {
            max_bytes: parse_i64_str(block.and_then(|v| v.get("max_bytes"))),
            max_gas: parse_i64_str(block.and_then(|v| v.get("max_gas"))),
        }),
        evidence: Some(EvidenceParams {
            max_age_num_blocks: parse_i64_str(evidence.and_then(|v| v.get("max_age_num_blocks"))),
            max_age_duration: parse_duration_nanos(
                evidence.and_then(|v| v.get("max_age_duration")),
            ),
            max_bytes: parse_i64_str(evidence.and_then(|v| v.get("max_bytes"))),
        }),
        validator: Some(ValidatorParams {
            pub_key_types: validator
                .and_then(|v| v.get("pub_key_types"))
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(ToOwned::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
        }),
        version: Some(VersionParams {
            app: parse_u64_str(version.and_then(|v| v.get("app"))),
        }),
        abci: None,
        synchrony: Some(SynchronyParams {
            precision: parse_duration_nanos(synchrony.and_then(|v| v.get("precision"))),
            message_delay: parse_duration_nanos(synchrony.and_then(|v| v.get("message_delay"))),
        }),
        feature: Some(FeatureParams {
            vote_extensions_enable_height: Some(parse_i64_str(
                feature.and_then(|v| v.get("vote_extensions_enable_height")),
            )),
            pbts_enable_height: Some(parse_i64_str(
                feature.and_then(|v| v.get("pbts_enable_height")),
            )),
        }),
    }
}

fn build_init_chain_request(genesis: &Value) -> Result<InitChainRequest, CosmosIntegrationError> {
    let genesis_time = genesis
        .get("genesis_time")
        .and_then(Value::as_str)
        .ok_or_else(|| CosmosIntegrationError::Transport("missing genesis_time".to_owned()))?;
    let chain_id = genesis
        .get("chain_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let initial_height = genesis
        .get("initial_height")
        .and_then(Value::as_i64)
        .or_else(|| {
            genesis
                .get("initial_height")
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<i64>().ok())
        })
        .unwrap_or(1);
    let mut app_state = genesis.get("app_state").cloned().unwrap_or(Value::Null);
    let mut injected_consensus_module = false;
    if let Some(app_state_obj) = app_state.as_object_mut() {
        let consensus_params_value = genesis
            .get("consensus")
            .and_then(|v| v.get("params"))
            .cloned()
            .unwrap_or(Value::Null);

        let should_inject = app_state_obj
            .get("consensus")
            .is_none_or(|value| value.is_null());
        if should_inject {
            app_state_obj.insert(
                "consensus".to_owned(),
                serde_json::json!({
                    "params": consensus_params_value
                }),
            );
            injected_consensus_module = true;
        }

        let genesis_tx_bond_denom = app_state_obj
            .get("genutil")
            .and_then(|v| v.get("gen_txs"))
            .and_then(Value::as_array)
            .and_then(|txs| txs.first())
            .and_then(|tx| tx.get("body"))
            .and_then(|body| body.get("messages"))
            .and_then(Value::as_array)
            .and_then(|msgs| msgs.first())
            .and_then(|msg| msg.get("value"))
            .and_then(|value| value.get("denom"))
            .or_else(|| {
                app_state_obj
                    .get("genutil")
                    .and_then(|v| v.get("gen_txs"))
                    .and_then(Value::as_array)
                    .and_then(|txs| txs.first())
                    .and_then(|tx| tx.get("body"))
                    .and_then(|body| body.get("messages"))
                    .and_then(Value::as_array)
                    .and_then(|msgs| msgs.first())
                    .and_then(|msg| msg.get("value"))
                    .and_then(|value| value.get("denom"))
            })
            .and_then(Value::as_str)
            .map(str::to_owned);

        if let (Some(staking_obj), Some(genesis_denom)) = (
            app_state_obj.get_mut("staking").and_then(Value::as_object_mut),
            genesis_tx_bond_denom,
        ) {
            if let Some(params) = staking_obj.get_mut("params").and_then(Value::as_object_mut) {
                params.insert("bond_denom".to_owned(), Value::String(genesis_denom));
            }
        }
    }

    let app_state_bytes = serde_json::to_vec(&app_state)
        .map_err(|err| CosmosIntegrationError::Transport(format!("invalid app_state: {err}")))?;
    info!(
        chain_id,
        initial_height,
        app_state_bytes_len = app_state_bytes.len(),
        injected_consensus_module,
        "built InitChainRequest from cosmos genesis"
    );

    Ok(InitChainRequest {
        time: Some(parse_timestamp_rfc3339(genesis_time)?),
        chain_id,
        consensus_params: Some(parse_consensus_params(genesis)),
        validators: Vec::new(),
        app_state_bytes,
        initial_height,
    })
}

enum AbciTransport<'a> {
    Grpc(&'a str),
    Unix(&'a str),
    Tcp(&'a str),
}

fn parse_abci_transport(endpoint: &str) -> AbciTransport<'_> {
    if endpoint.starts_with("unix://") {
        AbciTransport::Unix(endpoint.trim_start_matches("unix://"))
    } else if endpoint.starts_with("tcp://") {
        AbciTransport::Tcp(endpoint.trim_start_matches("tcp://"))
    } else {
        AbciTransport::Grpc(endpoint)
    }
}

fn encode_varint(mut value: usize) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
    out
}

fn decode_varint(reader: &mut impl Read) -> Result<usize, CosmosIntegrationError> {
    let mut shift = 0usize;
    let mut result = 0usize;
    loop {
        let mut byte = [0u8; 1];
        reader.read_exact(&mut byte)?;
        let current = byte[0];
        result |= usize::from(current & 0x7f) << shift;
        if current & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift >= usize::BITS as usize {
            return Err(CosmosIntegrationError::Transport(
                "invalid varint length prefix".to_owned(),
            ));
        }
    }
}

fn write_socket_message(
    writer: &mut impl Write,
    request: &Request,
) -> Result<(), CosmosIntegrationError> {
    let mut body = Vec::new();
    request.encode(&mut body)?;
    let prefix = encode_varint(body.len());
    writer.write_all(&prefix)?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

fn read_socket_message(reader: &mut impl Read) -> Result<Response, CosmosIntegrationError> {
    let len = decode_varint(reader)?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    Response::decode(body.as_slice()).map_err(CosmosIntegrationError::Decode)
}

fn socket_roundtrip(
    endpoint: &str,
    request: Request,
) -> Result<Response, CosmosIntegrationError> {
    enum PersistentSocket {
        Unix(UnixStream),
        Tcp(TcpStream),
    }

    impl PersistentSocket {
        fn connect(endpoint: &str) -> Result<Self, CosmosIntegrationError> {
            match parse_abci_transport(endpoint) {
                AbciTransport::Unix(path) => Ok(Self::Unix(UnixStream::connect(path)?)),
                AbciTransport::Tcp(addr) => Ok(Self::Tcp(TcpStream::connect(addr)?)),
                AbciTransport::Grpc(_) => Err(CosmosIntegrationError::InvalidEndpoint(
                    "grpc endpoint passed to socket client".to_owned(),
                )),
            }
        }
    }

    impl Read for PersistentSocket {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self {
                PersistentSocket::Unix(stream) => stream.read(buf),
                PersistentSocket::Tcp(stream) => stream.read(buf),
            }
        }
    }

    impl Write for PersistentSocket {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            match self {
                PersistentSocket::Unix(stream) => stream.write(buf),
                PersistentSocket::Tcp(stream) => stream.write(buf),
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            match self {
                PersistentSocket::Unix(stream) => stream.flush(),
                PersistentSocket::Tcp(stream) => stream.flush(),
            }
        }
    }

    static SOCKET_CLIENTS: Lazy<Mutex<HashMap<String, PersistentSocket>>> =
        Lazy::new(|| Mutex::new(HashMap::new()));

    fn do_roundtrip(
        mut stream: &mut PersistentSocket,
        request: Request,
    ) -> Result<Response, CosmosIntegrationError> {
        write_socket_message(&mut stream, &request)?;
        write_socket_message(
            &mut stream,
            &Request {
                value: Some(request::Value::Flush(monad_cometbft_proto::cometbft::abci::v1::FlushRequest {})),
            },
        )?;
        let first = read_socket_message(&mut stream)?;
        let second = read_socket_message(&mut stream)?;
        match second.value {
            Some(response::Value::Flush(_)) => Ok(first),
            _ => Err(CosmosIntegrationError::Transport(
                "socket ABCI protocol expected trailing flush response".to_owned(),
            )),
        }
    }

    match parse_abci_transport(endpoint) {
        AbciTransport::Grpc(_) => Err(CosmosIntegrationError::InvalidEndpoint(
            "grpc endpoint passed to socket client".to_owned(),
        )),
        AbciTransport::Unix(_) | AbciTransport::Tcp(_) => {
            let mut clients = SOCKET_CLIENTS.lock().unwrap();
            if !clients.contains_key(endpoint) {
                clients.insert(endpoint.to_owned(), PersistentSocket::connect(endpoint)?);
            }
            let stream = clients
                .get_mut(endpoint)
                .expect("socket client must exist after insertion");
            match do_roundtrip(stream, request) {
                Ok(resp) => Ok(resp),
                Err(err) => {
                    clients.remove(endpoint);
                    Err(err)
                }
            }
        }
    }
}

pub async fn check_tx_via_transport(
    endpoint: &str,
    tx_bytes: &[u8],
) -> Result<monad_cometbft_proto::cometbft::abci::v1::CheckTxResponse, CosmosIntegrationError> {
    let request_msg = CheckTxRequest {
        tx: tx_bytes.to_vec(),
        r#type: CheckTxType::Check as i32,
    };
    match parse_abci_transport(endpoint) {
        AbciTransport::Grpc(endpoint) => {
            let mut client = connect_client(endpoint).await?;
            client
                .check_tx(request_msg)
                .await
                .map(|resp| resp.into_inner())
                .map_err(|err| CosmosIntegrationError::GrpcStatus(err.to_string()))
        }
        AbciTransport::Unix(_) | AbciTransport::Tcp(_) => {
            let endpoint_owned = endpoint.to_string();
            tokio::task::spawn_blocking(move || {
                let response = socket_roundtrip(
                    &endpoint_owned,
                    Request {
                        value: Some(request::Value::CheckTx(request_msg)),
                    },
                )?;
                match response.value {
                    Some(response::Value::CheckTx(resp)) => Ok(resp),
                    Some(response::Value::Exception(resp)) => {
                        Err(CosmosIntegrationError::Transport(resp.error))
                    }
                    other => Err(CosmosIntegrationError::Transport(format!(
                        "unexpected response for CheckTx: {:?}",
                        other.map(|_| "other")
                    ))),
                }
            })
            .await
            .map_err(|e| {
                CosmosIntegrationError::Transport(format!("check_tx spawn_blocking: {e}"))
            })?
        }
    }
}

async fn prepare_proposal_via_transport(
    endpoint: &str,
    request_msg: PrepareProposalRequest,
) -> Result<monad_cometbft_proto::cometbft::abci::v1::PrepareProposalResponse, CosmosIntegrationError>
{
    match parse_abci_transport(endpoint) {
        AbciTransport::Grpc(endpoint) => {
            let mut client = connect_client(endpoint).await?;
            client
                .prepare_proposal(request_msg)
                .await
                .map(|resp| resp.into_inner())
                .map_err(|err| CosmosIntegrationError::GrpcStatus(err.to_string()))
        }
        AbciTransport::Unix(_) | AbciTransport::Tcp(_) => {
            let response = socket_roundtrip(
                endpoint,
                Request {
                    value: Some(request::Value::PrepareProposal(request_msg)),
                },
            )?;
            match response.value {
                Some(response::Value::PrepareProposal(resp)) => Ok(resp),
                Some(response::Value::Exception(resp)) => Err(CosmosIntegrationError::Transport(resp.error)),
                other => Err(CosmosIntegrationError::Transport(format!(
                    "unexpected response for PrepareProposal: {:?}",
                    other.map(|_| "other")
                ))),
            }
        }
    }
}

async fn info_via_transport(
    endpoint: &str,
) -> Result<monad_cometbft_proto::cometbft::abci::v1::InfoResponse, CosmosIntegrationError> {
    match parse_abci_transport(endpoint) {
        AbciTransport::Grpc(endpoint) => {
            let mut client = connect_client(endpoint).await?;
            client
                .info(InfoRequest {
                    version: String::new(),
                    block_version: 0,
                    p2p_version: 0,
                    abci_version: String::new(),
                })
                .await
                .map(|resp| resp.into_inner())
                .map_err(|err| CosmosIntegrationError::GrpcStatus(err.to_string()))
        }
        AbciTransport::Unix(_) | AbciTransport::Tcp(_) => {
            let response = socket_roundtrip(
                endpoint,
                Request {
                    value: Some(request::Value::Info(InfoRequest {
                        version: String::new(),
                        block_version: 0,
                        p2p_version: 0,
                        abci_version: String::new(),
                    })),
                },
            )?;
            match response.value {
                Some(response::Value::Info(resp)) => Ok(resp),
                Some(response::Value::Exception(resp)) => {
                    Err(CosmosIntegrationError::Transport(resp.error))
                }
                _ => Err(CosmosIntegrationError::Transport(
                    "unexpected response for Info".to_owned(),
                )),
            }
        }
    }
}

async fn init_chain_via_transport(
    endpoint: &str,
    request_msg: InitChainRequest,
) -> Result<monad_cometbft_proto::cometbft::abci::v1::InitChainResponse, CosmosIntegrationError> {
    match parse_abci_transport(endpoint) {
        AbciTransport::Grpc(endpoint) => {
            let mut client = connect_client(endpoint).await?;
            client
                .init_chain(request_msg)
                .await
                .map(|resp| resp.into_inner())
                .map_err(|err| CosmosIntegrationError::GrpcStatus(err.to_string()))
        }
        AbciTransport::Unix(_) | AbciTransport::Tcp(_) => {
            let response = socket_roundtrip(
                endpoint,
                Request {
                    value: Some(request::Value::InitChain(request_msg)),
                },
            )?;
            match response.value {
                Some(response::Value::InitChain(resp)) => Ok(resp),
                Some(response::Value::Exception(resp)) => {
                    Err(CosmosIntegrationError::Transport(resp.error))
                }
                _ => Err(CosmosIntegrationError::Transport(
                    "unexpected response for InitChain".to_owned(),
                )),
            }
        }
    }
}

async fn process_proposal_via_transport(
    endpoint: &str,
    request_msg: ProcessProposalRequest,
) -> Result<monad_cometbft_proto::cometbft::abci::v1::ProcessProposalResponse, CosmosIntegrationError>
{
    match parse_abci_transport(endpoint) {
        AbciTransport::Grpc(endpoint) => {
            let mut client = connect_client(endpoint).await?;
            client
                .process_proposal(request_msg)
                .await
                .map(|resp| resp.into_inner())
                .map_err(|err| CosmosIntegrationError::GrpcStatus(err.to_string()))
        }
        AbciTransport::Unix(_) | AbciTransport::Tcp(_) => {
            let response = socket_roundtrip(
                endpoint,
                Request {
                    value: Some(request::Value::ProcessProposal(request_msg)),
                },
            )?;
            match response.value {
                Some(response::Value::ProcessProposal(resp)) => Ok(resp),
                Some(response::Value::Exception(resp)) => Err(CosmosIntegrationError::Transport(resp.error)),
                other => Err(CosmosIntegrationError::Transport(format!(
                    "unexpected response for ProcessProposal: {:?}",
                    other.map(|_| "other")
                ))),
            }
        }
    }
}

async fn finalize_and_commit_via_transport(
    endpoint: &str,
    request_msg: FinalizeBlockRequest,
) -> Result<
    (
        monad_cometbft_proto::cometbft::abci::v1::FinalizeBlockResponse,
        monad_cometbft_proto::cometbft::abci::v1::CommitResponse,
    ),
    CosmosIntegrationError,
> {
    match parse_abci_transport(endpoint) {
        AbciTransport::Grpc(endpoint) => {
            let mut client = connect_client(endpoint).await?;
            let finalize_block = client
                .finalize_block(request_msg)
                .await
                .map(|resp| resp.into_inner())
                .map_err(|err| CosmosIntegrationError::GrpcStatus(err.to_string()))?;
            let commit = client
                .commit(CommitRequest {})
                .await
                .map(|resp| resp.into_inner())
                .map_err(|err| CosmosIntegrationError::GrpcStatus(err.to_string()))?;
            Ok((finalize_block, commit))
        }
        AbciTransport::Unix(_) | AbciTransport::Tcp(_) => {
            let finalize_response = socket_roundtrip(
                endpoint,
                Request {
                    value: Some(request::Value::FinalizeBlock(request_msg)),
                },
            )?;
            let finalize_block = match finalize_response.value {
                Some(response::Value::FinalizeBlock(resp)) => resp,
                Some(response::Value::Exception(resp)) => {
                    return Err(CosmosIntegrationError::Transport(resp.error));
                }
                _ => {
                    return Err(CosmosIntegrationError::Transport(
                        "unexpected response for FinalizeBlock".to_owned(),
                    ));
                }
            };

            let commit_response = socket_roundtrip(
                endpoint,
                Request {
                    value: Some(request::Value::Commit(CommitRequest {})),
                },
            )?;
            let commit = match commit_response.value {
                Some(response::Value::Commit(resp)) => resp,
                Some(response::Value::Exception(resp)) => {
                    return Err(CosmosIntegrationError::Transport(resp.error));
                }
                _ => {
                    return Err(CosmosIntegrationError::Transport(
                        "unexpected response for Commit".to_owned(),
                    ));
                }
            };
            Ok((finalize_block, commit))
        }
    }
}

fn timestamp_from_ns(timestamp_ns: u128) -> Timestamp {
    Timestamp {
        seconds: (timestamp_ns / 1_000_000_000) as i64,
        nanos: (timestamp_ns % 1_000_000_000) as i32,
    }
}

fn encode_message<M: Message>(message: &M) -> Result<Vec<u8>, prost::EncodeError> {
    let mut out = Vec::new();
    message.encode(&mut out)?;
    Ok(out)
}

fn hash_txs(txs: &[Vec<u8>]) -> Vec<u8> {
    let mut hasher = monad_crypto::hasher::HasherType::new();
    for tx in txs {
        hasher.update(tx);
    }
    hasher.hash().0.to_vec()
}

fn prepare_request_from_header(
    header: &ProposedCosmosHeader,
    txs: Vec<Vec<u8>>,
) -> PrepareProposalRequest {
    PrepareProposalRequest {
        max_tx_bytes: header.max_tx_bytes as i64,
        txs,
        local_last_commit: Some(ExtendedCommitInfo::default()),
        misbehavior: Vec::<Misbehavior>::new(),
        height: header.height as i64,
        time: Some(timestamp_from_ns(header.time_ns)),
        next_validators_hash: header.next_validators_hash.clone(),
        proposer_address: header.proposer_address.clone(),
    }
}

fn process_request_from_inputs(
    header: &ProposedCosmosHeader,
    body: &CosmosBlockBody,
) -> ProcessProposalRequest {
    ProcessProposalRequest {
        txs: body.txs.iter().cloned().collect(),
        proposed_last_commit: Some(CommitInfo::default()),
        misbehavior: Vec::<Misbehavior>::new(),
        hash: hash_txs(&body.txs.iter().cloned().collect::<Vec<_>>()),
        height: header.height as i64,
        time: Some(timestamp_from_ns(header.time_ns)),
        next_validators_hash: header.next_validators_hash.clone(),
        proposer_address: header.proposer_address.clone(),
    }
}

fn finalize_request_from_inputs(
    header: &ProposedCosmosHeader,
    body: &CosmosBlockBody,
) -> FinalizeBlockRequest {
    FinalizeBlockRequest {
        txs: body.txs.iter().cloned().collect(),
        decided_last_commit: Some(CommitInfo::default()),
        misbehavior: Vec::<Misbehavior>::new(),
        hash: hash_txs(&body.txs.iter().cloned().collect::<Vec<_>>()),
        height: header.height as i64,
        time: Some(timestamp_from_ns(header.time_ns)),
        next_validators_hash: header.next_validators_hash.clone(),
        proposer_address: header.proposer_address.clone(),
        syncing_to_height: header.height as i64,
    }
}

#[derive(Debug)]
pub struct CosmosCommitStore {
    dir: PathBuf,
    commits: BTreeMap<SeqNum, CosmosFinalizedHeader>,
}

impl CosmosCommitStore {
    pub fn new(dir: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&dir)?;
        let mut commits = BTreeMap::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("rlp") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let Ok(height) = stem.parse::<u64>() else {
                continue;
            };
            let bytes = fs::read(&path)?;
            let header: CosmosFinalizedHeader =
                alloy_rlp::decode_exact(&bytes).map_err(io::Error::other)?;
            commits.insert(SeqNum(height), header);
        }
        Ok(Self { dir, commits })
    }

    pub fn commit(&mut self, header: CosmosFinalizedHeader) -> io::Result<()> {
        let seq_num = header.seq_num();
        let path = self.dir.join(format!("{}.rlp", seq_num.0));
        fs::write(path, alloy_rlp::encode(&header))?;
        self.commits.insert(seq_num, header);
        Ok(())
    }

    pub fn ensure_genesis_from_cosmos_genesis(
        &mut self,
        endpoint: &str,
        genesis_path: impl AsRef<Path>,
    ) -> Result<(), CosmosIntegrationError> {
        if !self.commits.is_empty() {
            return Ok(());
        }

        let genesis_path = genesis_path.as_ref();
        let app_hash = if genesis_path.exists() {
            let json: Value = serde_json::from_slice(&fs::read(genesis_path)?)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
            let info = block_on_async(async { info_via_transport(endpoint).await })?;
            info!(
                endpoint,
                last_block_height = info.last_block_height,
                last_block_app_hash_len = info.last_block_app_hash.len(),
                app_version = info.app_version,
                "received ABCI Info response before genesis initialization"
            );
            if info.last_block_height <= 0 || info.last_block_app_hash.is_empty() {
                let init_request = build_init_chain_request(&json)?;
                let init_response =
                    block_on_async(async { init_chain_via_transport(endpoint, init_request).await })?;
                info!(
                    endpoint,
                    init_app_hash_len = init_response.app_hash.len(),
                    init_validator_updates = init_response.validators.len(),
                    has_consensus_params = init_response.consensus_params.is_some(),
                    "received ABCI InitChain response"
                );
                init_response.app_hash
            } else {
                info.last_block_app_hash
            }
        } else {
            Vec::new()
        };

        self.commit(CosmosFinalizedHeader {
            height: GENESIS_SEQ_NUM.0,
            app_hash,
            tx_results_hash: Vec::new(),
            validator_updates_hash: Vec::new(),
            finalize_block_response: Vec::new(),
            commit_response: Vec::new(),
            retain_height: GENESIS_SEQ_NUM.0,
        })?;
        Ok(())
    }

    pub fn get(&self, seq_num: &SeqNum) -> Option<&CosmosFinalizedHeader> {
        self.commits.get(seq_num)
    }

    pub fn earliest(&self) -> Option<SeqNum> {
        self.commits.first_key_value().map(|(seq, _)| *seq)
    }

    pub fn latest(&self) -> Option<SeqNum> {
        self.commits.last_key_value().map(|(seq, _)| *seq)
    }

    /// When the ABCI app was advanced by another client (e.g. `debug_abci_first_block`) or a prior
    /// run, local `*.rlp` files can still only reflect genesis (`0.rlp`). In that case
    /// [`Self::latest`] stays at `SeqNum(0)` while CometBFT `Info.last_block_height` is already
    /// ≥ 1, so [`crate::CosmosTxPoolExecutor::drain_app_commits`] would call `FinalizeBlock` for a
    /// height the app has already committed (`invalid height: N; expected: N+1`).
    ///
    /// This method pulls [`info_via_transport`] and, if the app is exactly **one** block ahead of
    /// our persisted latest height, appends one synthetic [`CosmosFinalizedHeader`] using
    /// `last_block_app_hash`. Larger gaps return an error so operators reset state instead of
    /// silently diverging.
    pub fn sync_with_abci_app(&mut self, endpoint: &str) -> Result<(), CosmosIntegrationError> {
        loop {
            let info = block_on_async(async { info_via_transport(endpoint).await })?;
            let remote = info.last_block_height.max(0) as u64;
            let local = self.latest().map(|s| s.0).unwrap_or(0);
            if remote <= local {
                return Ok(());
            }
            if remote > local + 1 {
                return Err(CosmosIntegrationError::Transport(format!(
                    "ABCI last_block_height={remote} is ahead of local cosmos-commits latest={local} by more than one; \
                     reset the app home or remove cosmos-commits and use a fresh socket (common cause: running debug_abci_first_block against the same biyachaind before monad-node)"
                )));
            }
            info!(
                local,
                remote,
                app_hash_len = info.last_block_app_hash.len(),
                "catching up CosmosCommitStore from ABCI Info (app one block ahead of disk)"
            );
            self.commit(CosmosFinalizedHeader {
                height: remote,
                app_hash: info.last_block_app_hash.clone(),
                tx_results_hash: Vec::new(),
                validator_updates_hash: Vec::new(),
                finalize_block_response: Vec::new(),
                commit_response: Vec::new(),
                retain_height: 0,
            })?;
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct AbciGenesisDebugInfo {
    pub info_last_block_height: i64,
    pub info_last_block_app_hash_len: usize,
    pub init_chain_sent: bool,
    pub init_chain_app_hash_len: usize,
    pub init_chain_validator_updates: usize,
    pub init_chain_has_consensus_params: bool,
}

#[derive(Debug, serde::Serialize)]
pub struct AbciFirstBlockDebugInfo {
    pub init_chain_sent: bool,
    pub finalize_ok: bool,
    pub commit_ok: bool,
    pub finalize_app_hash_len: usize,
    pub commit_retain_height: i64,
}

pub fn debug_abci_genesis_handshake(
    endpoint: &str,
    genesis_path: impl AsRef<Path>,
) -> Result<AbciGenesisDebugInfo, CosmosIntegrationError> {
    let genesis_path = genesis_path.as_ref();
    let info = block_on_async(async { info_via_transport(endpoint).await })?;

    let mut result = AbciGenesisDebugInfo {
        info_last_block_height: info.last_block_height,
        info_last_block_app_hash_len: info.last_block_app_hash.len(),
        init_chain_sent: false,
        init_chain_app_hash_len: 0,
        init_chain_validator_updates: 0,
        init_chain_has_consensus_params: false,
    };

    if info.last_block_height <= 0 || info.last_block_app_hash.is_empty() {
        let json: Value = serde_json::from_slice(&fs::read(genesis_path)?)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        let init_request = build_init_chain_request(&json)?;
        let init_response =
            block_on_async(async { init_chain_via_transport(endpoint, init_request).await })?;
        result.init_chain_sent = true;
        result.init_chain_app_hash_len = init_response.app_hash.len();
        result.init_chain_validator_updates = init_response.validators.len();
        result.init_chain_has_consensus_params = init_response.consensus_params.is_some();
    }

    Ok(result)
}

pub fn debug_abci_first_block(
    endpoint: &str,
    genesis_path: impl AsRef<Path>,
) -> Result<AbciFirstBlockDebugInfo, CosmosIntegrationError> {
    let genesis_path = genesis_path.as_ref();
    let json: Value = serde_json::from_slice(&fs::read(genesis_path)?)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

    let init_request = build_init_chain_request(&json)?;
    let _ = block_on_async(async { init_chain_via_transport(endpoint, init_request).await })?;

    let genesis_time = json
        .get("genesis_time")
        .and_then(Value::as_str)
        .ok_or_else(|| CosmosIntegrationError::Transport("missing genesis_time".to_owned()))?;
    let time = parse_timestamp_rfc3339(genesis_time)?;

    let (finalize_block, commit) = block_on_async(async {
        finalize_and_commit_via_transport(
            endpoint,
            FinalizeBlockRequest {
                txs: Vec::new(),
                decided_last_commit: Some(CommitInfo::default()),
                misbehavior: Vec::new(),
                hash: Vec::new(),
                height: 1,
                time: Some(time),
                next_validators_hash: Vec::new(),
                proposer_address: Vec::new(),
                syncing_to_height: 1,
            },
        )
        .await
    })?;

    Ok(AbciFirstBlockDebugInfo {
        init_chain_sent: true,
        finalize_ok: true,
        commit_ok: true,
        finalize_app_hash_len: finalize_block.app_hash.len(),
        commit_retain_height: commit.retain_height,
    })
}

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

#[derive(Clone, Copy, Debug, Default)]
pub struct CosmosBlockPolicy;

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

        let latest_app_height = state_backend
            .raw_read_latest_finalized_block()
            .unwrap_or(GENESIS_SEQ_NUM);
        let max_allowed_seq_num = latest_app_height + SeqNum(COSMOS_MAX_AHEAD_BLOCKS);
        if block.get_seq_num() > max_allowed_seq_num {
            return Err(BlockPolicyError::BlockNotCoherent);
        }

        if block.get_seq_num() != extending_seq_num + SeqNum(1) {
            return Err(BlockPolicyError::BlockNotCoherent);
        }
        if block.get_timestamp() <= extending_timestamp {
            return Err(BlockPolicyError::TimestampError);
        }
        if !block.get_execution_results().is_empty() {
            return Err(BlockPolicyError::ExecutionResultMismatch);
        }
        Ok(())
    }

    fn get_expected_execution_results(
        &self,
        _block_seq_num: SeqNum,
        _extending_blocks: Vec<&Self::ValidatedBlock>,
        _state_backend: &SBT,
    ) -> Result<Vec<CosmosFinalizedHeader>, StateBackendError> {
        Ok(Vec::new())
    }

    fn update_committed_block(&mut self, _block: &Self::ValidatedBlock) {}
    fn reset(&mut self, _last_delay_committed_blocks: Vec<&Self::ValidatedBlock>) {}
}

#[derive(Debug, Default, Clone)]
pub struct CosmosBlockValidator {
    endpoint: String,
}

impl CosmosBlockValidator {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
        }
    }
}

#[derive(Debug)]
pub enum CosmosBlockValidationError {
    Abci(CosmosIntegrationError),
    HeaderPayloadMismatch,
}

impl From<CosmosIntegrationError> for CosmosBlockValidationError {
    fn from(value: CosmosIntegrationError) -> Self {
        Self::Abci(value)
    }
}

impl From<monad_consensus_types::block::ConsensusFullBlockError> for CosmosBlockValidationError {
    fn from(_value: monad_consensus_types::block::ConsensusFullBlockError) -> Self {
        Self::HeaderPayloadMismatch
    }
}

impl<ST, SCT, SBT, CCT, CRT>
    BlockValidator<
        ST,
        SCT,
        CosmosExecutionProtocol,
        CosmosBlockPolicy,
        SBT,
        CCT,
        CRT,
    > for CosmosBlockValidator
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
        header: monad_consensus_types::block::ConsensusBlockHeader<ST, SCT, CosmosExecutionProtocol>,
        body: ConsensusBlockBody<CosmosExecutionProtocol>,
        _author_pubkey: Option<&SignatureCollectionPubKeyType<SCT>>,
        _chain_config: &CCT,
        _metrics: &mut Metrics,
    ) -> Result<PassthruWrappedBlock<ST, SCT, CosmosExecutionProtocol>, Self::BlockValidationError>
    {
        if header.seq_num <= SeqNum(COSMOS_MAX_AHEAD_BLOCKS) {
            return Ok(PassthruWrappedBlock(ConsensusFullBlock::new(header, body)?));
        }

        let request = process_request_from_inputs(&header.execution_inputs, &body.execution_body);
        let endpoint = self.endpoint.clone();
        let response = block_on_async(async move {
            process_proposal_via_transport(&endpoint, request).await
        })?;

        if response.status != 1 {
            return Err(CosmosIntegrationError::ProposalRejected.into());
        }

        Ok(PassthruWrappedBlock(ConsensusFullBlock::new(header, body)?))
    }
}

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
    /// IPC path (post-CheckTx): polled in [`Stream::poll_next`] like ETH `poll_txs`.
    ipc_checked: Option<ReceiverStream<Vec<Bytes>>>,
    forward_egress: VecDeque<Bytes>,
    pending_app_commits: BTreeMap<SeqNum, BPT::ValidatedBlock>,
    events: VecDeque<MonadEvent<ST, SCT, CosmosExecutionProtocol>>,
    waker: Option<Waker>,
    store: std::sync::Arc<std::sync::Mutex<CosmosCommitStore>>,
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
        store: std::sync::Arc<std::sync::Mutex<CosmosCommitStore>>,
        ipc_checked_rx: Option<tokio::sync::mpsc::Receiver<Vec<Bytes>>>,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            pending_txs: IndexedCosmosMempool::new(),
            ipc_checked: ipc_checked_rx.map(ReceiverStream::new),
            forward_egress: VecDeque::new(),
            pending_app_commits: BTreeMap::new(),
            events: VecDeque::new(),
            waker: None,
            store,
            metrics: ExecutorMetrics::default(),
            _phantom: PhantomData,
        }
    }

    /// Txs that already passed ABCI CheckTx (IPC pipeline): insert pool + schedule P2P forward.
    fn apply_checked_ingress_batch(&mut self, txs: Vec<Bytes>) {
        let n = txs.len();
        let wire_bytes: usize = txs.iter().map(|b| b.len()).sum();
        let mut accepted = 0usize;
        let mut duplicates = 0usize;
        for tx in txs {
            let fwd = tx.clone();
            if self.pending_txs.try_push(tx) {
                accepted += 1;
                self.forward_egress.push_back(fwd);
            } else {
                duplicates += 1;
            }
        }
        if n > 0 {
            info!(
                count = n,
                accepted,
                duplicates,
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

    /// Drop txs from the local mempool whose raw bytes appear in this committed execution body
    /// (e.g. included via `PrepareProposal` or matching forwarded txs).
    fn purge_mempool_txs_for_committed_block(&mut self, block: &BPT::ValidatedBlock) {
        for tx in block.body().execution_body.txs.iter() {
            self.pending_txs.remove_by_raw(tx.as_slice());
        }
    }

    fn cosmos_synthetic_finalized_header(height: u64, app_hash: Vec<u8>) -> CosmosFinalizedHeader {
        CosmosFinalizedHeader {
            height,
            app_hash,
            tx_results_hash: Vec::new(),
            validator_updates_hash: Vec::new(),
            finalize_block_response: Vec::new(),
            commit_response: Vec::new(),
            retain_height: 0,
        }
    }

    /// If [`CosmosCommitStore`] was advanced (e.g. [`CosmosCommitStore::sync_with_abci_app`]) past
    /// heights still present in `pending_app_commits`, drop those entries and purge mempool by the
    /// corresponding block bodies so we do not stall waiting for a height already on disk.
    fn flush_stale_pending_app_commits(&mut self) {
        let latest = self
            .store
            .lock()
            .unwrap()
            .latest()
            .unwrap_or(GENESIS_SEQ_NUM);
        let stale: Vec<SeqNum> = self
            .pending_app_commits
            .range(..=latest)
            .map(|(k, _)| *k)
            .collect();
        for h in stale {
            if let Some(block) = self.pending_app_commits.remove(&h) {
                self.purge_mempool_txs_for_committed_block(&block);
                info!(
                    height = h.0,
                    latest_disk = latest.0,
                    "cosmos txpool: removed stale pending commit (already persisted on disk)"
                );
            }
        }
    }

    fn drain_app_commits(&mut self) {
        if let Err(err) = self
            .store
            .lock()
            .unwrap()
            .sync_with_abci_app(&self.endpoint)
        {
            warn!(
                ?err,
                endpoint = %self.endpoint,
                "CosmosCommitStore::sync_with_abci_app failed; if the ABCI app is >1 height ahead of disk, drain will stall until state is aligned (see error text)"
            );
        }

        loop {
            self.flush_stale_pending_app_commits();

            let latest_app_height = self
                .store
                .lock()
                .unwrap()
                .latest()
                .unwrap_or(GENESIS_SEQ_NUM);
            let next_height = latest_app_height + SeqNum(1);
            let Some(block) = self.pending_app_commits.remove(&next_height) else {
                break;
            };

            let header = block.header().execution_inputs.clone();
            let body = block.body().execution_body.clone();
            let endpoint = self.endpoint.clone();

            let info = match block_on_async(async { info_via_transport(&endpoint).await }) {
                Ok(i) => i,
                Err(err) => {
                    warn!(?err, "ABCI Info failed before FinalizeBlock; will retry");
                    self.pending_app_commits.insert(next_height, block);
                    break;
                }
            };
            let abci_height = info.last_block_height.max(0) as u64;
            if abci_height >= next_height.0 {
                if abci_height == next_height.0 {
                    info!(
                        next_height = next_height.0,
                        "ABCI app already committed this height; persisting local header from Info (no duplicate FinalizeBlock)"
                    );
                    let synthetic = Self::cosmos_synthetic_finalized_header(
                        next_height.0,
                        info.last_block_app_hash.clone(),
                    );
                    if let Err(err) = self.store.lock().unwrap().commit(synthetic) {
                        warn!(?err, "failed to persist synthetic cosmos commit");
                        self.pending_app_commits.insert(next_height, block);
                        break;
                    }
                    self.purge_mempool_txs_for_committed_block(&block);
                    continue;
                }
                warn!(
                    next_height = next_height.0,
                    abci_height,
                    "ABCI last_block_height is ahead of the pending height we would commit; cannot use Info app_hash for this height and FinalizeBlock would be invalid. Align with CosmosCommitStore::sync_with_abci_app, reset app home, or remove cosmos-commits when the gap is >1"
                );
                self.pending_app_commits.insert(next_height, block);
                break;
            }

            let finalize_request = finalize_request_from_inputs(&header, &body);
            let result = block_on_async(async move {
                finalize_and_commit_via_transport(&endpoint, finalize_request).await
            });

            match result {
                Ok((finalize_block, commit)) => {
                    match CosmosFinalizedHeader::from_abci_responses(
                        block.get_seq_num().0,
                        &finalize_block,
                        &commit,
                    ) {
                        Ok(header) => {
                            let committed_height = header.height;
                            let committed_txs = body.txs.len();
                            if let Err(err) = self.store.lock().unwrap().commit(header) {
                                warn!(?err, "failed to persist cosmos commit");
                                self.pending_app_commits.insert(next_height, block);
                                break;
                            }
                            self.purge_mempool_txs_for_committed_block(&block);
                            println!(
                                "msg=txs height={} txs={}",
                                committed_height,
                                committed_txs,
                            );
                            let _ = performance_monitor::record_step(committed_height, "new_height");
                            performance_monitor::flush_block(committed_height);
                        }
                        Err(err) => {
                            warn!(?err, "failed to encode cosmos commit");
                            self.pending_app_commits.insert(next_height, block);
                            break;
                        }
                    }
                }
                Err(err) => {
                    warn!(?err, seq_num = next_height.0, "FinalizeBlock/Commit failed");
                    self.pending_app_commits.insert(next_height, block);
                    break;
                }
            }
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
                    let latest_app_height = self
                        .store
                        .lock()
                        .unwrap()
                        .latest()
                        .unwrap_or(GENESIS_SEQ_NUM);
                    let max_allowed_seq_num = latest_app_height + SeqNum(COSMOS_MAX_AHEAD_BLOCKS);
                    if seq_num > max_allowed_seq_num {
                        warn!(
                            requested_seq_num = seq_num.0,
                            max_allowed_seq_num = max_allowed_seq_num.0,
                            latest_app_height = latest_app_height.0,
                            "skipping proposal while application height lags too far behind consensus"
                        );
                        continue;
                    }

                    let header = ProposedCosmosHeader {
                        height: seq_num.0,
                        max_tx_bytes: proposal_byte_limit,
                        time_ns: timestamp_ns,
                        local_last_commit: encode_message(&ExtendedCommitInfo::default())
                            .unwrap_or_default(),
                        misbehavior: Default::default(),
                        next_validators_hash: Vec::new(),
                        proposer_address: Vec::new(),
                    };

                    // Tx list for the block comes from ABCI `PrepareProposal` (application layer), not
                    // from the local mempool. Mempool is only pruned after commit (see
                    // `purge_mempool_txs_for_committed_block`).
                    let prepared_txs = if latest_app_height == GENESIS_SEQ_NUM {
                        info!(
                            seq_num = seq_num.0,
                            "bypassing PrepareProposal while application is still at genesis state"
                        );
                        Ok(Vec::new())
                    } else {
                        let endpoint = self.endpoint.clone();
                        let prepare_request =
                            prepare_request_from_header(&header, Vec::new());
                        block_on_async(async move {
                            prepare_proposal_via_transport(&endpoint, prepare_request)
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
                                    latest_app_height = latest_app_height.0,
                                    n_included,
                                    included_bytes,
                                    mempool_pending = self.pending_txs.pending_len(),
                                    "cosmos txpool: proposal txs from PrepareProposal (app); local mempool unchanged"
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
                TxPoolCommand::InsertForwardedTxs {
                    txs,
                    sender: _,
                } => {
                    let n = txs.len();
                    let wire_bytes: usize = txs.iter().map(|b| b.len()).sum();
                    let mut accepted = 0usize;
                    let mut duplicates = 0usize;
                    let mut check_reject = 0usize;
                    let endpoint = self.endpoint.clone();
                    let mut to_apply = Vec::new();
                    for tx in txs {
                        match block_on_async(async {
                            check_tx_via_transport(&endpoint, tx.as_ref()).await
                        }) {
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
                    // 这里其他节点过来的数据不需要二次转发
                    for tx in to_apply {
                        if self.pending_txs.try_push(tx) {
                            accepted += 1;
                        } else {
                            duplicates += 1;
                        }
                    }
            
                    if n > 0 {
                        info!(
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
                        self.pending_app_commits.insert(block.get_seq_num(), block);
                    }
                    self.drain_app_commits();
                }
                TxPoolCommand::EnterRound { .. } | TxPoolCommand::Reset { .. } => {}
            }
        }
    }

    fn metrics(&self) -> ExecutorMetricsChain<'_> {
        self.metrics.as_ref().into()
    }
}

impl<ST, SCT, BPT, SBT, CCT, CRT> futures::Stream for CosmosTxPoolExecutor<ST, SCT, BPT, SBT, CCT, CRT>
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
                return Poll::Ready(Some(MonadEvent::MempoolEvent(MempoolEvent::ForwardTxs(
                    batch,
                ))));
            }
        }

        this.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

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
    fetches_tx:
        tokio::sync::mpsc::UnboundedSender<
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
                    info!(block_num, "committed cosmos block");
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use monad_crypto::NopSignature;
    use monad_multi_sig::MultiSig;
    use monad_cosmos_types::{CosmosExecutionProtocol, CosmosFinalizedHeader};
    use tempfile::tempdir;

    use super::{CosmosCommitStore, CosmosStateBackend};
    use monad_state_backend::StateBackend;
    use monad_types::{BlockId, SeqNum};

    #[test]
    fn commit_store_roundtrip() {
        let dir = tempdir().unwrap();
        let mut store = CosmosCommitStore::new(dir.path().to_path_buf()).unwrap();
        let header = CosmosFinalizedHeader {
            height: 7,
            app_hash: vec![1, 2, 3],
            tx_results_hash: vec![4],
            validator_updates_hash: vec![5],
            finalize_block_response: vec![6],
            commit_response: vec![7],
            retain_height: 7,
        };
        store.commit(header.clone()).unwrap();

        let reopened = CosmosCommitStore::new(dir.path().to_path_buf()).unwrap();
        assert_eq!(reopened.get(&SeqNum(7)), Some(&header));
        assert_eq!(reopened.latest(), Some(SeqNum(7)));
        assert_eq!(reopened.earliest(), Some(SeqNum(7)));
    }

    #[test]
    fn state_backend_reads_persisted_commit() {
        let dir = tempdir().unwrap();
        let mut store = CosmosCommitStore::new(dir.path().to_path_buf()).unwrap();
        store
            .commit(CosmosFinalizedHeader {
                height: 9,
                app_hash: vec![9],
                tx_results_hash: vec![],
                validator_updates_hash: vec![],
                finalize_block_response: vec![],
                commit_response: vec![],
                retain_height: 9,
            })
            .unwrap();

        let backend = CosmosStateBackend::new(Arc::new(Mutex::new(store)));
        let block_id = BlockId(monad_types::Hash([0; 32]));
        let result = <CosmosStateBackend as StateBackend<
            NopSignature,
            MultiSig<NopSignature>,
            CosmosExecutionProtocol,
        >>::get_execution_result(&backend, &block_id, &SeqNum(9), true)
        .unwrap();
        assert_eq!(result.height, 9);
        assert_eq!(
            <CosmosStateBackend as StateBackend<
                NopSignature,
                MultiSig<NopSignature>,
                CosmosExecutionProtocol,
            >>::raw_read_latest_finalized_block(&backend),
            Some(SeqNum(9))
        );
    }
}
