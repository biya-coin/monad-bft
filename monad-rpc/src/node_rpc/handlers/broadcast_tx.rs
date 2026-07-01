// CometBFT broadcast_tx_* handlers
//
// Reference: cometbft/rpc/core/mempool.go
//
// Three variants are supported, matching the reference implementation:
//
//  broadcast_tx_async  – fire-and-forget; returns as soon as the tx is queued.
//  broadcast_tx_sync   – waits for the IPC send to complete (monad-bft analog
//                        of CheckTx) but does NOT wait for block inclusion.
//  broadcast_tx_commit – not yet implemented (requires block-event subscription).
//
// Both GET and POST JSON-RPC are dispatched to the same inner async functions.
//
// Tx encoding: CometBFT clients send the raw transaction as standard base64.
// The hash returned is SHA-256 of the raw bytes, upper-case hex (matching the
// CometBFT wire format).

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use crate::node_rpc::{
    resources::NodeRpcResources,
    types::{CometErrorResponse, CometResponse, ResultBroadcastTx},
};

// ---------------------------------------------------------------------------
// Public entry-points (called from server.rs)
// ---------------------------------------------------------------------------

/// `broadcast_tx_async` – queue tx and return immediately.
///
/// Mirrors `BroadcastTxAsync` in mempool.go:
///   - Does NOT wait for CheckTx.
///   - Returns `{code:0, hash:"..."}` as soon as the tx is forwarded.
pub async fn broadcast_tx_async(
    resources: &NodeRpcResources,
    tx_b64: &str,
    id: i64,
) -> actix_web::HttpResponse {
    let tx_bytes = match decode_tx(tx_b64, id) {
        Ok(b) => b,
        Err(resp) => return resp,
    };

    let hash = tx_hash(&tx_bytes);
    debug!(hash = %hex::encode_upper(hash), "broadcast_tx_async: forwarding tx");

    if let Err(resp) = forward_tx(resources, tx_bytes, id).await {
        return resp;
    }

    json_ok(id, ResultBroadcastTx::accepted(hash))
}

/// `broadcast_tx_sync` – forward tx and wait for CheckTx.
///
/// Mirrors `BroadcastTxSync` in mempool.go:
///   - Waits for monad-node to run ABCI CheckTx through the cosmos-txpool-ipc
///     socket.
///   - Does NOT wait for block inclusion.
///   - Returns the CheckTx code/log/codespace on success.
pub async fn broadcast_tx_sync(
    resources: &NodeRpcResources,
    tx_b64: &str,
    id: i64,
) -> actix_web::HttpResponse {
    let tx_bytes = match decode_tx(tx_b64, id) {
        Ok(b) => b,
        Err(resp) => return resp,
    };

    let hash = tx_hash(&tx_bytes);
    debug!(hash = %hex::encode_upper(hash), "broadcast_tx_sync: forwarding tx");

    let response = match forward_tx_sync(resources, tx_bytes, id).await {
        Ok(response) => response,
        Err(resp) => return resp,
    };

    json_ok(id, ResultBroadcastTx::from_check_tx(hash, response))
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Decode a base64-encoded transaction sent by a Cosmos SDK client.
fn decode_tx(tx_b64: &str, id: i64) -> Result<Vec<u8>, actix_web::HttpResponse> {
    // Trim whitespace that some clients may add.
    let trimmed = tx_b64.trim();

    // Some clients send 0x-prefixed hex instead of base64; handle both.
    let bytes = if let Some(hex_str) = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")) {
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

    // Lightweight sanity check: a Cosmos SDK TxRaw is a protobuf message.
    // Field 1 (body_bytes) and field 2 (auth_info_bytes) must be present and
    // non-empty for a well-formed transaction.  We do a best-effort check here
    // and let biyachaind perform the authoritative validation.
    if let Err(reason) = lightweight_tx_check(&bytes) {
        warn!("broadcast_tx: lightweight check failed: {reason}");
        return Err(json_err(id, -32602, format!("invalid tx: {reason}")));
    }

    Ok(bytes)
}

/// Very lightweight check that the bytes look like a protobuf TxRaw.
/// We only verify that at least one field can be decoded; full validation
/// is biyachaind's responsibility.
fn lightweight_tx_check(bytes: &[u8]) -> Result<(), &'static str> {
    if bytes.len() < 2 {
        return Err("tx too short to be a valid protobuf message");
    }
    // First byte of a protobuf varint must have a valid wire-type (0-5).
    // For TxRaw, field 1 (body_bytes) has wire type 2 (length-delimited).
    // Tag byte = (field_number << 3) | wire_type = (1 << 3) | 2 = 0x0a.
    // We accept any length-delimited field (wire type 2) as the first tag.
    let wire_type = bytes[0] & 0x07;
    if wire_type != 2 {
        return Err("first protobuf field is not length-delimited (not a TxRaw)");
    }
    Ok(())
}

/// SHA-256 of the raw transaction bytes – the hash CometBFT uses for Tx.
fn tx_hash(tx_bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(tx_bytes);
    hasher.finalize().into()
}

/// Send the raw bytes to the cosmos-txpool-ipc socket inside monad-bft.
async fn forward_tx(
    resources: &NodeRpcResources,
    tx_bytes: Vec<u8>,
    id: i64,
) -> Result<(), actix_web::HttpResponse> {
    let ipc_path = resources.cosmos_ipc_path.as_ref().ok_or_else(|| {
        json_err(id, -32603, "cosmos IPC path not configured; broadcast unavailable")
    })?;

    monad_txpool::cosmos_txpool_ipc::feed_raw_txs(ipc_path, vec![tx_bytes])
        .await
        .map_err(|e| {
            warn!("broadcast_tx: IPC forward failed: {e}");
            json_err(id, -32603, format!("failed to forward tx to mempool: {e}"))
        })
}

async fn forward_tx_sync(
    resources: &NodeRpcResources,
    tx_bytes: Vec<u8>,
    id: i64,
) -> Result<monad_cometbft_proto::cometbft::abci::v1::CheckTxResponse, actix_web::HttpResponse> {
    let ipc_path = resources.cosmos_ipc_path.as_ref().ok_or_else(|| {
        json_err(
            id,
            -32603,
            "cosmos IPC path not configured; broadcast unavailable",
        )
    })?;

    monad_txpool::cosmos_txpool_ipc::feed_raw_tx_sync(ipc_path, tx_bytes)
        .await
        .map_err(|e| {
            warn!("broadcast_tx_sync: IPC forward failed: {e}");
            json_err(id, -32603, format!("failed to forward tx to mempool: {e}"))
        })
}

// ---------------------------------------------------------------------------
// Response helpers
// ---------------------------------------------------------------------------

fn json_ok(id: i64, result: ResultBroadcastTx) -> actix_web::HttpResponse {
    actix_web::HttpResponse::Ok()
        .content_type("application/json")
        .json(CometResponse::with_id(id, result))
}

fn json_err(id: i64, code: i32, message: impl Into<String>) -> actix_web::HttpResponse {
    actix_web::HttpResponse::Ok() // CometBFT always returns HTTP 200
        .content_type("application/json")
        .json(CometErrorResponse::new(id, code, message))
}
