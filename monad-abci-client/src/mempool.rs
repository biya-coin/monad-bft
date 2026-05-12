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

//! Mempool-phase ABCI client interface.
//!
//! These methods are called during the mempool phase and must be fast
//! as they are called for every incoming transaction.

use monad_cometbft_proto::cometbft::abci::v1::{CheckTxRequest, CheckTxResponse};

use crate::error::Result;

/// AbciClientMempool defines methods used during the mempool phase.
///
/// Corresponds to CometBFT's `proxy.AppConnMempool` interface.
///
/// These methods are:
/// - Called frequently for every incoming transaction
/// - Must be fast (< 100ms typically)
/// - Can be called concurrently (though the app may serialize them internally)
#[async_trait::async_trait]
pub trait AbciClientMempool: Send + Sync {
    /// CheckTx validates a transaction and checks if it should be added to the mempool.
    ///
    /// The application should perform:
    /// - Signature validation
    /// - Nonce/sequence checking
    /// - Balance checks
    /// - Fee validation
    ///
    /// The returned code should be:
    /// - 0 for successful validation
    /// - Non-zero for rejection (application-specific error code)
    async fn check_tx(&self, req: CheckTxRequest) -> Result<CheckTxResponse>;

    /// Flush tells the application to flush any pending data.
    /// This is called after a batch of CheckTx calls to ensure consistency.
    async fn flush(&self) -> Result<()>;
}
