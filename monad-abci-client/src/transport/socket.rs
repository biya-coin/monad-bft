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

//! Unix socket-based ABCI client implementation.
//!
//! This implementation provides ABCI client functionality over Unix sockets,
//! supporting both sync and async patterns via blocking_on_async.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::collections::HashMap;
use std::sync::Mutex;

use monad_cometbft_proto::cometbft::abci::v1::{
    CheckTxRequest, CheckTxResponse, CommitRequest, CommitResponse, EchoRequest, EchoResponse,
    ExtendVoteRequest, ExtendVoteResponse, FinalizeBlockRequest, FinalizeBlockResponse,
    InfoRequest, InfoResponse, InitChainRequest, InitChainResponse, ProcessProposalRequest,
    ProcessProposalResponse, PrepareProposalRequest, PrepareProposalResponse, QueryRequest,
    QueryResponse, Request, Response, VerifyVoteExtensionRequest, VerifyVoteExtensionResponse,
    ApplySnapshotChunkRequest, ApplySnapshotChunkResponse, ListSnapshotsRequest,
    ListSnapshotsResponse, LoadSnapshotChunkRequest, LoadSnapshotChunkResponse,
    OfferSnapshotRequest, OfferSnapshotResponse, request, response,
};
use once_cell::sync::Lazy;
use prost::Message;

use crate::consensus::AbciClientConsensus;
use crate::error::{AbciClientError, Result};
use crate::mempool::AbciClientMempool;
use crate::query::AbciClientQuery;
use crate::snapshot::AbciClientSnapshot;

/// Unix socket-based ABCI client.
///
/// Maintains persistent connections to ABCI servers for protocol-level communication.
#[derive(Clone)]
pub struct SocketAbciClient {
    endpoint: String,
}

impl SocketAbciClient {
    /// Create a new socket ABCI client with the given endpoint.
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
        }
    }

    /// Send a request and receive a response over the socket.
    async fn socket_roundtrip(&self, request: Request) -> Result<Response> {
        let endpoint = self.endpoint.clone();
        tokio::task::spawn_blocking(move || Self::do_socket_roundtrip(&endpoint, request))
            .await
            .map_err(|e| AbciClientError::Transport(format!("spawn_blocking failed: {}", e)))?
    }

    /// Synchronous socket roundtrip (to be called from blocking context).
    fn do_socket_roundtrip(endpoint: &str, request: Request) -> Result<Response> {
        enum SocketVariant {
            Unix(UnixStream),
            Tcp(TcpStream),
        }

        impl Read for SocketVariant {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                match self {
                    SocketVariant::Unix(s) => s.read(buf),
                    SocketVariant::Tcp(s) => s.read(buf),
                }
            }
        }

        impl Write for SocketVariant {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                match self {
                    SocketVariant::Unix(s) => s.write(buf),
                    SocketVariant::Tcp(s) => s.write(buf),
                }
            }

            fn flush(&mut self) -> std::io::Result<()> {
                match self {
                    SocketVariant::Unix(s) => s.flush(),
                    SocketVariant::Tcp(s) => s.flush(),
                }
            }
        }

        static SOCKET_CLIENTS: Lazy<Mutex<HashMap<String, SocketVariant>>> =
            Lazy::new(|| Mutex::new(HashMap::new()));

        let mut clients = SOCKET_CLIENTS.lock().unwrap();

        if !clients.contains_key(endpoint) {
            let socket = if endpoint.starts_with("/") {
                SocketVariant::Unix(UnixStream::connect(endpoint)?)
            } else {
                SocketVariant::Tcp(TcpStream::connect(endpoint)?)
            };
            clients.insert(endpoint.to_string(), socket);
        }

        let socket = clients.get_mut(endpoint).unwrap();

        // Encode and send request
        let mut body = Vec::new();
        request.encode(&mut body)?;
        let prefix = encode_varint(body.len());
        socket.write_all(&prefix)?;
        socket.write_all(&body)?;

        // Send flush
        let flush_req = Request {
            value: Some(request::Value::Flush(Default::default())),
        };
        let mut flush_body = Vec::new();
        flush_req.encode(&mut flush_body)?;
        let flush_prefix = encode_varint(flush_body.len());
        socket.write_all(&flush_prefix)?;
        socket.write_all(&flush_body)?;
        socket.flush()?;

        // Read first response
        let len1 = decode_varint(socket)?;
        let mut body1 = vec![0u8; len1];
        socket.read_exact(&mut body1)?;
        let response1 = Response::decode(body1.as_slice())?;

        // Read flush response
        let len2 = decode_varint(socket)?;
        let mut body2 = vec![0u8; len2];
        socket.read_exact(&mut body2)?;
        let response2 = Response::decode(body2.as_slice())?;

        // Verify flush response
        match response2.value {
            Some(response::Value::Flush(_)) => Ok(response1),
            _ => Err(AbciClientError::Transport(
                "expected trailing flush response".to_owned(),
            )),
        }
    }
}

fn encode_varint(mut value: usize) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
    out
}

fn decode_varint(reader: &mut dyn Read) -> Result<usize> {
    let mut shift = 0usize;
    let mut result = 0usize;
    loop {
        let mut byte = [0u8; 1];
        reader.read_exact(&mut byte)?;
        let current = byte[0];
        result |= usize::from(current & 0x7f) << shift;
        if current & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift >= usize::BITS as usize {
            return Err(AbciClientError::Transport(
                "invalid varint length prefix".to_owned(),
            ));
        }
    }
}

#[async_trait::async_trait]
impl AbciClientConsensus for SocketAbciClient {
    async fn init_chain(&self, req: InitChainRequest) -> Result<InitChainResponse> {
        let resp = self
            .socket_roundtrip(Request {
                value: Some(request::Value::InitChain(req)),
            })
            .await?;
        match resp.value {
            Some(response::Value::InitChain(r)) => Ok(r),
            _ => Err(AbciClientError::Transport(
                "unexpected response type for InitChain".to_owned(),
            )),
        }
    }

    async fn prepare_proposal(
        &self,
        req: PrepareProposalRequest,
    ) -> Result<PrepareProposalResponse> {
        let resp = self
            .socket_roundtrip(Request {
                value: Some(request::Value::PrepareProposal(req)),
            })
            .await?;
        match resp.value {
            Some(response::Value::PrepareProposal(r)) => Ok(r),
            _ => Err(AbciClientError::Transport(
                "unexpected response type for PrepareProposal".to_owned(),
            )),
        }
    }

    async fn process_proposal(
        &self,
        req: ProcessProposalRequest,
    ) -> Result<ProcessProposalResponse> {
        let resp = self
            .socket_roundtrip(Request {
                value: Some(request::Value::ProcessProposal(req)),
            })
            .await?;
        match resp.value {
            Some(response::Value::ProcessProposal(r)) => Ok(r),
            _ => Err(AbciClientError::Transport(
                "unexpected response type for ProcessProposal".to_owned(),
            )),
        }
    }

    async fn extend_vote(&self, req: ExtendVoteRequest) -> Result<ExtendVoteResponse> {
        let resp = self
            .socket_roundtrip(Request {
                value: Some(request::Value::ExtendVote(req)),
            })
            .await?;
        match resp.value {
            Some(response::Value::ExtendVote(r)) => Ok(r),
            _ => Err(AbciClientError::Transport(
                "unexpected response type for ExtendVote".to_owned(),
            )),
        }
    }

    async fn verify_vote_extension(
        &self,
        req: VerifyVoteExtensionRequest,
    ) -> Result<VerifyVoteExtensionResponse> {
        let resp = self
            .socket_roundtrip(Request {
                value: Some(request::Value::VerifyVoteExtension(req)),
            })
            .await?;
        match resp.value {
            Some(response::Value::VerifyVoteExtension(r)) => Ok(r),
            _ => Err(AbciClientError::Transport(
                "unexpected response type for VerifyVoteExtension".to_owned(),
            )),
        }
    }

    async fn finalize_block(
        &self,
        req: FinalizeBlockRequest,
    ) -> Result<FinalizeBlockResponse> {
        let resp = self
            .socket_roundtrip(Request {
                value: Some(request::Value::FinalizeBlock(req)),
            })
            .await?;
        match resp.value {
            Some(response::Value::FinalizeBlock(r)) => Ok(r),
            _ => Err(AbciClientError::Transport(
                "unexpected response type for FinalizeBlock".to_owned(),
            )),
        }
    }

    async fn commit(&self, _req: CommitRequest) -> Result<CommitResponse> {
        let resp = self
            .socket_roundtrip(Request {
                value: Some(request::Value::Commit(Default::default())),
            })
            .await?;
        match resp.value {
            Some(response::Value::Commit(r)) => Ok(r),
            _ => Err(AbciClientError::Transport(
                "unexpected response type for Commit".to_owned(),
            )),
        }
    }
}

#[async_trait::async_trait]
impl AbciClientMempool for SocketAbciClient {
    async fn check_tx(&self, req: CheckTxRequest) -> Result<CheckTxResponse> {
        let resp = self
            .socket_roundtrip(Request {
                value: Some(request::Value::CheckTx(req)),
            })
            .await?;
        match resp.value {
            Some(response::Value::CheckTx(r)) => Ok(r),
            _ => Err(AbciClientError::Transport(
                "unexpected response type for CheckTx".to_owned(),
            )),
        }
    }

    async fn flush(&self) -> Result<()> {
        let resp = self
            .socket_roundtrip(Request {
                value: Some(request::Value::Flush(Default::default())),
            })
            .await?;
        match resp.value {
            Some(response::Value::Flush(_)) => Ok(()),
            _ => Err(AbciClientError::Transport(
                "unexpected response type for Flush".to_owned(),
            )),
        }
    }
}

#[async_trait::async_trait]
impl AbciClientQuery for SocketAbciClient {
    async fn echo(&self, req: EchoRequest) -> Result<EchoResponse> {
        let resp = self
            .socket_roundtrip(Request {
                value: Some(request::Value::Echo(req)),
            })
            .await?;
        match resp.value {
            Some(response::Value::Echo(r)) => Ok(r),
            _ => Err(AbciClientError::Transport(
                "unexpected response type for Echo".to_owned(),
            )),
        }
    }

    async fn info(&self, req: InfoRequest) -> Result<InfoResponse> {
        let resp = self
            .socket_roundtrip(Request {
                value: Some(request::Value::Info(req)),
            })
            .await?;
        match resp.value {
            Some(response::Value::Info(r)) => Ok(r),
            _ => Err(AbciClientError::Transport(
                "unexpected response type for Info".to_owned(),
            )),
        }
    }

    async fn query(&self, req: QueryRequest) -> Result<QueryResponse> {
        let resp = self
            .socket_roundtrip(Request {
                value: Some(request::Value::Query(req)),
            })
            .await?;
        match resp.value {
            Some(response::Value::Query(r)) => Ok(r),
            _ => Err(AbciClientError::Transport(
                "unexpected response type for Query".to_owned(),
            )),
        }
    }
}

#[async_trait::async_trait]
impl AbciClientSnapshot for SocketAbciClient {
    async fn list_snapshots(
        &self,
        req: ListSnapshotsRequest,
    ) -> Result<ListSnapshotsResponse> {
        let resp = self
            .socket_roundtrip(Request {
                value: Some(request::Value::ListSnapshots(req)),
            })
            .await?;
        match resp.value {
            Some(response::Value::ListSnapshots(r)) => Ok(r),
            _ => Err(AbciClientError::Transport(
                "unexpected response type for ListSnapshots".to_owned(),
            )),
        }
    }

    async fn offer_snapshot(
        &self,
        req: OfferSnapshotRequest,
    ) -> Result<OfferSnapshotResponse> {
        let resp = self
            .socket_roundtrip(Request {
                value: Some(request::Value::OfferSnapshot(req)),
            })
            .await?;
        match resp.value {
            Some(response::Value::OfferSnapshot(r)) => Ok(r),
            _ => Err(AbciClientError::Transport(
                "unexpected response type for OfferSnapshot".to_owned(),
            )),
        }
    }

    async fn load_snapshot_chunk(
        &self,
        req: LoadSnapshotChunkRequest,
    ) -> Result<LoadSnapshotChunkResponse> {
        let resp = self
            .socket_roundtrip(Request {
                value: Some(request::Value::LoadSnapshotChunk(req)),
            })
            .await?;
        match resp.value {
            Some(response::Value::LoadSnapshotChunk(r)) => Ok(r),
            _ => Err(AbciClientError::Transport(
                "unexpected response type for LoadSnapshotChunk".to_owned(),
            )),
        }
    }

    async fn apply_snapshot_chunk(
        &self,
        req: ApplySnapshotChunkRequest,
    ) -> Result<ApplySnapshotChunkResponse> {
        let resp = self
            .socket_roundtrip(Request {
                value: Some(request::Value::ApplySnapshotChunk(req)),
            })
            .await?;
        match resp.value {
            Some(response::Value::ApplySnapshotChunk(r)) => Ok(r),
            _ => Err(AbciClientError::Transport(
                "unexpected response type for ApplySnapshotChunk".to_owned(),
            )),
        }
    }
}
