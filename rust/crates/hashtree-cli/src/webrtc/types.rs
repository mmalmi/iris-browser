//! WebRTC signaling types compatible with iris-client and hashtree-ts

use hashtree_network::DataMessage as SharedDataMessage;
pub use hashtree_network::{
    decrement_htl_with_policy, should_forward_htl, validate_mesh_frame, DataQuoteRequest,
    DataQuoteResponse, DataRequest, DataResponse, HtlMode, HtlPolicy, IceCandidate, MeshNostrFrame,
    MeshNostrPayload, PeerHTLConfig, PeerId, PeerPool, PoolConfig, PoolSettings,
    RequestDispatchConfig, SelectionStrategy, SignalingMessage, TimedSeenSet, BLOB_REQUEST_POLICY,
    DECREMENT_AT_MAX_PROB, DECREMENT_AT_MIN_PROB, MAX_HTL, MESH_DEFAULT_HTL, MESH_EVENT_POLICY,
    MESH_MAX_HTL, MESH_PROTOCOL, MESH_PROTOCOL_VERSION, MSG_TYPE_QUOTE_REQUEST,
    MSG_TYPE_QUOTE_RESPONSE, MSG_TYPE_REQUEST, MSG_TYPE_RESPONSE,
};
use serde::{Deserialize, Serialize};

/// Backward-compatible helper using blob-request policy.
pub fn decrement_htl(htl: u8, config: &PeerHTLConfig) -> u8 {
    decrement_htl_with_policy(htl, &BLOB_REQUEST_POLICY, config)
}

/// Backward-compatible helper for existing call sites.
pub fn should_forward(htl: u8) -> bool {
    should_forward_htl(htl)
}

/// Event kind for WebRTC signaling (ephemeral kind 25050)
/// All signaling uses this kind - hellos use #l tag, directed use gift wrap
pub const WEBRTC_KIND: u64 = 25050;

/// Tag for hello messages (broadcast discovery)
pub const HELLO_TAG: &str = "hello";

/// Legacy tag for WebRTC signaling messages (kept for compatibility)
pub const WEBRTC_TAG: &str = "webrtc";

/// Configuration for WebRTC manager
#[derive(Clone)]
pub struct WebRTCConfig {
    /// Nostr relays for signaling
    pub relays: Vec<String>,
    /// Whether negotiated WebRTC signaling should run at all.
    pub signaling_enabled: bool,
    /// Maximum outbound connections (legacy, use pools instead)
    pub max_outbound: usize,
    /// Maximum inbound connections (legacy, use pools instead)
    pub max_inbound: usize,
    /// Hello message interval in milliseconds
    pub hello_interval_ms: u64,
    /// Message timeout in milliseconds
    pub message_timeout_ms: u64,
    /// STUN servers for NAT traversal
    pub stun_servers: Vec<String>,
    /// Enable debug logging
    pub debug: bool,
    /// Optional LAN multicast transport for offline discovery + root lookup.
    pub multicast: super::multicast::MulticastConfig,
    /// Optional Android Wi-Fi Aware nearby discovery/signaling bus.
    pub wifi_aware: super::wifi_aware::WifiAwareConfig,
    /// Optional native Bluetooth peer transport.
    pub bluetooth: super::bluetooth::BluetoothConfig,
    /// Pool settings for follows and other peers
    pub pools: PoolSettings,
    /// Retrieval peer selection strategy (shared with simulation).
    pub request_selection_strategy: SelectionStrategy,
    /// Whether fairness constraints are enabled for retrieval peer selection.
    pub request_fairness_enabled: bool,
    /// Hedged request dispatch policy for retrieval (shared with simulation).
    pub request_dispatch: RequestDispatchConfig,
}

impl Default for WebRTCConfig {
    fn default() -> Self {
        Self {
            relays: vec![
                "wss://relay.damus.io".to_string(),
                "wss://relay.primal.net".to_string(),
                "wss://temp.iris.to".to_string(),
                "wss://relay.snort.social".to_string(),
            ],
            signaling_enabled: true,
            max_outbound: 6,
            max_inbound: 6,
            hello_interval_ms: 3000,
            message_timeout_ms: 15000,
            stun_servers: vec![
                "stun:stun.iris.to:3478".to_string(),
                "stun:stun.l.google.com:19302".to_string(),
                "stun:stun.cloudflare.com:3478".to_string(),
            ],
            debug: false,
            multicast: super::multicast::MulticastConfig::default(),
            wifi_aware: super::wifi_aware::WifiAwareConfig::default(),
            bluetooth: super::bluetooth::BluetoothConfig::default(),
            pools: PoolSettings::default(),
            request_selection_strategy: SelectionStrategy::TitForTat,
            request_fairness_enabled: true,
            request_dispatch: RequestDispatchConfig {
                initial_fanout: 2,
                hedge_fanout: 1,
                max_fanout: 8,
                hedge_interval_ms: 120,
            },
        }
    }
}

/// Peer connection status
#[derive(Debug, Clone)]
pub struct PeerStatus {
    pub peer_id: String,
    pub pubkey: String,
    pub state: String,
    pub direction: PeerDirection,
    pub connected_at: Option<std::time::Instant>,
    pub pool: PeerPool,
}

/// Direction of peer connection
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerDirection {
    Inbound,
    Outbound,
}

/// Peer state change event for signaling layer notification
#[derive(Debug, Clone)]
pub enum PeerStateEvent {
    /// Peer connection succeeded
    Connected(PeerId),
    /// Peer connection failed
    Failed(PeerId),
    /// Peer disconnected
    Disconnected(PeerId),
}

impl std::fmt::Display for PeerDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerDirection::Inbound => write!(f, "inbound"),
            PeerDirection::Outbound => write!(f, "outbound"),
        }
    }
}

pub const MSG_TYPE_PAYMENT: u8 = 0x04;
pub const MSG_TYPE_PAYMENT_ACK: u8 = 0x05;
pub const MSG_TYPE_CHUNK: u8 = 0x06;

/// Hashtree data channel protocol messages
/// Shared between WebRTC data channels and WebSocket transport
///
/// Wire format: [type byte][msgpack body]
/// Request:  [0x00][msgpack: {h: bytes32, htl?: u8, q?: u64}]
/// Response: [0x01][msgpack: {h: bytes32, d: bytes}]
/// QuoteRequest:  [0x02][msgpack: {h: bytes32, p: u64, t: u32, m?: string}]
/// QuoteResponse: [0x03][msgpack: {h: bytes32, a: bool, q?: u64, p?: u64, t?: u32, m?: string}]
/// Payment:       [0x04][msgpack: {h: bytes32, q: u64, c: u32, p: u64, m?: string, tok: string}]
/// PaymentAck:    [0x05][msgpack: {h: bytes32, q: u64, c: u32, a: bool, e?: string}]
/// Chunk:         [0x06][msgpack: {h: bytes32, q: u64, c: u32, n: u32, p: u64, d: bytes}]

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataPayment {
    #[serde(with = "serde_bytes")]
    pub h: Vec<u8>,
    pub q: u64,
    pub c: u32,
    pub p: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub m: Option<String>,
    pub tok: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataPaymentAck {
    #[serde(with = "serde_bytes")]
    pub h: Vec<u8>,
    pub q: u64,
    pub c: u32,
    pub a: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub e: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataChunk {
    #[serde(with = "serde_bytes")]
    pub h: Vec<u8>,
    pub q: u64,
    pub c: u32,
    pub n: u32,
    pub p: u64,
    #[serde(with = "serde_bytes")]
    pub d: Vec<u8>,
}

#[derive(Debug, Clone)]
pub enum DataMessage {
    Request(DataRequest),
    Response(DataResponse),
    QuoteRequest(DataQuoteRequest),
    QuoteResponse(DataQuoteResponse),
    Payment(DataPayment),
    PaymentAck(DataPaymentAck),
    Chunk(DataChunk),
}

/// Encode a request to wire format: [0x00][msgpack body]
/// Uses named fields for cross-language compatibility with TypeScript
pub fn encode_request(req: &DataRequest) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    Ok(hashtree_network::encode_request(req))
}

/// Encode a response to wire format: [0x01][msgpack body]
/// Uses named fields for cross-language compatibility with TypeScript
pub fn encode_response(res: &DataResponse) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    Ok(hashtree_network::encode_response(res))
}

/// Encode a quote request to wire format: [0x02][msgpack body]
pub fn encode_quote_request(req: &DataQuoteRequest) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    Ok(hashtree_network::encode_quote_request(req))
}

/// Encode a quote response to wire format: [0x03][msgpack body]
pub fn encode_quote_response(res: &DataQuoteResponse) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    Ok(hashtree_network::encode_quote_response(res))
}

pub fn encode_payment(req: &DataPayment) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    let body = rmp_serde::to_vec_named(req)?;
    let mut result = Vec::with_capacity(1 + body.len());
    result.push(MSG_TYPE_PAYMENT);
    result.extend(body);
    Ok(result)
}

pub fn encode_payment_ack(res: &DataPaymentAck) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    let body = rmp_serde::to_vec_named(res)?;
    let mut result = Vec::with_capacity(1 + body.len());
    result.push(MSG_TYPE_PAYMENT_ACK);
    result.extend(body);
    Ok(result)
}

pub fn encode_chunk(chunk: &DataChunk) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    let body = rmp_serde::to_vec_named(chunk)?;
    let mut result = Vec::with_capacity(1 + body.len());
    result.push(MSG_TYPE_CHUNK);
    result.extend(body);
    Ok(result)
}

/// Parse a wire format message
pub fn parse_message(data: &[u8]) -> Result<DataMessage, rmp_serde::decode::Error> {
    if data.is_empty() {
        return Err(rmp_serde::decode::Error::LengthMismatch(0));
    }

    let msg_type = data[0];

    match msg_type {
        MSG_TYPE_REQUEST | MSG_TYPE_RESPONSE | MSG_TYPE_QUOTE_REQUEST | MSG_TYPE_QUOTE_RESPONSE => {
            match hashtree_network::parse_message(data) {
                Some(SharedDataMessage::Request(req)) => Ok(DataMessage::Request(req)),
                Some(SharedDataMessage::Response(res)) => Ok(DataMessage::Response(res)),
                Some(SharedDataMessage::QuoteRequest(req)) => Ok(DataMessage::QuoteRequest(req)),
                Some(SharedDataMessage::QuoteResponse(res)) => Ok(DataMessage::QuoteResponse(res)),
                None => Err(rmp_serde::decode::Error::LengthMismatch(msg_type as u32)),
            }
        }
        MSG_TYPE_PAYMENT => {
            let req: DataPayment = rmp_serde::from_slice(&data[1..])?;
            Ok(DataMessage::Payment(req))
        }
        MSG_TYPE_PAYMENT_ACK => {
            let res: DataPaymentAck = rmp_serde::from_slice(&data[1..])?;
            Ok(DataMessage::PaymentAck(res))
        }
        MSG_TYPE_CHUNK => {
            let chunk: DataChunk = rmp_serde::from_slice(&data[1..])?;
            Ok(DataMessage::Chunk(chunk))
        }
        _ => Err(rmp_serde::decode::Error::LengthMismatch(msg_type as u32)),
    }
}

/// Convert hash to hex string for logging/map keys
pub fn hash_to_hex(hash: &[u8]) -> String {
    hashtree_network::hash_to_key(hash)
}

/// Encode a DataMessage to wire format (deprecated - use encode_request/encode_response)
pub fn encode_message(msg: &DataMessage) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    match msg {
        DataMessage::Request(req) => encode_request(req),
        DataMessage::Response(res) => encode_response(res),
        DataMessage::QuoteRequest(req) => encode_quote_request(req),
        DataMessage::QuoteResponse(res) => encode_quote_response(res),
        DataMessage::Payment(req) => encode_payment(req),
        DataMessage::PaymentAck(res) => encode_payment_ack(res),
        DataMessage::Chunk(chunk) => encode_chunk(chunk),
    }
}
