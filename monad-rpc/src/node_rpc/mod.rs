// Node RPC – CometBFT-compatible RPC module for monad-rpc.
//
// Exposes a subset of the CometBFT JSON-RPC API on a dedicated TCP port
// (default 26657) so that Cosmos SDK tooling (biyachaind, ignite, etc.) can
// interact with a monad-bft chain without modification.
//
// Reference implementation: cometbft/rpc/

pub mod handlers;
pub mod resources;
pub mod server;
pub mod types;
