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

//! Consensus-phase ABCI client interface.
//!
//! These methods are called during consensus and block execution phases.
//! Unlike mempool CheckTx, these methods are not performance-critical
//! as they are executed sequentially per block.

use monad_cometbft_proto::cometbft::abci::v1::{
    CommitRequest, CommitResponse, ExtendVoteRequest, ExtendVoteResponse,
    FinalizeBlockRequest, FinalizeBlockResponse, InitChainRequest, InitChainResponse,
    PrepareProposalRequest, PrepareProposalResponse, ProcessProposalRequest,
    ProcessProposalResponse, VerifyVoteExtensionRequest, VerifyVoteExtensionResponse,
};

use crate::error::Result;

/// AbciClientConsensus defines methods used during consensus and block execution.
///
/// Corresponds to CometBFT's `proxy.AppConnConsensus` interface.
///
/// These methods are:
/// - Called synchronously during block execution
/// - Can take arbitrary time (not performance-critical per transaction)
/// - Must be deterministic and produce consistent results
#[async_trait::async_trait]
pub trait AbciClientConsensus: Send + Sync {
    /// InitChain is called once upon genesis to initialize the application state.
    async fn init_chain(&self, req: InitChainRequest) -> Result<InitChainResponse>;

    /// PrepareProposal is called when a validator is creating a new proposal.
    /// The application can modify the transactions to be included in the proposal.
    async fn prepare_proposal(
        &self,
        req: PrepareProposalRequest,
    ) -> Result<PrepareProposalResponse>;

    /// ProcessProposal is called when a validator receives a proposal from the network.
    /// The application validates the proposal and returns acceptance/rejection.
    async fn process_proposal(
        &self,
        req: ProcessProposalRequest,
    ) -> Result<ProcessProposalResponse>;

    /// ExtendVote is called when a validator is extending its vote with custom data.
    async fn extend_vote(&self, req: ExtendVoteRequest) -> Result<ExtendVoteResponse>;

    /// VerifyVoteExtension is called when a validator receives a vote with extensions.
    async fn verify_vote_extension(
        &self,
        req: VerifyVoteExtensionRequest,
    ) -> Result<VerifyVoteExtensionResponse>;

    /// FinalizeBlock is called to execute all transactions in a block and update the state.
    /// This is where the application's business logic executes transactions.
    async fn finalize_block(
        &self,
        req: FinalizeBlockRequest,
    ) -> Result<FinalizeBlockResponse>;

    /// Commit persists the new state to disk and returns the app hash.
    /// This is called after FinalizeBlock and must be atomic with state persistence.
    async fn commit(&self, req: CommitRequest) -> Result<CommitResponse>;
}
