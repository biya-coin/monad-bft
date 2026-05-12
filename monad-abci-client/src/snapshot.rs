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

//! State sync snapshot ABCI client interface.
//!
//! These methods are used for state synchronization and are not part of normal consensus.

use monad_cometbft_proto::cometbft::abci::v1::{
    ApplySnapshotChunkRequest, ApplySnapshotChunkResponse, ListSnapshotsRequest,
    ListSnapshotsResponse, LoadSnapshotChunkRequest, LoadSnapshotChunkResponse,
    OfferSnapshotRequest, OfferSnapshotResponse,
};

use crate::error::Result;

/// AbciClientSnapshot defines methods used for state synchronization via snapshots.
///
/// Corresponds to CometBFT's `proxy.AppConnSnapshot` interface.
///
/// These methods are:
/// - Used only during state sync, not consensus
/// - Can be slow (state sync is a background operation)
/// - Called to transfer large state snapshots
#[async_trait::async_trait]
pub trait AbciClientSnapshot: Send + Sync {
    /// ListSnapshots lists available snapshots for state sync.
    async fn list_snapshots(
        &self,
        req: ListSnapshotsRequest,
    ) -> Result<ListSnapshotsResponse>;

    /// OfferSnapshot begins restoring a snapshot from the application.
    async fn offer_snapshot(
        &self,
        req: OfferSnapshotRequest,
    ) -> Result<OfferSnapshotResponse>;

    /// LoadSnapshotChunk loads a chunk from a snapshot being restored.
    async fn load_snapshot_chunk(
        &self,
        req: LoadSnapshotChunkRequest,
    ) -> Result<LoadSnapshotChunkResponse>;

    /// ApplySnapshotChunk applies a chunk to the snapshot being restored.
    async fn apply_snapshot_chunk(
        &self,
        req: ApplySnapshotChunkRequest,
    ) -> Result<ApplySnapshotChunkResponse>;
}
