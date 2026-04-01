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

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    time::Duration,
};

use alloy_rlp::{Decodable, Encodable, RlpDecodable, RlpEncodable, encode_list};
use arrayvec::ArrayVec;
use message::{PeerLookupRequest, PeerLookupResponse, Ping, Pong};
use monad_crypto::{
    certificate_signature::{
        CertificateSignature, CertificateSignaturePubKey, CertificateSignatureRecoverable,
    },
    signing_domain,
};
use monad_executor::ExecutorMetrics;
use monad_executor_glue::PeerEntry;
use monad_node_config::NodeBootstrapPeerConfig;
use monad_types::{Epoch, NodeId, Round};
use tracing::debug;

pub mod discovery;
pub mod driver;
pub mod ipv4_validation;
pub mod message;
pub mod mock;

pub use message::PeerDiscoveryMessage;

#[derive(Debug, Clone)]
pub struct PeerSource<PK: monad_crypto::certificate_signature::PubKey> {
    pub id: NodeId<PK>,
    pub addr: SocketAddr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PortTag {
    TCP = 0,
    UDP = 1,
    AuthenticatedUDP = 2,
    DirectUDP = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct Port {
    pub tag: u8,
    pub port: u16,
}

impl Port {
    pub fn new(tag: PortTag, port: u16) -> Self {
        Self {
            tag: tag as u8,
            port,
        }
    }

    pub fn tag_enum(&self) -> Option<PortTag> {
        match self.tag {
            0 => Some(PortTag::TCP),
            1 => Some(PortTag::UDP),
            2 => Some(PortTag::AuthenticatedUDP),
            3 => Some(PortTag::DirectUDP),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PortList<const N: usize>(ArrayVec<Port, N>);

impl<const N: usize> Encodable for PortList<N> {
    fn length(&self) -> usize {
        alloy_rlp::list_length(&self.0)
    }

    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        alloy_rlp::encode_list(&self.0, out)
    }
}

impl<const N: usize> Decodable for PortList<N> {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let payload = &mut alloy_rlp::Header::decode_bytes(buf, true)?;
        let mut vec = ArrayVec::new();
        while !payload.is_empty() {
            let port = Port::decode(payload)?;
            if vec.try_push(port).is_err() {
                return Err(alloy_rlp::Error::Custom("too many ports"));
            }
        }
        Ok(PortList(vec))
    }
}

impl<const N: usize> std::ops::Deref for PortList<N> {
    type Target = ArrayVec<Port, N>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<const N: usize> std::ops::DerefMut for PortList<N> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<const N: usize> From<ArrayVec<Port, N>> for PortList<N> {
    fn from(vec: ArrayVec<Port, N>) -> Self {
        Self(vec)
    }
}

impl<const N: usize> AsRef<[Port]> for PortList<N> {
    fn as_ref(&self) -> &[Port] {
        &self.0
    }
}

impl<const N: usize> PortList<N> {
    fn port_by_tag(&self, tag: PortTag) -> Option<u16> {
        self.0
            .iter()
            .find(|p| p.tag_enum() == Some(tag))
            .map(|p| p.port)
    }

    fn tcp_port(&self) -> Option<u16> {
        self.port_by_tag(PortTag::TCP)
    }

    fn udp_port(&self) -> Option<u16> {
        self.port_by_tag(PortTag::UDP)
    }

    fn authenticated_udp_port(&self) -> Option<u16> {
        self.port_by_tag(PortTag::AuthenticatedUDP)
    }

    fn direct_udp_port(&self) -> Option<u16> {
        self.port_by_tag(PortTag::DirectUDP)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WireNameRecordV2 {
    pub ip: Ipv4Addr,
    pub ports: PortList<8>,
    pub capabilities: u64,
    pub seq: u64,
}

impl Encodable for WireNameRecordV2 {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        let enc: [&dyn Encodable; 4] = [
            &self.ip.octets(),
            &self.ports,
            &self.capabilities,
            &self.seq,
        ];
        encode_list::<_, dyn Encodable>(&enc, out);
    }
}

impl Decodable for WireNameRecordV2 {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let buf = &mut alloy_rlp::Header::decode_bytes(buf, true)?;

        let Ok(ip_bytes) = <[u8; 4]>::decode(buf) else {
            return Err(alloy_rlp::Error::Custom("Invalid IPv4 address"));
        };
        let ip = Ipv4Addr::from(ip_bytes);
        let ports = PortList::decode(buf)?;
        let capabilities = u64::decode(buf)?;
        let seq = u64::decode(buf)?;

        if !buf.is_empty() {
            return Err(alloy_rlp::Error::UnexpectedLength);
        }
        Ok(Self {
            ip,
            ports,
            capabilities,
            seq,
        })
    }
}

impl WireNameRecordV2 {
    fn decode_to_name_record(buf: &mut &[u8]) -> alloy_rlp::Result<NameRecord> {
        let wire = Self::decode(buf)?;

        let mut seen_tags = HashSet::new();
        for port in wire.ports.iter() {
            if !seen_tags.insert(port.tag) {
                return Err(alloy_rlp::Error::Custom("duplicate port tag"));
            }

            if port.tag_enum().is_none() {
                debug!(
                    tag = port.tag,
                    port = port.port,
                    "unknown port tag in name record"
                );
            }
        }

        if wire.ports.tcp_port().is_none() {
            return Err(alloy_rlp::Error::Custom("Missing TCP port"));
        }
        if wire.ports.authenticated_udp_port().is_none() {
            return Err(alloy_rlp::Error::Custom("Missing authenticated UDP port"));
        }

        Ok(NameRecord { record: wire })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameRecord {
    record: WireNameRecordV2,
}

impl NameRecord {
    pub fn new(ip: Ipv4Addr, port: u16, seq: u64) -> Self {
        Self::new_with_ports(ip, port, Some(port), port, None, seq)
    }

    pub fn new_with_authentication(
        ip: Ipv4Addr,
        tcp_port: u16,
        udp_port: u16,
        authenticated_udp_port: u16,
        seq: u64,
    ) -> Self {
        Self::new_with_ports(
            ip,
            tcp_port,
            Some(udp_port),
            authenticated_udp_port,
            None,
            seq,
        )
    }

    pub fn new_with_ports(
        ip: Ipv4Addr,
        tcp_port: u16,
        udp_port: Option<u16>,
        authenticated_udp_port: u16,
        direct_udp_port: Option<u16>,
        seq: u64,
    ) -> Self {
        let mut ports_vec = ArrayVec::new();
        ports_vec.push(Port::new(PortTag::TCP, tcp_port));
        if let Some(udp_port) = udp_port {
            ports_vec.push(Port::new(PortTag::UDP, udp_port));
        }
        ports_vec.push(Port::new(PortTag::AuthenticatedUDP, authenticated_udp_port));
        if let Some(direct_udp_port) = direct_udp_port {
            ports_vec.push(Port::new(PortTag::DirectUDP, direct_udp_port));
        }
        let wire = WireNameRecordV2 {
            ip,
            ports: PortList(ports_vec),
            capabilities: 0,
            seq,
        };
        Self { record: wire }
    }

    pub fn ip(&self) -> Ipv4Addr {
        self.record.ip
    }

    pub fn capabilities(&self) -> u64 {
        self.record.capabilities
    }

    pub fn seq(&self) -> u64 {
        self.record.seq
    }

    pub fn tcp_port(&self) -> u16 {
        self.record
            .ports
            .tcp_port()
            .expect("name record must have TCP port")
    }

    pub fn udp_port(&self) -> Option<u16> {
        self.record.ports.udp_port()
    }

    pub fn tcp_socket(&self) -> SocketAddrV4 {
        SocketAddrV4::new(self.ip(), self.tcp_port())
    }

    pub fn udp_socket(&self) -> Option<SocketAddrV4> {
        self.udp_port()
            .map(|port| SocketAddrV4::new(self.ip(), port))
    }

    pub fn authenticated_udp_port(&self) -> u16 {
        self.record
            .ports
            .authenticated_udp_port()
            .expect("name record must have authenticated UDP port")
    }

    pub fn authenticated_udp_socket(&self) -> SocketAddrV4 {
        SocketAddrV4::new(self.ip(), self.authenticated_udp_port())
    }

    pub fn direct_udp_port(&self) -> Option<u16> {
        self.record.ports.direct_udp_port()
    }

    pub fn direct_udp_socket(&self) -> Option<SocketAddrV4> {
        self.direct_udp_port()
            .map(|port| SocketAddrV4::new(self.ip(), port))
    }

    pub fn check_capability(&self, capability: Capability) -> bool {
        (self.capabilities() & (1u64 << (capability as u8))) != 0
    }

    pub fn set_capability(&mut self, capability: Capability) {
        self.record.capabilities |= 1u64 << (capability as u8);
    }
}

impl Encodable for NameRecord {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        self.record.encode(out)
    }
}

impl Decodable for NameRecord {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        WireNameRecordV2::decode_to_name_record(buf)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {}

#[derive(Debug, Clone, PartialEq, RlpEncodable, RlpDecodable, Eq)]
pub struct MonadNameRecord<ST: CertificateSignatureRecoverable> {
    pub name_record: NameRecord,
    pub signature: ST,
}

impl<ST: CertificateSignatureRecoverable> MonadNameRecord<ST> {
    pub fn new(name_record: NameRecord, key: &ST::KeyPairType) -> Self {
        let mut encoded = Vec::new();
        name_record.encode(&mut encoded);
        let signature = ST::sign::<signing_domain::NameRecord>(&encoded, key);
        Self {
            name_record,
            signature,
        }
    }

    pub fn recover_pubkey(
        &self,
    ) -> Result<NodeId<CertificateSignaturePubKey<ST>>, <ST as CertificateSignature>::Error> {
        let mut encoded = Vec::new();
        self.name_record.encode(&mut encoded);
        let pubkey = self
            .signature
            .recover_pubkey::<signing_domain::NameRecord>(&encoded)?;
        Ok(NodeId::new(pubkey))
    }

    pub fn udp_address(&self) -> Option<SocketAddrV4> {
        self.name_record.udp_socket()
    }

    pub fn authenticated_udp_address(&self) -> SocketAddrV4 {
        self.name_record.authenticated_udp_socket()
    }

    pub fn direct_udp_address(&self) -> Option<SocketAddrV4> {
        self.name_record.direct_udp_socket()
    }

    pub fn all_udp_sockets(&self) -> impl Iterator<Item = SocketAddrV4> + '_ {
        let mut sockets = ArrayVec::<SocketAddrV4, 3>::new();
        for socket in [
            self.udp_address(),
            Some(self.authenticated_udp_address()),
            self.direct_udp_address(),
        ]
        .into_iter()
        .flatten()
        {
            if !sockets.contains(&socket) {
                sockets.push(socket);
            }
        }
        sockets.into_iter()
    }

    pub fn seq(&self) -> u64 {
        self.name_record.seq()
    }

    pub fn with_pubkey(
        &self,
        pubkey: CertificateSignaturePubKey<ST>,
    ) -> MonadNameRecordWithPubkey<'_, ST> {
        MonadNameRecordWithPubkey {
            record: self,
            pubkey,
        }
    }
}

#[derive(Debug)]
pub enum PeerEntryConversionError<E> {
    InvalidAddress,
    MissingAuthenticatedUdpPort,
    InvalidSignature(E),
}

impl<ST: CertificateSignatureRecoverable> TryFrom<&PeerEntry<ST>> for MonadNameRecord<ST> {
    type Error = PeerEntryConversionError<<ST as CertificateSignature>::Error>;

    fn try_from(peer: &PeerEntry<ST>) -> Result<Self, Self::Error> {
        let Some(authenticated_udp_port) = peer.auth_port else {
            return Err(PeerEntryConversionError::MissingAuthenticatedUdpPort);
        };

        let mut candidates = ArrayVec::<NameRecord, 2>::new();
        if let Ok(address) = peer.addr.parse::<SocketAddrV4>() {
            candidates.push(NameRecord::new_with_ports(
                *address.ip(),
                address.port(),
                None,
                authenticated_udp_port,
                peer.direct_udp_port,
                peer.record_seq_num,
            ));
            candidates.push(NameRecord::new_with_ports(
                *address.ip(),
                address.port(),
                Some(address.port()),
                authenticated_udp_port,
                peer.direct_udp_port,
                peer.record_seq_num,
            ));
        } else if let Ok(ip) = peer.addr.parse::<Ipv4Addr>() {
            candidates.push(NameRecord::new_with_ports(
                ip,
                authenticated_udp_port,
                None,
                authenticated_udp_port,
                peer.direct_udp_port,
                peer.record_seq_num,
            ));
        } else {
            return Err(PeerEntryConversionError::InvalidAddress);
        }

        let mut last_error = None;
        for candidate in candidates {
            let mut encoded = Vec::new();
            candidate.encode(&mut encoded);
            match peer
                .signature
                .verify::<signing_domain::NameRecord>(&encoded, &peer.pubkey)
            {
                Ok(()) => {
                    return Ok(MonadNameRecord {
                        name_record: candidate,
                        signature: peer.signature,
                    });
                }
                Err(error) => last_error = Some(error),
            }
        }

        Err(PeerEntryConversionError::InvalidSignature(
            last_error.expect("at least one candidate name record is required"),
        ))
    }
}

impl<ST: CertificateSignatureRecoverable> TryFrom<&MonadNameRecord<ST>> for PeerEntry<ST> {
    type Error = <ST as CertificateSignature>::Error;

    fn try_from(record: &MonadNameRecord<ST>) -> Result<Self, Self::Error> {
        let pubkey = record.recover_pubkey()?.pubkey();
        Ok(record.with_pubkey(pubkey).into())
    }
}

impl<ST: CertificateSignatureRecoverable> From<MonadNameRecordWithPubkey<'_, ST>>
    for NodeBootstrapPeerConfig<ST>
{
    fn from(record_with_pubkey: MonadNameRecordWithPubkey<'_, ST>) -> Self {
        let peer_entry: PeerEntry<_> = record_with_pubkey.into();
        NodeBootstrapPeerConfig {
            address: peer_entry.addr,
            record_seq_num: peer_entry.record_seq_num,
            secp256k1_pubkey: peer_entry.pubkey,
            name_record_sig: peer_entry.signature,
            auth_port: peer_entry.auth_port,
            direct_udp_port: peer_entry.direct_udp_port,
        }
    }
}

#[derive(Debug)]
pub enum PeerConfigConversionError {
    InvalidAddress,
    InvalidSignature,
    MissingAuthenticatedUdpPort,
}

impl<ST: CertificateSignatureRecoverable> TryFrom<&NodeBootstrapPeerConfig<ST>>
    for MonadNameRecord<ST>
{
    type Error = PeerConfigConversionError;

    fn try_from(peer_config: &NodeBootstrapPeerConfig<ST>) -> Result<Self, Self::Error> {
        let peer_entry = PeerEntry {
            pubkey: peer_config.secp256k1_pubkey,
            addr: peer_config.address.clone(),
            signature: peer_config.name_record_sig,
            record_seq_num: peer_config.record_seq_num,
            auth_port: peer_config.auth_port,
            direct_udp_port: peer_config.direct_udp_port,
        };

        MonadNameRecord::try_from(&peer_entry).map_err(|error| match error {
            PeerEntryConversionError::InvalidAddress => PeerConfigConversionError::InvalidAddress,
            PeerEntryConversionError::MissingAuthenticatedUdpPort => {
                PeerConfigConversionError::MissingAuthenticatedUdpPort
            }
            PeerEntryConversionError::InvalidSignature(_) => {
                PeerConfigConversionError::InvalidSignature
            }
        })
    }
}

pub struct MonadNameRecordWithPubkey<'a, ST: CertificateSignatureRecoverable> {
    record: &'a MonadNameRecord<ST>,
    pubkey: CertificateSignaturePubKey<ST>,
}

impl<ST: CertificateSignatureRecoverable> From<MonadNameRecordWithPubkey<'_, ST>>
    for PeerEntry<ST>
{
    fn from(record_with_pubkey: MonadNameRecordWithPubkey<'_, ST>) -> Self {
        let addr = if record_with_pubkey.record.name_record.udp_port().is_none()
            && record_with_pubkey.record.name_record.tcp_port()
                == record_with_pubkey
                    .record
                    .name_record
                    .authenticated_udp_port()
        {
            record_with_pubkey.record.name_record.ip().to_string()
        } else {
            record_with_pubkey
                .record
                .name_record
                .tcp_socket()
                .to_string()
        };

        PeerEntry {
            pubkey: record_with_pubkey.pubkey,
            addr,
            signature: record_with_pubkey.record.signature,
            record_seq_num: record_with_pubkey.record.name_record.seq(),
            auth_port: Some(
                record_with_pubkey
                    .record
                    .name_record
                    .authenticated_udp_port(),
            ),
            direct_udp_port: record_with_pubkey.record.name_record.direct_udp_port(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum PeerDiscoveryEvent<ST: CertificateSignatureRecoverable> {
    SendPing {
        to: NodeId<CertificateSignaturePubKey<ST>>,
        name_record: NameRecord,
        ping: Ping<ST>,
    },
    PingRequest {
        from: PeerSource<CertificateSignaturePubKey<ST>>,
        ping: Ping<ST>,
    },
    PongResponse {
        from: PeerSource<CertificateSignaturePubKey<ST>>,
        pong: Pong,
    },
    PingTimeout {
        to: NodeId<CertificateSignaturePubKey<ST>>,
        ping_id: u32,
    },
    SendPeerLookup {
        to: NodeId<CertificateSignaturePubKey<ST>>,
        target: NodeId<CertificateSignaturePubKey<ST>>,
        open_discovery: bool,
    },
    PeerLookupRequest {
        from: PeerSource<CertificateSignaturePubKey<ST>>,
        request: PeerLookupRequest<ST>,
    },
    PeerLookupResponse {
        from: PeerSource<CertificateSignaturePubKey<ST>>,
        response: PeerLookupResponse<ST>,
    },
    PeerLookupTimeout {
        to: NodeId<CertificateSignaturePubKey<ST>>,
        target: NodeId<CertificateSignaturePubKey<ST>>,
        lookup_id: u32,
    },
    SendFullNodeRaptorcastRequest {
        to: NodeId<CertificateSignaturePubKey<ST>>,
    },
    FullNodeRaptorcastRequest {
        from: PeerSource<CertificateSignaturePubKey<ST>>,
    },
    FullNodeRaptorcastResponse {
        from: PeerSource<CertificateSignaturePubKey<ST>>,
    },
    UpdateCurrentRound {
        round: Round,
        epoch: Epoch,
    },
    UpdateValidatorSet {
        epoch: Epoch,
        validators: BTreeSet<NodeId<CertificateSignaturePubKey<ST>>>,
    },
    UpdatePeers {
        peers: Vec<PeerEntry<ST>>,
    },
    UpdatePinnedNodes {
        dedicated_full_nodes: BTreeSet<NodeId<CertificateSignaturePubKey<ST>>>,
        prioritized_full_nodes: BTreeSet<NodeId<CertificateSignaturePubKey<ST>>>,
    },
    UpdateConfirmGroup {
        end_round: Round,
        peers: BTreeSet<NodeId<CertificateSignaturePubKey<ST>>>,
    },
    Refresh,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TimerKind {
    SendPing,
    PingTimeout,
    RetryPeerLookup { lookup_id: u32 },
    Refresh,
    FullNodeRaptorcastRequest,
}

#[derive(Debug, Clone)]
pub enum PeerDiscoveryTimerCommand<E, ST: CertificateSignatureRecoverable> {
    Schedule {
        node_id: NodeId<CertificateSignaturePubKey<ST>>,
        timer_kind: TimerKind,
        duration: Duration,
        on_timeout: E,
    },
    ScheduleReset {
        node_id: NodeId<CertificateSignaturePubKey<ST>>,
        timer_kind: TimerKind,
    },
}

#[derive(Debug, Clone)]
pub struct PeerDiscoveryMetricsCommand(ExecutorMetrics);

#[derive(Debug, Clone)]
pub enum PeerDiscoveryCommand<ST: CertificateSignatureRecoverable> {
    RouterCommand {
        target: NodeId<CertificateSignaturePubKey<ST>>,
        message: PeerDiscoveryMessage<ST>,
    },
    PingPongCommand {
        target: NodeId<CertificateSignaturePubKey<ST>>,
        name_record: NameRecord,
        message: PeerDiscoveryMessage<ST>,
    },
    TimerCommand(PeerDiscoveryTimerCommand<PeerDiscoveryEvent<ST>, ST>),
    MetricsCommand(PeerDiscoveryMetricsCommand),
}

pub trait PeerDiscoveryAlgo {
    type SignatureType: CertificateSignatureRecoverable;

    fn send_ping(
        &mut self,
        target: NodeId<CertificateSignaturePubKey<Self::SignatureType>>,
        name_record: NameRecord,
        ping: Ping<Self::SignatureType>,
    ) -> Vec<PeerDiscoveryCommand<Self::SignatureType>>;

    fn handle_ping(
        &mut self,
        from: PeerSource<CertificateSignaturePubKey<Self::SignatureType>>,
        ping: Ping<Self::SignatureType>,
    ) -> Vec<PeerDiscoveryCommand<Self::SignatureType>>;

    fn handle_pong(
        &mut self,
        from: PeerSource<CertificateSignaturePubKey<Self::SignatureType>>,
        pong: Pong,
    ) -> Vec<PeerDiscoveryCommand<Self::SignatureType>>;

    fn handle_ping_timeout(
        &mut self,
        to: NodeId<CertificateSignaturePubKey<Self::SignatureType>>,
        ping_id: u32,
    ) -> Vec<PeerDiscoveryCommand<Self::SignatureType>>;

    fn send_peer_lookup_request(
        &mut self,
        to: NodeId<CertificateSignaturePubKey<Self::SignatureType>>,
        target: NodeId<CertificateSignaturePubKey<Self::SignatureType>>,
        open_discovery: bool,
    ) -> Vec<PeerDiscoveryCommand<Self::SignatureType>>;

    fn handle_peer_lookup_request(
        &mut self,
        from: PeerSource<CertificateSignaturePubKey<Self::SignatureType>>,
        request: PeerLookupRequest<Self::SignatureType>,
    ) -> Vec<PeerDiscoveryCommand<Self::SignatureType>>;

    fn handle_peer_lookup_response(
        &mut self,
        from: PeerSource<CertificateSignaturePubKey<Self::SignatureType>>,
        response: PeerLookupResponse<Self::SignatureType>,
    ) -> Vec<PeerDiscoveryCommand<Self::SignatureType>>;

    fn handle_peer_lookup_timeout(
        &mut self,
        to: NodeId<CertificateSignaturePubKey<Self::SignatureType>>,
        target: NodeId<CertificateSignaturePubKey<Self::SignatureType>>,
        lookup_id: u32,
    ) -> Vec<PeerDiscoveryCommand<Self::SignatureType>>;

    fn send_full_node_raptorcast_request(
        &mut self,
        to: NodeId<CertificateSignaturePubKey<Self::SignatureType>>,
    ) -> Vec<PeerDiscoveryCommand<Self::SignatureType>>;

    fn handle_full_node_raptorcast_request(
        &mut self,
        from: NodeId<CertificateSignaturePubKey<Self::SignatureType>>,
    ) -> Vec<PeerDiscoveryCommand<Self::SignatureType>>;

    fn handle_full_node_raptorcast_response(
        &mut self,
        from: NodeId<CertificateSignaturePubKey<Self::SignatureType>>,
    ) -> Vec<PeerDiscoveryCommand<Self::SignatureType>>;

    fn refresh(&mut self) -> Vec<PeerDiscoveryCommand<Self::SignatureType>>;

    fn update_current_round(
        &mut self,
        round: Round,
        epoch: Epoch,
    ) -> Vec<PeerDiscoveryCommand<Self::SignatureType>>;

    fn update_validator_set(
        &mut self,
        epoch: Epoch,
        validators: BTreeSet<NodeId<CertificateSignaturePubKey<Self::SignatureType>>>,
    ) -> Vec<PeerDiscoveryCommand<Self::SignatureType>>;

    fn update_peers(
        &mut self,
        peers: Vec<PeerEntry<Self::SignatureType>>,
    ) -> Vec<PeerDiscoveryCommand<Self::SignatureType>>;

    fn update_pinned_nodes(
        &mut self,
        dedicated_full_nodes: BTreeSet<NodeId<CertificateSignaturePubKey<Self::SignatureType>>>,
        prioritized_full_nodes: BTreeSet<NodeId<CertificateSignaturePubKey<Self::SignatureType>>>,
    ) -> Vec<PeerDiscoveryCommand<Self::SignatureType>>;

    fn update_peer_participation(
        &mut self,
        round: Round,
        peers: BTreeSet<NodeId<CertificateSignaturePubKey<Self::SignatureType>>>,
    ) -> Vec<PeerDiscoveryCommand<Self::SignatureType>>;

    fn metrics(&self) -> &ExecutorMetrics;

    fn get_pending_addr_by_id(
        &self,
        id: &NodeId<CertificateSignaturePubKey<Self::SignatureType>>,
    ) -> Option<SocketAddrV4>;

    fn get_addr_by_id(
        &self,
        id: &NodeId<CertificateSignaturePubKey<Self::SignatureType>>,
    ) -> Option<SocketAddrV4>;

    fn get_known_addrs(
        &self,
    ) -> HashMap<NodeId<CertificateSignaturePubKey<Self::SignatureType>>, SocketAddrV4>;

    fn get_secondary_fullnodes(
        &self,
    ) -> Vec<NodeId<CertificateSignaturePubKey<Self::SignatureType>>>;

    fn get_name_records(
        &self,
    ) -> HashMap<
        NodeId<CertificateSignaturePubKey<Self::SignatureType>>,
        MonadNameRecord<Self::SignatureType>,
    >;

    fn get_name_record(
        &self,
        id: &NodeId<CertificateSignaturePubKey<Self::SignatureType>>,
    ) -> Option<&MonadNameRecord<Self::SignatureType>>;
}

pub trait PeerDiscoveryAlgoBuilder {
    type PeerDiscoveryAlgoType: PeerDiscoveryAlgo;

    fn build(
        self,
    ) -> (
        Self::PeerDiscoveryAlgoType,
        Vec<
            PeerDiscoveryCommand<<Self::PeerDiscoveryAlgoType as PeerDiscoveryAlgo>::SignatureType>,
        >,
    );
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use monad_secp::{KeyPair, SecpSignature};
    use rstest::*;

    use super::*;

    #[test]
    fn test_name_record_v4_rlp() {
        let name_record = NameRecord::new(Ipv4Addr::from_str("1.1.1.1").unwrap(), 8000, 2);

        let mut encoded = Vec::new();
        name_record.encode(&mut encoded);

        let result = NameRecord::decode(&mut encoded.as_slice());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), name_record);
    }

    #[test]
    fn test_name_record_v2() {
        let name_record = NameRecord::new_with_ports(
            Ipv4Addr::from_str("1.1.1.1").unwrap(),
            8000,
            Some(8001),
            8001,
            None,
            1,
        );

        assert_eq!(
            name_record.tcp_socket(),
            SocketAddrV4::from_str("1.1.1.1:8000").unwrap()
        );
        assert_eq!(
            name_record.udp_socket(),
            Some(SocketAddrV4::from_str("1.1.1.1:8001").unwrap())
        );
    }

    #[test]
    fn test_name_record_duplicate_port() {
        let mut ports_vec = ArrayVec::new();
        ports_vec.push(Port::new(PortTag::TCP, 8000));
        ports_vec.push(Port::new(PortTag::UDP, 8001));
        ports_vec.push(Port::new(PortTag::TCP, 8002));

        let wire = WireNameRecordV2 {
            ip: Ipv4Addr::from_str("1.1.1.1").unwrap(),
            ports: PortList(ports_vec),
            capabilities: 0,
            seq: 1,
        };

        let mut encoded = Vec::new();
        wire.encode(&mut encoded);

        let decoded = NameRecord::decode(&mut encoded.as_slice());
        assert!(decoded.is_err());
    }

    #[test]
    fn test_name_record_missing_tcp_port() {
        let mut ports_vec = ArrayVec::new();
        ports_vec.push(Port::new(PortTag::UDP, 8001));

        let wire = WireNameRecordV2 {
            ip: Ipv4Addr::from_str("1.1.1.1").unwrap(),
            ports: PortList(ports_vec),
            capabilities: 0,
            seq: 1,
        };

        let mut encoded = Vec::new();
        wire.encode(&mut encoded);

        let decoded = NameRecord::decode(&mut encoded.as_slice());
        assert!(decoded.is_err());
    }

    #[test]
    fn test_name_record_with_unknown_ports_and_capabilities() {
        let mut ports_vec = ArrayVec::new();
        ports_vec.push(Port::new(PortTag::TCP, 9000));
        ports_vec.push(Port::new(PortTag::UDP, 9001));
        ports_vec.push(Port { tag: 2, port: 9002 });
        ports_vec.push(Port { tag: 5, port: 9005 });

        let wire = WireNameRecordV2 {
            ip: Ipv4Addr::from_str("10.0.0.1").unwrap(),
            ports: PortList(ports_vec),
            capabilities: 7,
            seq: 100,
        };

        let mut wire_encoded = Vec::new();
        wire.encode(&mut wire_encoded);

        let decoded = NameRecord::decode(&mut wire_encoded.as_slice()).unwrap();

        assert_eq!(decoded.ip(), Ipv4Addr::from_str("10.0.0.1").unwrap());
        assert_eq!(decoded.tcp_port(), 9000);
        assert_eq!(decoded.udp_port(), Some(9001));
        assert_eq!(decoded.capabilities(), 7);
        assert_eq!(decoded.seq(), 100);

        let mut reencoded = Vec::new();
        decoded.encode(&mut reencoded);
        assert_eq!(wire_encoded, reencoded);

        let keypair = KeyPair::from_ikm(b"test keypair for signature veri").unwrap();
        let signature = SecpSignature::sign::<signing_domain::NameRecord>(&wire_encoded, &keypair);

        let signed_record = MonadNameRecord::<SecpSignature> {
            name_record: decoded,
            signature,
        };

        let recovered_node_id = signed_record.recover_pubkey().unwrap();
        let expected_node_id = NodeId::new(keypair.pubkey());

        assert_eq!(recovered_node_id, expected_node_id);
    }

    #[test]
    fn test_name_record_roundtrip() {
        let ip = Ipv4Addr::from_str("192.168.50.100").unwrap();
        let port = 8888u16;
        let seq = 42u64;

        let record = NameRecord::new(ip, port, seq);
        let mut encoded = Vec::new();
        record.encode(&mut encoded);

        insta::assert_debug_snapshot!("name_record_encoded", hex::encode(&encoded));

        let decoded = NameRecord::decode(&mut encoded.as_slice()).unwrap();

        assert_eq!(decoded.ip(), ip);
        assert_eq!(decoded.tcp_port(), port);
        assert_eq!(decoded.udp_port(), Some(port));
        assert_eq!(decoded.authenticated_udp_port(), port);
        assert_eq!(decoded.capabilities(), 0);
        assert_eq!(decoded.seq(), seq);

        let mut reencoded = Vec::new();
        decoded.encode(&mut reencoded);
        assert_eq!(encoded, reencoded);

        let keypair = KeyPair::from_ikm(b"test roundtrip").unwrap();
        let signature = SecpSignature::sign::<signing_domain::NameRecord>(&encoded, &keypair);

        let signed_record = MonadNameRecord::<SecpSignature> {
            name_record: decoded.clone(),
            signature,
        };

        let recovered_node_id = signed_record.recover_pubkey().unwrap();
        let expected_node_id = NodeId::new(keypair.pubkey());
        assert_eq!(recovered_node_id, expected_node_id);

        let mut signed_encoded = Vec::new();
        signed_record.encode(&mut signed_encoded);

        let decoded_signed =
            MonadNameRecord::<SecpSignature>::decode(&mut signed_encoded.as_slice()).unwrap();
        assert_eq!(decoded_signed.name_record.ip(), ip);
        assert_eq!(decoded_signed.name_record.tcp_port(), port);
        assert_eq!(decoded_signed.name_record.udp_port(), Some(port));
        assert_eq!(decoded_signed.name_record.seq(), seq);

        let recovered_from_decoded = decoded_signed.recover_pubkey().unwrap();
        assert_eq!(recovered_from_decoded, expected_node_id);
    }

    #[test]
    fn test_name_record_v2_roundtrip() {
        let ip = Ipv4Addr::from_str("192.168.50.100").unwrap();
        let tcp_port = 9000u16;
        let udp_port = 9001u16;
        let authenticated_udp_port = 9002u16;
        let capabilities = 0u64;
        let seq = 42u64;

        let v2_record = NameRecord::new_with_authentication(
            ip,
            tcp_port,
            udp_port,
            authenticated_udp_port,
            seq,
        );
        let mut v2_encoded = Vec::new();
        v2_record.encode(&mut v2_encoded);

        insta::assert_debug_snapshot!("v2_encoded", hex::encode(&v2_encoded));

        let decoded = NameRecord::decode(&mut v2_encoded.as_slice()).unwrap();

        assert_eq!(decoded.ip(), ip);
        assert_eq!(decoded.tcp_port(), tcp_port);
        assert_eq!(decoded.udp_port(), Some(udp_port));
        assert_eq!(decoded.authenticated_udp_port(), authenticated_udp_port);
        assert_eq!(decoded.capabilities(), capabilities);
        assert_eq!(decoded.seq(), seq);

        let mut reencoded = Vec::new();
        decoded.encode(&mut reencoded);
        assert_eq!(v2_encoded, reencoded);

        let keypair = KeyPair::from_ikm(b"test v2 roundtrip").unwrap();
        let signature = SecpSignature::sign::<signing_domain::NameRecord>(&v2_encoded, &keypair);

        let signed_record = MonadNameRecord::<SecpSignature> {
            name_record: decoded.clone(),
            signature,
        };

        let recovered_node_id = signed_record.recover_pubkey().unwrap();
        let expected_node_id = NodeId::new(keypair.pubkey());
        assert_eq!(recovered_node_id, expected_node_id);

        let mut signed_encoded = Vec::new();
        signed_record.encode(&mut signed_encoded);

        let decoded_signed =
            MonadNameRecord::<SecpSignature>::decode(&mut signed_encoded.as_slice()).unwrap();
        assert_eq!(decoded_signed.name_record.ip(), ip);
        assert_eq!(decoded_signed.name_record.tcp_port(), tcp_port);
        assert_eq!(decoded_signed.name_record.udp_port(), Some(udp_port));
        assert_eq!(decoded_signed.name_record.capabilities(), capabilities);
        assert_eq!(decoded_signed.name_record.seq(), seq);

        let recovered_from_decoded = decoded_signed.recover_pubkey().unwrap();
        assert_eq!(recovered_from_decoded, expected_node_id);
    }

    #[test]
    fn test_name_record_with_authentication() {
        let ip = Ipv4Addr::from_str("10.0.0.42").unwrap();
        let tcp_port = 9000u16;
        let udp_port = 9001u16;
        let authenticated_udp_port = 9002u16;
        let seq = 100u64;

        let auth_record = NameRecord::new_with_authentication(
            ip,
            tcp_port,
            udp_port,
            authenticated_udp_port,
            seq,
        );

        assert_eq!(auth_record.ip(), ip);
        assert_eq!(auth_record.tcp_port(), tcp_port);
        assert_eq!(auth_record.udp_port(), Some(udp_port));
        assert_eq!(auth_record.authenticated_udp_port(), authenticated_udp_port);
        assert_eq!(
            auth_record.authenticated_udp_socket(),
            SocketAddrV4::from_str("10.0.0.42:9002").unwrap()
        );
        assert_eq!(auth_record.capabilities(), 0);
        assert_eq!(auth_record.seq(), seq);

        let mut encoded = Vec::new();
        auth_record.encode(&mut encoded);

        insta::assert_debug_snapshot!("auth_encoded", hex::encode(&encoded));

        let decoded = NameRecord::decode(&mut encoded.as_slice()).unwrap();
        assert_eq!(decoded.ip(), ip);
        assert_eq!(decoded.tcp_port(), tcp_port);
        assert_eq!(decoded.udp_port(), Some(udp_port));
        assert_eq!(decoded.authenticated_udp_port(), authenticated_udp_port);
        assert_eq!(decoded.seq(), seq);

        let mut reencoded = Vec::new();
        decoded.encode(&mut reencoded);
        assert_eq!(encoded, reencoded);

        let keypair = KeyPair::from_ikm(b"test authenticated udp").unwrap();
        let signed_record = MonadNameRecord::<SecpSignature>::new(decoded, &keypair);

        assert_eq!(
            signed_record.authenticated_udp_address(),
            SocketAddrV4::from_str("10.0.0.42:9002").unwrap()
        );

        let recovered_node_id = signed_record.recover_pubkey().unwrap();
        let expected_node_id = NodeId::new(keypair.pubkey());
        assert_eq!(recovered_node_id, expected_node_id);
    }

    #[test]
    fn test_name_record_auth_only_roundtrip() {
        let ip = Ipv4Addr::from_str("10.0.0.52").unwrap();
        let tcp_port = 9050u16;
        let authenticated_udp_port = 9051u16;
        let seq = 110u64;

        let auth_only_record =
            NameRecord::new_with_ports(ip, tcp_port, None, authenticated_udp_port, None, seq);

        assert_eq!(auth_only_record.ip(), ip);
        assert_eq!(auth_only_record.tcp_port(), tcp_port);
        assert_eq!(auth_only_record.udp_port(), None);
        assert_eq!(
            auth_only_record.authenticated_udp_port(),
            authenticated_udp_port
        );
        assert_eq!(
            auth_only_record.authenticated_udp_socket(),
            SocketAddrV4::new(ip, authenticated_udp_port)
        );
        assert_eq!(auth_only_record.seq(), seq);

        let mut encoded = Vec::new();
        auth_only_record.encode(&mut encoded);

        let decoded = NameRecord::decode(&mut encoded.as_slice()).unwrap();
        assert_eq!(decoded, auth_only_record);

        let keypair = KeyPair::from_ikm(b"test auth only udp").unwrap();
        let signed_record = MonadNameRecord::<SecpSignature>::new(decoded, &keypair);
        assert_eq!(
            signed_record.all_udp_sockets().collect::<Vec<_>>(),
            vec![SocketAddrV4::new(ip, authenticated_udp_port)]
        );
        assert_eq!(
            signed_record.recover_pubkey().unwrap(),
            NodeId::new(keypair.pubkey())
        );
    }

    #[test]
    fn test_name_record_with_direct_udp_roundtrip() {
        let ip = Ipv4Addr::from_str("10.0.0.43").unwrap();
        let tcp_port = 9100u16;
        let udp_port = 9101u16;
        let authenticated_udp_port = 9102u16;
        let direct_udp_port = 9103u16;
        let seq = 101u64;

        let record = NameRecord::new_with_ports(
            ip,
            tcp_port,
            Some(udp_port),
            authenticated_udp_port,
            Some(direct_udp_port),
            seq,
        );

        assert_eq!(record.ip(), ip);
        assert_eq!(record.tcp_port(), tcp_port);
        assert_eq!(record.udp_port(), Some(udp_port));
        assert_eq!(record.authenticated_udp_port(), authenticated_udp_port);
        assert_eq!(record.direct_udp_port(), Some(direct_udp_port));
        assert_eq!(
            record.direct_udp_socket(),
            Some(SocketAddrV4::from_str("10.0.0.43:9103").unwrap())
        );
        assert_eq!(record.seq(), seq);

        let mut encoded = Vec::new();
        record.encode(&mut encoded);

        let decoded = NameRecord::decode(&mut encoded.as_slice()).unwrap();
        assert_eq!(decoded.ip(), ip);
        assert_eq!(decoded.tcp_port(), tcp_port);
        assert_eq!(decoded.udp_port(), Some(udp_port));
        assert_eq!(decoded.authenticated_udp_port(), authenticated_udp_port);
        assert_eq!(decoded.direct_udp_port(), Some(direct_udp_port));
        assert_eq!(decoded.seq(), seq);

        let mut reencoded = Vec::new();
        decoded.encode(&mut reencoded);
        assert_eq!(encoded, reencoded);
    }

    #[rstest]
    #[case::auth_and_direct(
        NameRecord::new_with_ports(
            Ipv4Addr::new(10, 0, 0, 44),
            9200,
            Some(9201),
            9202,
            Some(9203),
            102,
        ),
        vec![
            SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 44), 9201),
            SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 44), 9202),
            SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 44), 9203),
        ],
    )]
    #[case::direct_only_optional_middle_none(
        NameRecord::new_with_ports(
            Ipv4Addr::new(10, 0, 0, 45),
            9300,
            Some(9301),
            9301,
            Some(9303),
            103,
        ),
        vec![
            SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 45), 9301),
            SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 45), 9303),
        ],
    )]
    #[case::auth_only(
        NameRecord::new_with_ports(
            Ipv4Addr::new(10, 0, 0, 46),
            9400,
            Some(9401),
            9402,
            None,
            104,
        ),
        vec![
            SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 46), 9401),
            SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 46), 9402),
        ],
    )]
    #[case::auth_only_without_udp(
        NameRecord::new_with_ports(Ipv4Addr::new(10, 0, 0, 48), 9500, None, 9502, None, 106),
        vec![SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 48), 9502)],
    )]
    #[case::udp_only(
        NameRecord::new(Ipv4Addr::new(10, 0, 0, 47), 9501, 105),
        vec![SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 47), 9501)],
    )]
    fn test_monad_name_record_all_udp_sockets(
        #[case] record: NameRecord,
        #[case] expected_sockets: Vec<SocketAddrV4>,
    ) {
        let keypair = KeyPair::from_ikm(b"test all sockets").unwrap();
        let signed_record = MonadNameRecord::<SecpSignature>::new(record, &keypair);
        let sockets = signed_record.all_udp_sockets().collect::<Vec<_>>();
        assert_eq!(sockets, expected_sockets);
    }

    #[test]
    fn test_peer_entry_to_monad_name_record_invalid_signature() {
        let ip = Ipv4Addr::from_str("192.168.1.200").unwrap();
        let port = 9000u16;
        let auth_port = 9001u16;
        let seq = 10u64;

        let keypair = KeyPair::from_ikm(b"test invalid sig").unwrap();
        let other_keypair = KeyPair::from_ikm(b"other key").unwrap();
        let addr = SocketAddrV4::new(ip, port);

        let name_record = NameRecord::new_with_authentication(ip, port, port, auth_port, seq);
        let mut encoded = Vec::new();
        name_record.encode(&mut encoded);
        let wrong_signature =
            SecpSignature::sign::<signing_domain::NameRecord>(&encoded, &other_keypair);

        let peer_entry = PeerEntry {
            pubkey: keypair.pubkey(),
            addr: addr.to_string(),
            signature: wrong_signature,
            record_seq_num: seq,
            auth_port: Some(auth_port),
            direct_udp_port: None,
        };

        assert!(matches!(
            MonadNameRecord::<SecpSignature>::try_from(&peer_entry),
            Err(PeerEntryConversionError::InvalidSignature(_))
        ));
    }

    #[test]
    fn test_peer_entry_monad_name_record_requires_authenticated_udp_port() {
        let ip = Ipv4Addr::from_str("10.0.0.1").unwrap();
        let port = 5000u16;
        let seq = 1u64;

        let keypair = KeyPair::from_ikm(b"test missing auth port").unwrap();
        let name_record = NameRecord::new_with_ports(ip, port, None, port, None, seq);
        let original_monad_record = MonadNameRecord::<SecpSignature>::new(name_record, &keypair);

        let peer_entry = PeerEntry {
            pubkey: original_monad_record.recover_pubkey().unwrap().pubkey(),
            addr: ip.to_string(),
            signature: original_monad_record.signature,
            record_seq_num: seq,
            auth_port: None,
            direct_udp_port: None,
        };
        assert!(matches!(
            MonadNameRecord::<SecpSignature>::try_from(&peer_entry),
            Err(PeerEntryConversionError::MissingAuthenticatedUdpPort)
        ));
    }

    #[test]
    fn test_peer_entry_monad_name_record_roundtrip_with_auth() {
        let ip = Ipv4Addr::from_str("172.31.0.100").unwrap();
        let port = 8888u16;
        let auth_port = 8889u16;
        let seq = 99u64;

        let keypair = KeyPair::from_ikm(b"test roundtrip auth").unwrap();
        let name_record = NameRecord::new_with_authentication(ip, port, port, auth_port, seq);
        let original_monad_record = MonadNameRecord::<SecpSignature>::new(name_record, &keypair);

        let pubkey = original_monad_record.recover_pubkey().unwrap().pubkey();
        let peer_entry = PeerEntry::from(original_monad_record.with_pubkey(pubkey));
        assert_eq!(peer_entry.auth_port, Some(auth_port));

        let converted_monad_record =
            MonadNameRecord::<SecpSignature>::try_from(&peer_entry).unwrap();

        assert_eq!(
            converted_monad_record.name_record,
            original_monad_record.name_record
        );
        assert_eq!(
            converted_monad_record.signature,
            original_monad_record.signature
        );
    }

    #[test]
    fn test_peer_entry_monad_name_record_roundtrip_auth_only() {
        let ip = Ipv4Addr::from_str("172.31.0.110").unwrap();
        let tcp_port = 8895u16;
        let auth_port = 8896u16;
        let seq = 102u64;

        let keypair = KeyPair::from_ikm(b"test roundtrip auth only").unwrap();
        let name_record = NameRecord::new_with_ports(ip, tcp_port, None, auth_port, None, seq);
        let original_monad_record = MonadNameRecord::<SecpSignature>::new(name_record, &keypair);

        let pubkey = original_monad_record.recover_pubkey().unwrap().pubkey();
        let peer_entry = PeerEntry::from(original_monad_record.with_pubkey(pubkey));
        assert_eq!(peer_entry.addr, SocketAddrV4::new(ip, tcp_port).to_string());
        assert_eq!(peer_entry.auth_port, Some(auth_port));

        let converted_monad_record =
            MonadNameRecord::<SecpSignature>::try_from(&peer_entry).unwrap();

        assert_eq!(
            converted_monad_record.name_record,
            original_monad_record.name_record
        );
        assert_eq!(
            converted_monad_record.signature,
            original_monad_record.signature
        );
    }

    #[test]
    fn test_peer_entry_monad_name_record_roundtrip_auth_only_ip_form() {
        let ip = Ipv4Addr::from_str("172.31.0.120").unwrap();
        let auth_port = 8897u16;
        let seq = 103u64;

        let keypair = KeyPair::from_ikm(b"test roundtrip auth only ip form").unwrap();
        let name_record = NameRecord::new_with_ports(ip, auth_port, None, auth_port, None, seq);
        let original_monad_record = MonadNameRecord::<SecpSignature>::new(name_record, &keypair);

        let pubkey = original_monad_record.recover_pubkey().unwrap().pubkey();
        let peer_entry = PeerEntry::from(original_monad_record.with_pubkey(pubkey));
        assert_eq!(peer_entry.addr, ip.to_string());
        assert_eq!(peer_entry.auth_port, Some(auth_port));

        let converted_monad_record =
            MonadNameRecord::<SecpSignature>::try_from(&peer_entry).unwrap();

        assert_eq!(
            converted_monad_record.name_record,
            original_monad_record.name_record
        );
        assert_eq!(
            converted_monad_record.signature,
            original_monad_record.signature
        );
    }

    #[test]
    fn test_peer_entry_monad_name_record_roundtrip_with_direct_udp() {
        let ip = Ipv4Addr::from_str("172.31.0.101").unwrap();
        let port = 8890u16;
        let auth_port = 8891u16;
        let direct_udp_port = 8892u16;
        let seq = 100u64;

        let keypair = KeyPair::from_ikm(b"test roundtrip direct udp").unwrap();
        let name_record =
            NameRecord::new_with_ports(ip, port, Some(port), auth_port, Some(direct_udp_port), seq);
        let original_monad_record = MonadNameRecord::<SecpSignature>::new(name_record, &keypair);

        let pubkey = original_monad_record.recover_pubkey().unwrap().pubkey();
        let peer_entry = PeerEntry::from(original_monad_record.with_pubkey(pubkey));
        assert_eq!(peer_entry.auth_port, Some(auth_port));
        assert_eq!(peer_entry.direct_udp_port, Some(direct_udp_port));

        let converted_monad_record =
            MonadNameRecord::<SecpSignature>::try_from(&peer_entry).unwrap();

        assert_eq!(
            converted_monad_record.name_record,
            original_monad_record.name_record
        );
        assert_eq!(
            converted_monad_record.signature,
            original_monad_record.signature
        );
        assert_eq!(
            converted_monad_record.name_record.authenticated_udp_port(),
            auth_port
        );
        assert_eq!(
            converted_monad_record.name_record.direct_udp_port(),
            Some(direct_udp_port)
        );
    }

    #[test]
    fn test_peer_entry_monad_name_record_requires_authenticated_udp_port_for_direct_udp() {
        let ip = Ipv4Addr::from_str("172.31.0.102").unwrap();
        let port = 8893u16;
        let auth_port = 8894u16;
        let direct_udp_port = 8894u16;
        let seq = 101u64;

        let keypair = KeyPair::from_ikm(b"test direct udp missing auth").unwrap();
        let name_record =
            NameRecord::new_with_ports(ip, port, Some(port), auth_port, Some(direct_udp_port), seq);
        let original_monad_record = MonadNameRecord::<SecpSignature>::new(name_record, &keypair);

        let peer_entry = PeerEntry {
            pubkey: original_monad_record.recover_pubkey().unwrap().pubkey(),
            addr: SocketAddrV4::new(ip, port).to_string(),
            signature: original_monad_record.signature,
            record_seq_num: seq,
            auth_port: None,
            direct_udp_port: Some(direct_udp_port),
        };
        assert_eq!(peer_entry.direct_udp_port, Some(direct_udp_port));

        assert!(matches!(
            MonadNameRecord::<SecpSignature>::try_from(&peer_entry),
            Err(PeerEntryConversionError::MissingAuthenticatedUdpPort)
        ));
    }

    #[test]
    fn test_bootstrap_peer_config_monad_name_record_roundtrip_auth_only() {
        let ip = Ipv4Addr::from_str("172.31.0.111").unwrap();
        let tcp_port = 8897u16;
        let auth_port = 8898u16;
        let seq = 103u64;

        let keypair = KeyPair::from_ikm(b"test bootstrap auth only").unwrap();
        let name_record = NameRecord::new_with_ports(ip, tcp_port, None, auth_port, None, seq);
        let original_monad_record = MonadNameRecord::<SecpSignature>::new(name_record, &keypair);

        let pubkey = original_monad_record.recover_pubkey().unwrap().pubkey();
        let peer_config: NodeBootstrapPeerConfig<_> =
            original_monad_record.with_pubkey(pubkey).into();
        assert_eq!(
            peer_config.address,
            SocketAddrV4::new(ip, tcp_port).to_string()
        );
        assert_eq!(peer_config.auth_port, Some(auth_port));

        let converted_monad_record =
            MonadNameRecord::<SecpSignature>::try_from(&peer_config).unwrap();

        assert_eq!(
            converted_monad_record.name_record,
            original_monad_record.name_record
        );
        assert_eq!(
            converted_monad_record.signature,
            original_monad_record.signature
        );
    }

    #[test]
    fn test_bootstrap_peer_config_deserializes_ip_form() {
        let ip = Ipv4Addr::from_str("172.31.0.113").unwrap();
        let auth_port = 8901u16;
        let seq = 105u64;

        let keypair = KeyPair::from_ikm(b"test bootstrap ip form").unwrap();
        let name_record = NameRecord::new_with_ports(ip, auth_port, None, auth_port, None, seq);
        let original_monad_record = MonadNameRecord::<SecpSignature>::new(name_record, &keypair);
        let pubkey = original_monad_record.recover_pubkey().unwrap().pubkey();

        let config = format!(
            r#"
[[peers]]
address = "{ip}"
record_seq_num = {seq}
secp256k1_pubkey = "0x{pubkey}"
name_record_sig = "0x{signature}"
auth_port = {auth_port}
"#,
            pubkey = hex::encode(pubkey.bytes()),
            signature = hex::encode(original_monad_record.signature.serialize()),
        );

        let parsed: monad_node_config::NodeBootstrapConfig<SecpSignature> =
            toml::from_str(&config).unwrap();
        let peer_config = parsed.peers.into_iter().next().unwrap();

        assert_eq!(peer_config.address, ip.to_string());

        let converted = MonadNameRecord::<SecpSignature>::try_from(&peer_config).unwrap();
        assert_eq!(converted.name_record, original_monad_record.name_record);
        assert_eq!(converted.signature, original_monad_record.signature);
    }
}
