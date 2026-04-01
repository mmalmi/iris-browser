//! Stub module for when P2P feature is disabled
//! Provides minimal types to allow code to compile without webrtc dependencies

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

/// Connection state stub
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Connected,
    Disconnected,
}

/// Peer entry stub
#[derive(Debug)]
pub struct PeerEntry {
    pub state: ConnectionState,
    pub peer_id: PeerId,
    pub peer: Option<DummyPeer>,
    pub pool: PeerPool,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

/// Peer ID stub
#[derive(Debug)]
pub struct PeerId {
    pub pubkey: String,
}

impl PeerId {
    pub fn short(&self) -> &str {
        &self.pubkey[..8.min(self.pubkey.len())]
    }
}

/// Dummy peer stub
#[derive(Debug)]
pub struct DummyPeer;

impl DummyPeer {
    pub fn has_data_channel(&self) -> bool {
        false
    }
    pub fn state(&self) -> &str {
        "Disabled"
    }
    pub async fn request(&self, _hash: &str) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
}

/// Peer pool stub
#[derive(Debug, Clone, Copy)]
pub enum PeerPool {
    None,
}

#[derive(Debug, Clone)]
pub struct PeerRootEvent {
    pub hash: String,
    pub key: Option<String>,
    pub encrypted_key: Option<String>,
    pub self_encrypted_key: Option<String>,
    pub event_id: String,
    pub created_at: u64,
    pub peer_id: String,
}

/// WebRTC state stub - always empty when P2P is disabled
#[derive(Debug)]
pub struct WebRTCState {
    pub peers: Arc<RwLock<HashMap<String, PeerEntry>>>,
}

impl Default for WebRTCState {
    fn default() -> Self {
        Self {
            peers: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl WebRTCState {
    /// Query peers for data - always returns None when P2P is disabled
    pub async fn query_peers_for_data(&self, _hash: &str) -> Option<Vec<u8>> {
        None
    }

    /// Request from peers - always returns None when P2P is disabled
    pub async fn request_from_peers(&self, _hash: &str) -> Option<Vec<u8>> {
        None
    }

    /// Request from peers with source - always returns None when P2P is disabled
    pub async fn request_from_peers_with_source(&self, _hash: &str) -> Option<(Vec<u8>, String)> {
        None
    }

    /// Get bandwidth stats - always returns zeros when P2P is disabled
    pub fn get_bandwidth(&self) -> (u64, u64) {
        (0, 0)
    }

    /// Get mesh stats - always returns zeros when P2P is disabled
    pub fn get_mesh_stats(&self) -> (u64, u64, u64) {
        (0, 0, 0)
    }

    /// Resolve roots from peers - always returns None when P2P is disabled
    pub async fn resolve_root_from_peers(
        &self,
        _owner_pubkey: &str,
        _tree_name: &str,
        _per_peer_timeout: Duration,
    ) -> Option<PeerRootEvent> {
        None
    }
}

/// Content store trait stub
pub trait ContentStore: Send + Sync + 'static {
    /// Get content by hex hash
    fn get(&self, hash_hex: &str) -> Result<Option<Vec<u8>>>;
}

pub mod types {
    use super::*;

    pub const MAX_HTL: u8 = 7;
    pub const MSG_TYPE_REQUEST: u8 = 0x00;
    pub const MSG_TYPE_RESPONSE: u8 = 0x01;
    pub const MSG_TYPE_QUOTE_REQUEST: u8 = 0x02;
    pub const MSG_TYPE_QUOTE_RESPONSE: u8 = 0x03;
    pub const MSG_TYPE_PAYMENT: u8 = 0x04;
    pub const MSG_TYPE_PAYMENT_ACK: u8 = 0x05;
    pub const MSG_TYPE_CHUNK: u8 = 0x06;

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct DataRequest {
        #[serde(with = "serde_bytes")]
        pub h: Vec<u8>,
        #[serde(default = "default_htl")]
        pub htl: u8,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub q: Option<u64>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct DataResponse {
        #[serde(with = "serde_bytes")]
        pub h: Vec<u8>,
        #[serde(with = "serde_bytes")]
        pub d: Vec<u8>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct DataQuoteRequest {
        #[serde(with = "serde_bytes")]
        pub h: Vec<u8>,
        pub p: u64,
        pub t: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub m: Option<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct DataQuoteResponse {
        #[serde(with = "serde_bytes")]
        pub h: Vec<u8>,
        pub a: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub q: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub p: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub t: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub m: Option<String>,
    }

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

    fn default_htl() -> u8 {
        MAX_HTL
    }

    pub fn encode_request(req: &DataRequest) -> Result<Vec<u8>, rmp_serde::encode::Error> {
        let body = rmp_serde::to_vec_named(req)?;
        let mut result = Vec::with_capacity(1 + body.len());
        result.push(MSG_TYPE_REQUEST);
        result.extend(body);
        Ok(result)
    }

    pub fn encode_response(res: &DataResponse) -> Result<Vec<u8>, rmp_serde::encode::Error> {
        let body = rmp_serde::to_vec_named(res)?;
        let mut result = Vec::with_capacity(1 + body.len());
        result.push(MSG_TYPE_RESPONSE);
        result.extend(body);
        Ok(result)
    }

    pub fn encode_quote_request(
        req: &DataQuoteRequest,
    ) -> Result<Vec<u8>, rmp_serde::encode::Error> {
        let body = rmp_serde::to_vec_named(req)?;
        let mut result = Vec::with_capacity(1 + body.len());
        result.push(MSG_TYPE_QUOTE_REQUEST);
        result.extend(body);
        Ok(result)
    }

    pub fn encode_quote_response(
        res: &DataQuoteResponse,
    ) -> Result<Vec<u8>, rmp_serde::encode::Error> {
        let body = rmp_serde::to_vec_named(res)?;
        let mut result = Vec::with_capacity(1 + body.len());
        result.push(MSG_TYPE_QUOTE_RESPONSE);
        result.extend(body);
        Ok(result)
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

    pub fn parse_message(data: &[u8]) -> Result<DataMessage, rmp_serde::decode::Error> {
        if data.is_empty() {
            return Err(rmp_serde::decode::Error::LengthMismatch(0));
        }

        match data[0] {
            MSG_TYPE_REQUEST => Ok(DataMessage::Request(rmp_serde::from_slice(&data[1..])?)),
            MSG_TYPE_RESPONSE => Ok(DataMessage::Response(rmp_serde::from_slice(&data[1..])?)),
            MSG_TYPE_QUOTE_REQUEST => Ok(DataMessage::QuoteRequest(rmp_serde::from_slice(
                &data[1..],
            )?)),
            MSG_TYPE_QUOTE_RESPONSE => Ok(DataMessage::QuoteResponse(rmp_serde::from_slice(
                &data[1..],
            )?)),
            MSG_TYPE_PAYMENT => Ok(DataMessage::Payment(rmp_serde::from_slice(&data[1..])?)),
            MSG_TYPE_PAYMENT_ACK => Ok(DataMessage::PaymentAck(rmp_serde::from_slice(&data[1..])?)),
            MSG_TYPE_CHUNK => Ok(DataMessage::Chunk(rmp_serde::from_slice(&data[1..])?)),
            other => Err(rmp_serde::decode::Error::LengthMismatch(other as u32)),
        }
    }
}
