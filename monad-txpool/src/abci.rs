use std::{fs, io, path::Path};

use chrono::{DateTime, Utc};
use monad_abci_client::{
    AbciClientConsensus, AbciClientError, AbciClientMempool, AbciClientQuery,
    AbciTransportConfig, GrpcAbciClient, SocketAbciClient,
};
use monad_cometbft_proto::cometbft::abci::v1::{
    CheckTxRequest, CheckTxType, CommitInfo, CommitRequest, ExtendedCommitInfo,
    FinalizeBlockRequest, InfoRequest, InitChainRequest, Misbehavior,
    PrepareProposalRequest, ProcessProposalRequest, QueryRequest,
};
use monad_cometbft_proto::cometbft::types::v1::{
    BlockParams, ConsensusParams, EvidenceParams, FeatureParams, SynchronyParams,
    ValidatorParams, VersionParams,
};
use monad_cosmos_types::{CosmosBlockBody, CosmosFinalizedHeader, ProposedCosmosHeader};
use monad_crypto::hasher::Hasher;
use prost::Message;
use prost_types::Timestamp;
use serde_json::Value;
use thiserror::Error;
use tokio::runtime::{Handle, Runtime};
use tracing::info;

#[derive(Debug, Error)]
pub enum CosmosTxPoolError {
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
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

impl From<AbciClientError> for CosmosTxPoolError {
    fn from(value: AbciClientError) -> Self {
        match value {
            AbciClientError::InvalidEndpoint(err) => Self::InvalidEndpoint(err),
            AbciClientError::Transport(err) => Self::Transport(err),
            AbciClientError::GrpcStatus(err) => Self::GrpcStatus(err),
            AbciClientError::Encode(err) => Self::Encode(err),
            AbciClientError::Decode(err) => Self::Decode(err),
            AbciClientError::ProposalRejected => Self::ProposalRejected,
            AbciClientError::Io(err) => Self::Io(err),
            AbciClientError::Timeout => Self::Transport("ABCI request timed out".to_owned()),
            AbciClientError::ConnectionClosed => {
                Self::Transport("ABCI connection closed".to_owned())
            }
        }
    }
}

pub fn block_on_async<F, T>(future: F) -> Result<T, CosmosTxPoolError>
where
    F: std::future::Future<Output = Result<T, CosmosTxPoolError>>,
{
    if let Ok(handle) = Handle::try_current() {
        tokio::task::block_in_place(|| handle.block_on(future))
    } else {
        Runtime::new().map_err(CosmosTxPoolError::Io)?.block_on(future)
    }
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

pub fn parse_timestamp_rfc3339(value: &str) -> Result<Timestamp, CosmosTxPoolError> {
    let parsed = DateTime::parse_from_rfc3339(value)
        .map_err(|err| CosmosTxPoolError::Transport(format!("invalid genesis_time: {err}")))?;
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
            max_txs: parse_i64_str(block.and_then(|v| v.get("max_txs"))),
        }),
        evidence: Some(EvidenceParams {
            max_age_num_blocks: parse_i64_str(evidence.and_then(|v| v.get("max_age_num_blocks"))),
            max_age_duration: parse_duration_nanos(evidence.and_then(|v| v.get("max_age_duration"))),
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

pub fn build_init_chain_request(genesis: &Value) -> Result<InitChainRequest, CosmosTxPoolError> {
    let genesis_time = genesis
        .get("genesis_time")
        .and_then(Value::as_str)
        .ok_or_else(|| CosmosTxPoolError::Transport("missing genesis_time".to_owned()))?;
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
        .map_err(|err| CosmosTxPoolError::Transport(format!("invalid app_state: {err}")))?;
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

fn socket_client_from_endpoint(endpoint: &str) -> Result<SocketAbciClient, CosmosTxPoolError> {
    match AbciTransportConfig::from_endpoint(endpoint) {
        AbciTransportConfig::Unix(path) => Ok(SocketAbciClient::new(path.to_string_lossy().into_owned())),
        AbciTransportConfig::Tcp(addr) => Ok(SocketAbciClient::new(addr)),
        AbciTransportConfig::Grpc(_) => Err(CosmosTxPoolError::InvalidEndpoint(
            "grpc endpoint passed to socket client".to_owned(),
        )),
    }
}

pub async fn check_tx(
    endpoint: &str,
    tx_bytes: &[u8],
) -> Result<monad_cometbft_proto::cometbft::abci::v1::CheckTxResponse, CosmosTxPoolError> {
    let request_msg = CheckTxRequest {
        tx: tx_bytes.to_vec(),
        r#type: CheckTxType::Check as i32,
    };
    match AbciTransportConfig::from_endpoint(endpoint) {
        AbciTransportConfig::Grpc(_) => GrpcAbciClient::new(endpoint)
            .await?
            .check_tx(request_msg)
            .await
            .map_err(Into::into),
        AbciTransportConfig::Unix(_) | AbciTransportConfig::Tcp(_) => {
            socket_client_from_endpoint(endpoint)?
                .check_tx(request_msg)
                .await
                .map_err(Into::into)
        }
    }
}

pub async fn prepare_proposal(
    endpoint: &str,
    request_msg: PrepareProposalRequest,
) -> Result<monad_cometbft_proto::cometbft::abci::v1::PrepareProposalResponse, CosmosTxPoolError> {
    match AbciTransportConfig::from_endpoint(endpoint) {
        AbciTransportConfig::Grpc(_) => GrpcAbciClient::new(endpoint)
            .await?
            .prepare_proposal(request_msg)
            .await
            .map_err(Into::into),
        AbciTransportConfig::Unix(_) | AbciTransportConfig::Tcp(_) => socket_client_from_endpoint(endpoint)?
            .prepare_proposal(request_msg)
            .await
            .map_err(Into::into),
    }
}

pub async fn info(
    endpoint: &str,
) -> Result<monad_cometbft_proto::cometbft::abci::v1::InfoResponse, CosmosTxPoolError> {
    let request = InfoRequest {
        version: String::new(),
        block_version: 0,
        p2p_version: 0,
        abci_version: String::new(),
    };
    match AbciTransportConfig::from_endpoint(endpoint) {
        AbciTransportConfig::Grpc(_) => GrpcAbciClient::new(endpoint)
            .await?
            .info(request)
            .await
            .map_err(Into::into),
        AbciTransportConfig::Unix(_) | AbciTransportConfig::Tcp(_) => socket_client_from_endpoint(endpoint)?
            .info(request)
            .await
            .map_err(Into::into),
    }
}

pub async fn init_chain(
    endpoint: &str,
    request_msg: InitChainRequest,
) -> Result<monad_cometbft_proto::cometbft::abci::v1::InitChainResponse, CosmosTxPoolError> {
    match AbciTransportConfig::from_endpoint(endpoint) {
        AbciTransportConfig::Grpc(_) => GrpcAbciClient::new(endpoint)
            .await?
            .init_chain(request_msg)
            .await
            .map_err(Into::into),
        AbciTransportConfig::Unix(_) | AbciTransportConfig::Tcp(_) => socket_client_from_endpoint(endpoint)?
            .init_chain(request_msg)
            .await
            .map_err(Into::into),
    }
}

pub async fn process_proposal(
    endpoint: &str,
    request_msg: ProcessProposalRequest,
) -> Result<monad_cometbft_proto::cometbft::abci::v1::ProcessProposalResponse, CosmosTxPoolError> {
    match AbciTransportConfig::from_endpoint(endpoint) {
        AbciTransportConfig::Grpc(_) => GrpcAbciClient::new(endpoint)
            .await?
            .process_proposal(request_msg)
            .await
            .map_err(Into::into),
        AbciTransportConfig::Unix(_) | AbciTransportConfig::Tcp(_) => socket_client_from_endpoint(endpoint)?
            .process_proposal(request_msg)
            .await
            .map_err(Into::into),
    }
}

pub async fn finalize_block(
    endpoint: &str,
    request_msg: FinalizeBlockRequest,
) -> Result<monad_cometbft_proto::cometbft::abci::v1::FinalizeBlockResponse, CosmosTxPoolError> {
    match AbciTransportConfig::from_endpoint(endpoint) {
        AbciTransportConfig::Grpc(_) => GrpcAbciClient::new(endpoint)
            .await?
            .finalize_block(request_msg)
            .await
            .map_err(Into::into),
        AbciTransportConfig::Unix(_) | AbciTransportConfig::Tcp(_) => socket_client_from_endpoint(endpoint)?
            .finalize_block(request_msg)
            .await
            .map_err(Into::into),
    }
}

pub async fn commit(
    endpoint: &str,
) -> Result<monad_cometbft_proto::cometbft::abci::v1::CommitResponse, CosmosTxPoolError> {
    match AbciTransportConfig::from_endpoint(endpoint) {
        AbciTransportConfig::Grpc(_) => GrpcAbciClient::new(endpoint)
            .await?
            .commit(CommitRequest {})
            .await
            .map_err(Into::into),
        AbciTransportConfig::Unix(_) | AbciTransportConfig::Tcp(_) => socket_client_from_endpoint(endpoint)?
            .commit(CommitRequest {})
            .await
            .map_err(Into::into),
    }
}

pub async fn query_execution_result(
    endpoint: &str,
    height: u64,
) -> Result<CosmosFinalizedHeader, CosmosTxPoolError> {
    let req = QueryRequest {
        data: height.to_be_bytes().to_vec(),
        path: "monad/execution_result".to_owned(),
        height: 0,
        prove: false,
    };
    let resp = match AbciTransportConfig::from_endpoint(endpoint) {
        AbciTransportConfig::Grpc(_) => {
            GrpcAbciClient::new(endpoint).await?.query(req).await.map_err(CosmosTxPoolError::from)?
        }
        AbciTransportConfig::Unix(_) | AbciTransportConfig::Tcp(_) => {
            socket_client_from_endpoint(endpoint)?.query(req).await.map_err(CosmosTxPoolError::from)?
        }
    };
    if resp.code != 0 {
        return Err(CosmosTxPoolError::Transport(format!(
            "ABCI Query monad/execution_result height={height} code={} log={}",
            resp.code, resp.log
        )));
    }
    alloy_rlp::decode_exact(&resp.value)
        .map_err(|e| CosmosTxPoolError::Transport(format!("decode CosmosFinalizedHeader: {e}")))
}

fn timestamp_from_ns(timestamp_ns: u128) -> Timestamp {
    Timestamp {
        seconds: (timestamp_ns / 1_000_000_000) as i64,
        nanos: (timestamp_ns % 1_000_000_000) as i32,
    }
}

pub(crate) fn encode_message<M: Message>(message: &M) -> Result<Vec<u8>, prost::EncodeError> {
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

pub fn prepare_request_from_header(
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

pub fn process_request_from_inputs(
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

pub fn finalize_request_from_inputs(
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
) -> Result<AbciGenesisDebugInfo, CosmosTxPoolError> {
    let genesis_path = genesis_path.as_ref();
    let info = block_on_async(async { info(endpoint).await })?;

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
        let init_response = block_on_async(async { init_chain(endpoint, init_request).await })?;
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
) -> Result<AbciFirstBlockDebugInfo, CosmosTxPoolError> {
    let genesis_path = genesis_path.as_ref();
    let json: Value = serde_json::from_slice(&fs::read(genesis_path)?)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

    let init_request = build_init_chain_request(&json)?;
    let _ = block_on_async(async { init_chain(endpoint, init_request).await })?;

    let genesis_time = json
        .get("genesis_time")
        .and_then(Value::as_str)
        .ok_or_else(|| CosmosTxPoolError::Transport("missing genesis_time".to_owned()))?;
    let time = parse_timestamp_rfc3339(genesis_time)?;

    let finalize_request = FinalizeBlockRequest {
        txs: Vec::new(),
        decided_last_commit: Some(CommitInfo::default()),
        misbehavior: Vec::new(),
        hash: Vec::new(),
        height: 1,
        time: Some(time),
        next_validators_hash: Vec::new(),
        proposer_address: Vec::new(),
        syncing_to_height: 1,
    };
    let finalize_block = block_on_async(async { finalize_block(endpoint, finalize_request).await })?;
    let commit_resp = block_on_async(async { commit(endpoint).await })?;

    Ok(AbciFirstBlockDebugInfo {
        init_chain_sent: true,
        finalize_ok: true,
        commit_ok: true,
        finalize_app_hash_len: finalize_block.app_hash.len(),
        commit_retain_height: commit_resp.retain_height,
    })
}
