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

//! ABCI transport implementations for GRPC and Unix socket connections.

pub mod grpc;
pub mod socket;

pub use grpc::GrpcAbciClient;
pub use socket::SocketAbciClient;

use std::path::PathBuf;

/// Configuration for ABCI transport.
#[derive(Debug, Clone)]
pub enum AbciTransportConfig {
    /// GRPC endpoint (e.g., "http://localhost:26658")
    Grpc(String),

    /// Unix socket endpoint (e.g., "/tmp/abci.sock")
    Unix(PathBuf),

    /// TCP endpoint (e.g., "localhost:26658")
    Tcp(String),
}

impl AbciTransportConfig {
    /// Parse an endpoint string and return the appropriate transport config.
    pub fn from_endpoint(endpoint: &str) -> Self {
        if endpoint.starts_with("unix://") {
            AbciTransportConfig::Unix(PathBuf::from(endpoint.trim_start_matches("unix://")))
        } else if endpoint.starts_with("tcp://") {
            AbciTransportConfig::Tcp(endpoint.trim_start_matches("tcp://").to_string())
        } else {
            AbciTransportConfig::Grpc(endpoint.to_string())
        }
    }
}
