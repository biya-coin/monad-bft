// CometBFT RPC response types, mirroring cometbft/rpc/core/types/responses.go
// All field names and JSON keys match the reference implementation.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Generic JSON-RPC 2.0 envelope (CometBFT style: id = -1 for server-push)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct CometResponse<T: Serialize> {
    pub jsonrpc: &'static str,
    pub id:      i64,
    pub result:  T,
}

impl<T: Serialize> CometResponse<T> {
    /// Standard server-initiated response (id = -1)
    pub fn ok(result: T) -> Self {
        Self { jsonrpc: "2.0", id: -1, result }
    }

    /// Echo back the caller's request id
    pub fn with_id(id: i64, result: T) -> Self {
        Self { jsonrpc: "2.0", id, result }
    }
}

// ---------------------------------------------------------------------------
// Error response
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct CometError {
    pub code:    i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data:    Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CometErrorResponse {
    pub jsonrpc: &'static str,
    pub id:      i64,
    pub error:   CometError,
}

impl CometErrorResponse {
    pub fn new(id: i64, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            error: CometError { code, message: message.into(), data: None },
        }
    }

    pub fn internal(id: i64, message: impl Into<String>) -> Self {
        Self::new(id, -32603, message)
    }

    pub fn invalid_params(id: i64, message: impl Into<String>) -> Self {
        Self::new(id, -32602, message)
    }
}

// ---------------------------------------------------------------------------
// /broadcast_tx_async  /broadcast_tx_sync
// mirrors: ResultBroadcastTx in responses.go
// ---------------------------------------------------------------------------

/// Returned by broadcast_tx_async and broadcast_tx_sync.
/// `hash` is the upper-case hex-encoded SHA-256 of the raw transaction bytes.
#[derive(Debug, Serialize)]
pub struct ResultBroadcastTx {
    /// ABCI response code; 0 = OK
    pub code:      u32,
    /// Optional response data (hex encoded)
    pub data:      String,
    /// Human-readable log message
    pub log:       String,
    pub codespace: String,
    /// Upper-case hex SHA-256 of the raw tx bytes
    pub hash:      String,
}

impl ResultBroadcastTx {
    pub fn accepted(hash: [u8; 32]) -> Self {
        Self {
            code:      0,
            data:      String::new(),
            log:       String::new(),
            codespace: String::new(),
            hash:      hex::encode_upper(hash),
        }
    }

    pub fn from_check_tx(
        hash: [u8; 32],
        response: monad_cometbft_proto::cometbft::abci::v1::CheckTxResponse,
    ) -> Self {
        Self {
            code: response.code,
            data: hex::encode(response.data),
            log: response.log,
            codespace: response.codespace,
            hash: hex::encode_upper(hash),
        }
    }
}

// ---------------------------------------------------------------------------
// Incoming JSON-RPC request body (POST /)
// mirrors: RPCRequest in cometbft/rpc/jsonrpc/types/types.go
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CometRequest {
    #[serde(default = "default_jsonrpc")]
    pub jsonrpc: String,
    #[serde(default)]
    pub id:      serde_json::Value,
    pub method:  String,
    #[serde(default)]
    pub params:  serde_json::Value,
}

fn default_jsonrpc() -> String {
    "2.0".into()
}

impl CometRequest {
    /// Extract integer id, defaulting to -1 for notifications / missing ids.
    pub fn id_i64(&self) -> i64 {
        match &self.id {
            serde_json::Value::Number(n) => n.as_i64().unwrap_or(-1),
            serde_json::Value::String(s) => s.parse().unwrap_or(-1),
            _ => -1,
        }
    }
}

// ---------------------------------------------------------------------------
// Query-string params for GET /broadcast_tx_async?tx=<base64>
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct BroadcastTxQuery {
    pub tx: String,
}
