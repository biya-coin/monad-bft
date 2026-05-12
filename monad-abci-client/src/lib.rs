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

//! ABCI Client for communicating with the application layer (ABCI server).
//!
//! This module provides client interfaces for ABCI (Application Blockchain Interface),
//! following the design of CometBFT's proxy connections but in Rust for async/await.
//!
//! # Architecture
//!
//! Similar to CometBFT's `cometbft/proxy/app_conn.go`, this module defines several traits
//! that partition ABCI methods by their execution context:
//!
//! - [`AbciClientConsensus`]: Consensus-phase methods (CheckTx is NOT here)
//! - [`AbciClientMempool`]: Mempool-phase methods
//! - [`AbciClientQuery`]: Query methods
//! - [`AbciClientSnapshot`]: State sync snapshot methods
//!
//! Each trait can be implemented with different transports (GRPC, Unix socket, TCP).

pub mod consensus;
pub mod error;
pub mod mempool;
pub mod query;
pub mod snapshot;
pub mod transport;

pub use consensus::AbciClientConsensus;
pub use error::AbciClientError;
pub use mempool::AbciClientMempool;
pub use query::AbciClientQuery;
pub use snapshot::AbciClientSnapshot;
pub use transport::{AbciTransportConfig, GrpcAbciClient, SocketAbciClient};
