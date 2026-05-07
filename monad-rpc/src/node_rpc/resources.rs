// Shared state injected into every Node RPC handler via actix-web `web::Data`.

use std::path::PathBuf;

/// Resources available to all Node RPC handlers.
///
/// This is intentionally minimal: it holds only what is needed to serve the
/// endpoints implemented so far.  New fields should be added as more endpoints
/// are implemented.
#[derive(Clone)]
pub struct NodeRpcResources {
    /// Chain-id string as used by CometBFT / Cosmos SDK (e.g. "injective-1")
    pub chain_id: String,

    /// Unix-socket path of the Cosmos tx-pool IPC server inside monad-bft
    /// (the same socket that `monad-cosmos-integration::cosmos_txpool_ipc`
    /// listens on).  When `None`, broadcast endpoints return an error.
    pub cosmos_ipc_path: Option<PathBuf>,
}

impl NodeRpcResources {
    pub fn new(chain_id: impl Into<String>, cosmos_ipc_path: Option<PathBuf>) -> Self {
        Self {
            chain_id:        chain_id.into(),
            cosmos_ipc_path,
        }
    }
}
