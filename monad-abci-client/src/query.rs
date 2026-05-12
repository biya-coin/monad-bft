// Copyright (C) 2025 Category Labs, Inc.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

//! Query-phase ABCI client interface.
//!
//! These methods are used for querying application state and are not part of consensus.

use monad_cometbft_proto::cometbft::abci::v1::{EchoRequest, EchoResponse, InfoRequest, InfoResponse, QueryRequest, QueryResponse};

use crate::error::Result;

/// AbciClientQuery defines methods used for querying application state.
///
/// Corresponds to CometBFT's `proxy.AppConnQuery` interface.
///
/// These methods are:
/// - Used for querying, not consensus
/// - Can be called at any time
/// - Typically not performance-critical for consensus
#[async_trait::async_trait]
pub trait AbciClientQuery: Send + Sync {
    /// Echo simply echoes back the given string (useful for testing connections).
    async fn echo(&self, req: EchoRequest) -> Result<EchoResponse>;

    /// Info returns information about the application state.
    /// This includes the app version, last block height, last block app hash, etc.
    async fn info(&self, req: InfoRequest) -> Result<InfoResponse>;

    /// Query allows the application to serve read-only queries about state.
    /// The semantics of query and its response depend on the application.
    async fn query(&self, req: QueryRequest) -> Result<QueryResponse>;
}
