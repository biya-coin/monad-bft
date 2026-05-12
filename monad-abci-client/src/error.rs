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

use thiserror::Error;

/// Error types for ABCI client operations.
#[derive(Debug, Error)]
pub enum AbciClientError {
    #[error("invalid ABCI endpoint: {0}")]
    InvalidEndpoint(String),

    #[error("transport error: {0}")]
    Transport(String),

    #[error("grpc status: {0}")]
    GrpcStatus(String),

    #[error("encode error: {0}")]
    Encode(#[from] prost::EncodeError),

    #[error("decode error: {0}")]
    Decode(#[from] prost::DecodeError),

    #[error("proposal rejected by ABCI application")]
    ProposalRejected,

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("timeout")]
    Timeout,

    #[error("connection closed")]
    ConnectionClosed,
}

pub type Result<T> = std::result::Result<T, AbciClientError>;
