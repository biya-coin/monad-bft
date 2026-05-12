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

//! GRPC-based ABCI client implementation.

use monad_cometbft_proto::cometbft::abci::v1::{
    abci_service_client::AbciServiceClient, CheckTxRequest, CheckTxResponse, CommitRequest,
    CommitResponse, EchoRequest, EchoResponse, ExtendVoteRequest, ExtendVoteResponse,
    FinalizeBlockRequest, FinalizeBlockResponse, InfoRequest, InfoResponse, InitChainRequest,
    InitChainResponse, ProcessProposalRequest, ProcessProposalResponse, PrepareProposalRequest,
    PrepareProposalResponse, QueryRequest, QueryResponse, VerifyVoteExtensionRequest,
    VerifyVoteExtensionResponse, ApplySnapshotChunkRequest, ApplySnapshotChunkResponse,
    FlushRequest,
    ListSnapshotsRequest, ListSnapshotsResponse, LoadSnapshotChunkRequest,
    LoadSnapshotChunkResponse, OfferSnapshotRequest, OfferSnapshotResponse,
};
use tonic::transport::Channel;

use crate::consensus::AbciClientConsensus;
use crate::error::{AbciClientError, Result};
use crate::mempool::AbciClientMempool;
use crate::query::AbciClientQuery;
use crate::snapshot::AbciClientSnapshot;

/// GRPC-based ABCI client.
///
/// Uses tonic for GRPC communication with the application.
#[derive(Clone)]
pub struct GrpcAbciClient {
    client: AbciServiceClient<Channel>,
}

impl GrpcAbciClient {
    /// Create a new GRPC ABCI client connecting to the given endpoint.
    pub async fn new(endpoint: &str) -> Result<Self> {
        let channel = tonic::transport::Endpoint::from_shared(endpoint.to_owned())
            .map_err(|err| AbciClientError::InvalidEndpoint(err.to_string()))?
            .connect()
            .await
            .map_err(|err| AbciClientError::Transport(err.to_string()))?;

        Ok(Self {
            client: AbciServiceClient::new(channel),
        })
    }
}

#[async_trait::async_trait]
impl AbciClientConsensus for GrpcAbciClient {
    async fn init_chain(&self, req: InitChainRequest) -> Result<InitChainResponse> {
        self.client
            .clone()
            .init_chain(req)
            .await
            .map(|resp| resp.into_inner())
            .map_err(|err| AbciClientError::GrpcStatus(err.to_string()))
    }

    async fn prepare_proposal(
        &self,
        req: PrepareProposalRequest,
    ) -> Result<PrepareProposalResponse> {
        self.client
            .clone()
            .prepare_proposal(req)
            .await
            .map(|resp| resp.into_inner())
            .map_err(|err| AbciClientError::GrpcStatus(err.to_string()))
    }

    async fn process_proposal(
        &self,
        req: ProcessProposalRequest,
    ) -> Result<ProcessProposalResponse> {
        self.client
            .clone()
            .process_proposal(req)
            .await
            .map(|resp| resp.into_inner())
            .map_err(|err| AbciClientError::GrpcStatus(err.to_string()))
    }

    async fn extend_vote(&self, req: ExtendVoteRequest) -> Result<ExtendVoteResponse> {
        self.client
            .clone()
            .extend_vote(req)
            .await
            .map(|resp| resp.into_inner())
            .map_err(|err| AbciClientError::GrpcStatus(err.to_string()))
    }

    async fn verify_vote_extension(
        &self,
        req: VerifyVoteExtensionRequest,
    ) -> Result<VerifyVoteExtensionResponse> {
        self.client
            .clone()
            .verify_vote_extension(req)
            .await
            .map(|resp| resp.into_inner())
            .map_err(|err| AbciClientError::GrpcStatus(err.to_string()))
    }

    async fn finalize_block(
        &self,
        req: FinalizeBlockRequest,
    ) -> Result<FinalizeBlockResponse> {
        self.client
            .clone()
            .finalize_block(req)
            .await
            .map(|resp| resp.into_inner())
            .map_err(|err| AbciClientError::GrpcStatus(err.to_string()))
    }

    async fn commit(&self, req: CommitRequest) -> Result<CommitResponse> {
        self.client
            .clone()
            .commit(req)
            .await
            .map(|resp| resp.into_inner())
            .map_err(|err| AbciClientError::GrpcStatus(err.to_string()))
    }
}

#[async_trait::async_trait]
impl AbciClientMempool for GrpcAbciClient {
    async fn check_tx(&self, req: CheckTxRequest) -> Result<CheckTxResponse> {
        self.client
            .clone()
            .check_tx(req)
            .await
            .map(|resp| resp.into_inner())
            .map_err(|err| AbciClientError::GrpcStatus(err.to_string()))
    }

    async fn flush(&self) -> Result<()> {
        self.client
            .clone()
            .flush(FlushRequest {})
            .await
            .map(|_| ())
            .map_err(|err| AbciClientError::GrpcStatus(err.to_string()))
    }
}

#[async_trait::async_trait]
impl AbciClientQuery for GrpcAbciClient {
    async fn echo(&self, req: EchoRequest) -> Result<EchoResponse> {
        self.client
            .clone()
            .echo(req)
            .await
            .map(|resp| resp.into_inner())
            .map_err(|err| AbciClientError::GrpcStatus(err.to_string()))
    }

    async fn info(&self, req: InfoRequest) -> Result<InfoResponse> {
        self.client
            .clone()
            .info(req)
            .await
            .map(|resp| resp.into_inner())
            .map_err(|err| AbciClientError::GrpcStatus(err.to_string()))
    }

    async fn query(&self, req: QueryRequest) -> Result<QueryResponse> {
        self.client
            .clone()
            .query(req)
            .await
            .map(|resp| resp.into_inner())
            .map_err(|err| AbciClientError::GrpcStatus(err.to_string()))
    }
}

#[async_trait::async_trait]
impl AbciClientSnapshot for GrpcAbciClient {
    async fn list_snapshots(
        &self,
        req: ListSnapshotsRequest,
    ) -> Result<ListSnapshotsResponse> {
        self.client
            .clone()
            .list_snapshots(req)
            .await
            .map(|resp| resp.into_inner())
            .map_err(|err| AbciClientError::GrpcStatus(err.to_string()))
    }

    async fn offer_snapshot(
        &self,
        req: OfferSnapshotRequest,
    ) -> Result<OfferSnapshotResponse> {
        self.client
            .clone()
            .offer_snapshot(req)
            .await
            .map(|resp| resp.into_inner())
            .map_err(|err| AbciClientError::GrpcStatus(err.to_string()))
    }

    async fn load_snapshot_chunk(
        &self,
        req: LoadSnapshotChunkRequest,
    ) -> Result<LoadSnapshotChunkResponse> {
        self.client
            .clone()
            .load_snapshot_chunk(req)
            .await
            .map(|resp| resp.into_inner())
            .map_err(|err| AbciClientError::GrpcStatus(err.to_string()))
    }

    async fn apply_snapshot_chunk(
        &self,
        req: ApplySnapshotChunkRequest,
    ) -> Result<ApplySnapshotChunkResponse> {
        self.client
            .clone()
            .apply_snapshot_chunk(req)
            .await
            .map(|resp| resp.into_inner())
            .map_err(|err| AbciClientError::GrpcStatus(err.to_string()))
    }
}
