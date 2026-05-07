// CometBFT RPC HTTP server – actix-web route configuration.
//
// Protocol notes (from the reference implementation):
//
//  1. GET  /<method>?param=value  – query-string style (most common in CLI)
//  2. POST /                      – JSON-RPC 2.0 body (used by SDK clients)
//  3. POST /<method>              – JSON-RPC body on method-specific path
//
// All three variants are handled here.  Responses always use HTTP 200 even for
// application-level errors (matching cometbft behaviour).
//
// Route registration is done via `configure_routes` which is passed to
// `App::configure()` in main.rs.

use actix_web::{web, HttpResponse};
use serde_json::Value;

use super::{
    handlers::broadcast_tx,
    resources::NodeRpcResources,
    types::{BroadcastTxQuery, CometErrorResponse, CometRequest},
};

// ---------------------------------------------------------------------------
// Route configuration – call App::configure(node_rpc::server::configure_routes)
// ---------------------------------------------------------------------------

pub fn configure_routes(cfg: &mut web::ServiceConfig) {
    cfg
        // POST / – JSON-RPC dispatch (all methods)
        .service(web::resource("/").route(web::post().to(post_handler)))
        // GET /broadcast_tx_async?tx=<base64>
        .service(
            web::resource("/broadcast_tx_async")
                .route(web::get().to(get_broadcast_tx_async))
                .route(web::post().to(post_broadcast_tx_async)),
        )
        // GET /broadcast_tx_sync?tx=<base64>
        .service(
            web::resource("/broadcast_tx_sync")
                .route(web::get().to(get_broadcast_tx_sync))
                .route(web::post().to(post_broadcast_tx_sync)),
        );
}

// ---------------------------------------------------------------------------
// POST / handler – JSON-RPC 2.0 dispatch
// ---------------------------------------------------------------------------

async fn post_handler(
    resources: web::Data<NodeRpcResources>,
    body: web::Json<CometRequest>,
) -> HttpResponse {
    let req = body.into_inner();
    let id = req.id_i64();

    dispatch(&resources, &req.method, &req.params, id).await
}

// ---------------------------------------------------------------------------
// Per-method POST handlers (POST /broadcast_tx_async etc.)
// These receive a JSON-RPC body but the method is already known from the path.
// ---------------------------------------------------------------------------

async fn post_broadcast_tx_async(
    resources: web::Data<NodeRpcResources>,
    body: web::Json<CometRequest>,
) -> HttpResponse {
    let req = body.into_inner();
    let id = req.id_i64();
    dispatch(&resources, "broadcast_tx_async", &req.params, id).await
}

async fn post_broadcast_tx_sync(
    resources: web::Data<NodeRpcResources>,
    body: web::Json<CometRequest>,
) -> HttpResponse {
    let req = body.into_inner();
    let id = req.id_i64();
    dispatch(&resources, "broadcast_tx_sync", &req.params, id).await
}

// ---------------------------------------------------------------------------
// GET handlers
// ---------------------------------------------------------------------------

async fn get_broadcast_tx_async(
    resources: web::Data<NodeRpcResources>,
    query: web::Query<BroadcastTxQuery>,
) -> HttpResponse {
    broadcast_tx::broadcast_tx_async(&resources, &query.tx, -1).await
}

async fn get_broadcast_tx_sync(
    resources: web::Data<NodeRpcResources>,
    query: web::Query<BroadcastTxQuery>,
) -> HttpResponse {
    broadcast_tx::broadcast_tx_sync(&resources, &query.tx, -1).await
}

// ---------------------------------------------------------------------------
// Central dispatch – routes a method name + params to the right handler
// ---------------------------------------------------------------------------

async fn dispatch(
    resources: &NodeRpcResources,
    method: &str,
    params: &Value,
    id: i64,
) -> HttpResponse {
    match method {
        "broadcast_tx_async" => {
            let tx = extract_tx_param(params, id);
            match tx {
                Ok(tx) => broadcast_tx::broadcast_tx_async(resources, &tx, id).await,
                Err(resp) => resp,
            }
        }
        "broadcast_tx_sync" => {
            let tx = extract_tx_param(params, id);
            match tx {
                Ok(tx) => broadcast_tx::broadcast_tx_sync(resources, &tx, id).await,
                Err(resp) => resp,
            }
        }
        other => method_not_found(id, other),
    }
}

// ---------------------------------------------------------------------------
// Parameter extraction helpers
// ---------------------------------------------------------------------------

/// Extract the `tx` field from a JSON-RPC params value.
///
/// CometBFT clients send params either as:
///   - object:  `{"tx": "<base64>"}`
///   - array:   `["<base64>"]`  (positional)
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

fn method_not_found(id: i64, method: &str) -> HttpResponse {
    HttpResponse::Ok()
        .content_type("application/json")
        .json(CometErrorResponse::new(
            id,
            -32601,
            format!("method not found: {method}"),
        ))
}
