//! Minimal CometBFT-compatible mempool RPC (`broadcast_tx_async` / `broadcast_tx_sync`).
//!
//! Same wire format as `monad-rpc` `node_rpc`, but without ETH RPC or `monad_execution`.

use std::path::PathBuf;
use std::sync::Arc;

use actix_web::{web, App, HttpResponse, HttpServer};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::cosmos_txpool_ipc;

#[derive(Clone)]
pub struct CometRpcState {
    pub mempool_ipc_path: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct CometResponse<T: Serialize> {
    pub jsonrpc: &'static str,
    pub id: i64,
    pub result: T,
}

impl<T: Serialize> CometResponse<T> {
    pub fn with_id(id: i64, result: T) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct CometError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CometErrorResponse {
    pub jsonrpc: &'static str,
    pub id: i64,
    pub error: CometError,
}

impl CometErrorResponse {
    pub fn new(id: i64, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            error: CometError {
                code,
                message: message.into(),
                data: None,
            },
        }
    }

    pub fn invalid_params(id: i64, message: impl Into<String>) -> Self {
        Self::new(id, -32602, message)
    }
}

#[derive(Debug, Serialize)]
pub struct ResultBroadcastTx {
    pub code: u32,
    pub data: String,
    pub log: String,
    pub codespace: String,
    pub hash: String,
}

impl ResultBroadcastTx {
    pub fn accepted(hash: [u8; 32]) -> Self {
        Self {
            code: 0,
            data: String::new(),
            log: String::new(),
            codespace: String::new(),
            hash: hex::encode_upper(hash),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CometRequest {
    pub jsonrpc: Option<String>,
    pub id: Option<Value>,
    pub method: String,
    pub params: Value,
}

impl CometRequest {
    pub fn id_i64(&self) -> i64 {
        match &self.id {
            Some(Value::Number(n)) => n.as_i64().unwrap_or(-1),
            _ => -1,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct BroadcastTxQuery {
    pub tx: String,
}

pub async fn run_server(listen: &str, mempool_ipc_path: PathBuf) -> std::io::Result<()> {
    let state = Arc::new(CometRpcState { mempool_ipc_path });
    eprintln!(
        "cosmos-txpool-feed serve: listen={listen} mempool={}",
        state.mempool_ipc_path.display()
    );
    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(state.clone()))
            .configure(configure_routes)
    })
    .bind(listen)?
    .run()
    .await
}

pub fn configure_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(web::resource("/").route(web::post().to(post_handler)))
        .service(
            web::resource("/broadcast_tx_async")
                .route(web::get().to(get_broadcast_tx_async))
                .route(web::post().to(post_broadcast_tx_async)),
        )
        .service(
            web::resource("/broadcast_tx_sync")
                .route(web::get().to(get_broadcast_tx_sync))
                .route(web::post().to(post_broadcast_tx_sync)),
        );
}

async fn post_handler(
    state: web::Data<Arc<CometRpcState>>,
    body: web::Json<CometRequest>,
) -> HttpResponse {
    let req = body.into_inner();
    dispatch(&state, &req.method, &req.params, req.id_i64()).await
}

async fn post_broadcast_tx_async(
    state: web::Data<Arc<CometRpcState>>,
    body: web::Json<CometRequest>,
) -> HttpResponse {
    let req = body.into_inner();
    dispatch(&state, "broadcast_tx_async", &req.params, req.id_i64()).await
}

async fn post_broadcast_tx_sync(
    state: web::Data<Arc<CometRpcState>>,
    body: web::Json<CometRequest>,
) -> HttpResponse {
    let req = body.into_inner();
    dispatch(&state, "broadcast_tx_sync", &req.params, req.id_i64()).await
}

async fn get_broadcast_tx_async(
    state: web::Data<Arc<CometRpcState>>,
    query: web::Query<BroadcastTxQuery>,
) -> HttpResponse {
    broadcast_tx_async(&state, &query.tx, -1).await
}

async fn get_broadcast_tx_sync(
    state: web::Data<Arc<CometRpcState>>,
    query: web::Query<BroadcastTxQuery>,
) -> HttpResponse {
    broadcast_tx_sync(&state, &query.tx, -1).await
}

async fn dispatch(
    state: &Arc<CometRpcState>,
    method: &str,
    params: &Value,
    id: i64,
) -> HttpResponse {
    match method {
        "broadcast_tx_async" => match extract_tx_param(params, id) {
            Ok(tx) => broadcast_tx_async(state, &tx, id).await,
            Err(resp) => resp,
        },
        "broadcast_tx_sync" => match extract_tx_param(params, id) {
            Ok(tx) => broadcast_tx_sync(state, &tx, id).await,
            Err(resp) => resp,
        },
        other => HttpResponse::Ok()
            .content_type("application/json")
            .json(CometErrorResponse::new(
                id,
                -32601,
                format!("method not found: {other}"),
            )),
    }
}

fn extract_tx_param(params: &Value, id: i64) -> Result<String, HttpResponse> {
    let tx = match params {
        Value::Object(map) => map
            .get("tx")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        Value::Array(arr) => arr.first().and_then(|v| v.as_str()).map(str::to_owned),
        Value::String(s) => Some(s.clone()),
        _ => None,
    };
    tx.ok_or_else(|| {
        HttpResponse::Ok()
            .content_type("application/json")
            .json(CometErrorResponse::invalid_params(
                id,
                "missing required parameter 'tx'",
            ))
    })
}

pub async fn broadcast_tx_async(state: &CometRpcState, tx_b64: &str, id: i64) -> HttpResponse {
    let tx_bytes = match decode_tx(tx_b64, id) {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let hash = tx_hash(&tx_bytes);
    if let Err(resp) = forward_tx(state, tx_bytes, id).await {
        return resp;
    }
    json_ok(id, ResultBroadcastTx::accepted(hash))
}

pub async fn broadcast_tx_sync(state: &CometRpcState, tx_b64: &str, id: i64) -> HttpResponse {
    broadcast_tx_async(state, tx_b64, id).await
}

fn decode_tx(tx_b64: &str, id: i64) -> Result<Vec<u8>, HttpResponse> {
    let trimmed = tx_b64.trim();
    let bytes = if let Some(hex_str) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        hex::decode(hex_str).map_err(|e| {
            warn!("broadcast_tx: hex decode failed: {e}");
            json_err(id, -32602, format!("hex decode error: {e}"))
        })?
    } else {
        BASE64.decode(trimmed).map_err(|e| {
            warn!("broadcast_tx: base64 decode failed: {e}");
            json_err(id, -32602, format!("base64 decode error: {e}"))
        })?
    };
    if bytes.is_empty() {
        return Err(json_err(id, -32602, "tx must not be empty"));
    }
    if let Err(reason) = lightweight_tx_check(&bytes) {
        warn!("broadcast_tx: lightweight check failed: {reason}");
        return Err(json_err(id, -32602, format!("invalid tx: {reason}")));
    }
    Ok(bytes)
}

fn lightweight_tx_check(bytes: &[u8]) -> Result<(), &'static str> {
    if bytes.len() < 2 {
        return Err("tx too short to be a valid protobuf message");
    }
    let wire_type = bytes[0] & 0x07;
    if wire_type != 2 {
        return Err("first protobuf field is not length-delimited (not a TxRaw)");
    }
    Ok(())
}

fn tx_hash(tx_bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(tx_bytes);
    hasher.finalize().into()
}

async fn forward_tx(
    state: &CometRpcState,
    tx_bytes: Vec<u8>,
    id: i64,
) -> Result<(), HttpResponse> {
    cosmos_txpool_ipc::feed_raw_txs(&state.mempool_ipc_path, vec![tx_bytes])
        .await
        .map_err(|e| {
            warn!("broadcast_tx: IPC forward failed: {e}");
            json_err(
                id,
                -32603,
                format!("failed to forward tx to mempool: {e}"),
            )
        })
}

fn json_ok(id: i64, result: ResultBroadcastTx) -> HttpResponse {
    HttpResponse::Ok()
        .content_type("application/json")
        .json(CometResponse::with_id(id, result))
}

fn json_err(id: i64, code: i32, message: impl Into<String>) -> HttpResponse {
    HttpResponse::Ok()
        .content_type("application/json")
        .json(CometErrorResponse::new(id, code, message))
}
