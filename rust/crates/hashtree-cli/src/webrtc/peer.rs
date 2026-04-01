//! WebRTC peer connection for hashtree data exchange

use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use hashtree_network::{PeerLink as RoutedPeerLink, TransportError as RoutedTransportError};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex, Notify};
use tracing::{debug, error, info, warn};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

use super::cashu::{CashuQuoteState, ExpectedSettlement};
use super::types::{
    encode_chunk, encode_message, encode_payment, encode_payment_ack, encode_quote_response,
    encode_request, encode_response, hash_to_hex, parse_message, validate_mesh_frame, DataChunk,
    DataMessage, DataPayment, DataPaymentAck, DataQuoteRequest, DataRequest, DataResponse,
    MeshNostrFrame, PeerDirection, PeerHTLConfig, PeerId, PeerStateEvent, SignalingMessage,
    BLOB_REQUEST_POLICY,
};
use crate::nostr_relay::NostrRelay;
use nostr::{
    ClientMessage as NostrClientMessage, Filter as NostrFilter, JsonUtil as NostrJsonUtil,
    RelayMessage as NostrRelayMessage, SubscriptionId as NostrSubscriptionId,
};

/// Trait for content storage that can be used by WebRTC peers
pub trait ContentStore: Send + Sync + 'static {
    /// Get content by hex hash
    fn get(&self, hash_hex: &str) -> Result<Option<Vec<u8>>>;
}

/// Pending request tracking (keyed by hash hex)
pub struct PendingRequest {
    pub hash: Vec<u8>,
    pub response_tx: oneshot::Sender<Option<Vec<u8>>>,
    pub quoted: Option<PendingQuotedRequest>,
}

pub struct PendingQuotedRequest {
    pub quote_id: u64,
    pub mint_url: String,
    pub total_payment_sat: u64,
    pub confirmed_payment_sat: u64,
    pub next_chunk_index: u32,
    pub total_chunks: Option<u32>,
    pub assembled_data: Vec<u8>,
    pub in_flight_payment: Option<PendingChunkPayment>,
    pub buffered_chunk: Option<DataChunk>,
}

pub struct PendingChunkPayment {
    pub chunk_index: u32,
    pub amount_sat: u64,
    pub mint_url: String,
    pub operation_id: String,
    pub final_chunk: bool,
}

impl PendingRequest {
    pub fn standard(hash: Vec<u8>, response_tx: oneshot::Sender<Option<Vec<u8>>>) -> Self {
        Self {
            hash,
            response_tx,
            quoted: None,
        }
    }

    pub fn quoted(
        hash: Vec<u8>,
        response_tx: oneshot::Sender<Option<Vec<u8>>>,
        quote_id: u64,
        mint_url: String,
        total_payment_sat: u64,
    ) -> Self {
        Self {
            hash,
            response_tx,
            quoted: Some(PendingQuotedRequest {
                quote_id,
                mint_url,
                total_payment_sat,
                confirmed_payment_sat: 0,
                next_chunk_index: 0,
                total_chunks: None,
                assembled_data: Vec::new(),
                in_flight_payment: None,
                buffered_chunk: None,
            }),
        }
    }
}

async fn handle_quote_request_message(
    peer_short: &str,
    peer_id: &PeerId,
    store: &Option<Arc<dyn ContentStore>>,
    cashu_quotes: Option<&Arc<CashuQuoteState>>,
    req: &DataQuoteRequest,
) -> Option<super::types::DataQuoteResponse> {
    let Some(cashu_quotes) = cashu_quotes else {
        debug!(
            "[Peer {}] Ignoring quote request without Cashu policy",
            peer_short
        );
        return None;
    };

    if cashu_quotes
        .should_refuse_requests_from_peer(&peer_id.to_string())
        .await
    {
        return Some(
            cashu_quotes
                .build_quote_response(&peer_id.to_string(), req, false)
                .await,
        );
    }

    let hash_hex = hash_to_hex(&req.h);
    let can_serve = if let Some(store) = store {
        match store.get(&hash_hex) {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(e) => {
                warn!("[Peer {}] Store error during quote: {}", peer_short, e);
                false
            }
        }
    } else {
        false
    };

    Some(
        cashu_quotes
            .build_quote_response(&peer_id.to_string(), req, can_serve)
            .await,
    )
}

struct PaymentHandlingOutcome {
    ack: DataPaymentAck,
    next_chunk: Option<(DataChunk, ExpectedSettlement)>,
}

async fn send_quoted_chunk(
    dc: &Arc<RTCDataChannel>,
    peer_id: &PeerId,
    peer_short: &str,
    cashu_quotes: &Arc<CashuQuoteState>,
    chunk: DataChunk,
    expected: ExpectedSettlement,
) -> bool {
    let hash_hex = hash_to_hex(&chunk.h);
    let wire = match encode_chunk(&chunk) {
        Ok(wire) => wire,
        Err(err) => {
            warn!(
                "[Peer {}] Failed to encode quoted chunk {} for quote {}: {}",
                peer_short, chunk.c, chunk.q, err
            );
            return false;
        }
    };

    if let Err(err) = dc.send(&Bytes::from(wire)).await {
        warn!(
            "[Peer {}] Failed to send quoted chunk {} for quote {}: {}",
            peer_short, chunk.c, chunk.q, err
        );
        return false;
    }

    cashu_quotes
        .register_expected_payment(peer_id.to_string(), hash_hex, chunk.q, expected)
        .await;
    true
}

async fn fail_pending_request(
    pending_requests: &Arc<Mutex<HashMap<String, PendingRequest>>>,
    cashu_quotes: Option<&Arc<CashuQuoteState>>,
    hash_hex: &str,
) {
    let pending = pending_requests.lock().await.remove(hash_hex);
    let Some(pending) = pending else {
        return;
    };

    if let (Some(cashu_quotes), Some(quoted)) = (cashu_quotes, pending.quoted) {
        if let Some(in_flight) = quoted.in_flight_payment {
            let _ = cashu_quotes
                .revoke_payment_token(&in_flight.mint_url, &in_flight.operation_id)
                .await;
        }
    }
    let _ = pending.response_tx.send(None);
}

async fn process_chunk_message(
    peer_short: &str,
    _peer_id: &PeerId,
    dc: &Arc<RTCDataChannel>,
    pending_requests: &Arc<Mutex<HashMap<String, PendingRequest>>>,
    cashu_quotes: Option<&Arc<CashuQuoteState>>,
    chunk: DataChunk,
) {
    let hash_hex = hash_to_hex(&chunk.h);
    let Some(cashu_quotes) = cashu_quotes else {
        fail_pending_request(pending_requests, None, &hash_hex).await;
        return;
    };

    enum ChunkAction {
        BufferOnly,
        Fail,
        Pay {
            mint_url: String,
            amount_sat: u64,
            final_chunk: bool,
        },
    }

    let action = {
        let mut pending = pending_requests.lock().await;
        let Some(request) = pending.get_mut(&hash_hex) else {
            return;
        };
        match request.quoted.as_mut() {
            None => ChunkAction::Fail,
            Some(quoted) if quoted.quote_id != chunk.q || chunk.n == 0 => ChunkAction::Fail,
            Some(quoted) => {
                if let Some(in_flight) = quoted.in_flight_payment.as_ref() {
                    let expected_buffer_index = in_flight.chunk_index + 1;
                    if chunk.c == expected_buffer_index && quoted.buffered_chunk.is_none() {
                        quoted.buffered_chunk = Some(chunk.clone());
                        ChunkAction::BufferOnly
                    } else {
                        ChunkAction::Fail
                    }
                } else if chunk.c != quoted.next_chunk_index {
                    ChunkAction::Fail
                } else if let Some(total_chunks) = quoted.total_chunks {
                    if total_chunks != chunk.n {
                        ChunkAction::Fail
                    } else {
                        let next_total = quoted.confirmed_payment_sat.saturating_add(chunk.p);
                        if next_total > quoted.total_payment_sat
                            || (chunk.c + 1 == chunk.n && next_total != quoted.total_payment_sat)
                        {
                            ChunkAction::Fail
                        } else {
                            quoted.total_chunks = Some(chunk.n);
                            quoted.assembled_data.extend_from_slice(&chunk.d);
                            quoted.next_chunk_index += 1;
                            ChunkAction::Pay {
                                mint_url: quoted.mint_url.clone(),
                                amount_sat: chunk.p,
                                final_chunk: chunk.c + 1 == chunk.n,
                            }
                        }
                    }
                } else {
                    let next_total = quoted.confirmed_payment_sat.saturating_add(chunk.p);
                    if next_total > quoted.total_payment_sat
                        || (chunk.c + 1 == chunk.n && next_total != quoted.total_payment_sat)
                    {
                        ChunkAction::Fail
                    } else {
                        quoted.total_chunks = Some(chunk.n);
                        quoted.assembled_data.extend_from_slice(&chunk.d);
                        quoted.next_chunk_index += 1;
                        ChunkAction::Pay {
                            mint_url: quoted.mint_url.clone(),
                            amount_sat: chunk.p,
                            final_chunk: chunk.c + 1 == chunk.n,
                        }
                    }
                }
            }
        }
    };

    match action {
        ChunkAction::BufferOnly => (),
        ChunkAction::Fail => {
            warn!(
                "[Peer {}] Invalid quoted chunk {} for hash {}",
                peer_short, chunk.c, hash_hex
            );
            fail_pending_request(pending_requests, Some(cashu_quotes), &hash_hex).await;
        }
        ChunkAction::Pay {
            mint_url,
            amount_sat,
            final_chunk,
        } => {
            let payment = match cashu_quotes
                .create_payment_token(&mint_url, amount_sat)
                .await
            {
                Ok(payment) => payment,
                Err(err) => {
                    warn!(
                        "[Peer {}] Failed to create payment token for chunk {} of {}: {}",
                        peer_short, chunk.c, hash_hex, err
                    );
                    fail_pending_request(pending_requests, Some(cashu_quotes), &hash_hex).await;
                    return;
                }
            };

            {
                let mut pending = pending_requests.lock().await;
                let Some(request) = pending.get_mut(&hash_hex) else {
                    let _ = cashu_quotes
                        .revoke_payment_token(&payment.mint_url, &payment.operation_id)
                        .await;
                    return;
                };
                let Some(quoted) = request.quoted.as_mut() else {
                    let _ = cashu_quotes
                        .revoke_payment_token(&payment.mint_url, &payment.operation_id)
                        .await;
                    return;
                };
                quoted.in_flight_payment = Some(PendingChunkPayment {
                    chunk_index: chunk.c,
                    amount_sat,
                    mint_url: payment.mint_url.clone(),
                    operation_id: payment.operation_id.clone(),
                    final_chunk,
                });
            }

            let payment_msg = DataPayment {
                h: chunk.h,
                q: chunk.q,
                c: chunk.c,
                p: amount_sat,
                m: Some(payment.mint_url.clone()),
                tok: payment.token,
            };
            let wire = match encode_payment(&payment_msg) {
                Ok(wire) => wire,
                Err(err) => {
                    warn!(
                        "[Peer {}] Failed to encode payment for chunk {} of {}: {}",
                        peer_short, chunk.c, hash_hex, err
                    );
                    fail_pending_request(pending_requests, Some(cashu_quotes), &hash_hex).await;
                    return;
                }
            };
            if let Err(err) = dc.send(&Bytes::from(wire)).await {
                warn!(
                    "[Peer {}] Failed to send payment for chunk {} of {}: {}",
                    peer_short, chunk.c, hash_hex, err
                );
                fail_pending_request(pending_requests, Some(cashu_quotes), &hash_hex).await;
            }
        }
    }
}

async fn handle_payment_ack_message(
    peer_short: &str,
    peer_id: &PeerId,
    dc: &Arc<RTCDataChannel>,
    pending_requests: &Arc<Mutex<HashMap<String, PendingRequest>>>,
    cashu_quotes: Option<&Arc<CashuQuoteState>>,
    ack: DataPaymentAck,
) {
    let Some(cashu_quotes) = cashu_quotes else {
        return;
    };
    let hash_hex = hash_to_hex(&ack.h);
    let mut buffered_next = None;
    let mut completed = None;
    let mut failed = None;
    let mut confirmed_amount = None;
    let mut completed_data = None;

    {
        let mut pending = pending_requests.lock().await;
        let Some(request) = pending.get_mut(&hash_hex) else {
            return;
        };
        let Some(quoted) = request.quoted.as_mut() else {
            return;
        };
        let Some(in_flight) = quoted.in_flight_payment.take() else {
            return;
        };
        if ack.q != quoted.quote_id || ack.c != in_flight.chunk_index {
            quoted.in_flight_payment = Some(in_flight);
            return;
        }

        if !ack.a {
            failed = Some(in_flight);
        } else {
            quoted.confirmed_payment_sat = quoted
                .confirmed_payment_sat
                .saturating_add(in_flight.amount_sat);
            confirmed_amount = Some(in_flight.amount_sat);
            if in_flight.final_chunk {
                completed_data = Some(quoted.assembled_data.clone());
            } else if let Some(next_chunk) = quoted.buffered_chunk.take() {
                buffered_next = Some(next_chunk);
            }
        }

        if let Some(data) = completed_data.take() {
            let finished = pending
                .remove(&hash_hex)
                .expect("pending request must exist");
            completed = Some((finished.response_tx, data));
        }
    }

    if let Some(amount_sat) = confirmed_amount {
        cashu_quotes
            .record_paid_peer(&peer_id.to_string(), amount_sat)
            .await;
    }

    if let Some(in_flight) = failed {
        warn!(
            "[Peer {}] Payment ack rejected chunk {} for {}: {}",
            peer_short,
            ack.c,
            hash_hex,
            ack.e.as_deref().unwrap_or("payment rejected")
        );
        let _ = cashu_quotes
            .revoke_payment_token(&in_flight.mint_url, &in_flight.operation_id)
            .await;
        let removed = pending_requests.lock().await.remove(&hash_hex);
        if let Some(removed) = removed {
            let _ = removed.response_tx.send(None);
        }
        return;
    }

    if let Some((tx, data)) = completed {
        let _ = tx.send(Some(data));
        return;
    }

    if let Some(next_chunk) = buffered_next {
        process_chunk_message(
            peer_short,
            peer_id,
            dc,
            pending_requests,
            Some(cashu_quotes),
            next_chunk,
        )
        .await;
    }
}

async fn handle_payment_message(
    peer_id: &PeerId,
    cashu_quotes: Option<&Arc<CashuQuoteState>>,
    req: &DataPayment,
) -> PaymentHandlingOutcome {
    let nack = |err: String| PaymentHandlingOutcome {
        ack: DataPaymentAck {
            h: req.h.clone(),
            q: req.q,
            c: req.c,
            a: false,
            e: Some(err),
        },
        next_chunk: None,
    };

    let Some(cashu_quotes) = cashu_quotes else {
        return nack("Cashu settlement unavailable".to_string());
    };

    let expected = match cashu_quotes
        .claim_expected_payment(
            &peer_id.to_string(),
            &req.h,
            req.q,
            req.c,
            req.p,
            req.m.as_deref(),
        )
        .await
    {
        Ok(expected) => expected,
        Err(err) => {
            cashu_quotes
                .record_payment_default_from_peer(&peer_id.to_string())
                .await;
            return nack(err.to_string());
        }
    };

    match cashu_quotes.receive_payment_token(&req.tok).await {
        Ok(received) if received.amount_sat >= expected.payment_sat => {
            if expected.mint_url.as_deref() != Some(received.mint_url.as_str()) {
                cashu_quotes
                    .record_payment_default_from_peer(&peer_id.to_string())
                    .await;
                return nack("Received payment mint did not match quoted mint".to_string());
            }
            if let Err(err) = cashu_quotes
                .record_receipt_from_peer(
                    &peer_id.to_string(),
                    &received.mint_url,
                    received.amount_sat,
                )
                .await
            {
                warn!(
                    "[Peer {}] Failed to persist Cashu mint success for {}: {}",
                    peer_id.short(),
                    received.mint_url,
                    err
                );
            }

            let next_chunk = if expected.final_chunk {
                None
            } else {
                cashu_quotes
                    .next_outgoing_chunk(&peer_id.to_string(), &req.h, req.q)
                    .await
            };

            PaymentHandlingOutcome {
                ack: DataPaymentAck {
                    h: req.h.clone(),
                    q: req.q,
                    c: req.c,
                    a: true,
                    e: None,
                },
                next_chunk,
            }
        }
        Ok(_) => {
            cashu_quotes
                .record_payment_default_from_peer(&peer_id.to_string())
                .await;
            nack("Received payment amount was below the quoted amount".to_string())
        }
        Err(err) => {
            if let Some(mint_url) = expected.mint_url.as_deref().or(req.m.as_deref()) {
                let _ = cashu_quotes.record_mint_receive_failure(mint_url).await;
            }
            nack(err.to_string())
        }
    }
}

/// WebRTC peer connection with data channel protocol
pub struct Peer {
    pub peer_id: PeerId,
    pub direction: PeerDirection,
    pub created_at: std::time::Instant,
    pub connected_at: Option<std::time::Instant>,

    pc: Arc<RTCPeerConnection>,
    /// Data channel - can be set from callback when receiving channel from peer
    pub data_channel: Arc<Mutex<Option<Arc<RTCDataChannel>>>>,
    signaling_tx: mpsc::Sender<SignalingMessage>,
    my_peer_id: PeerId,

    // Content store for serving requests
    store: Option<Arc<dyn ContentStore>>,

    // Track pending outgoing requests (keyed by hash hex)
    pub pending_requests: Arc<Mutex<HashMap<String, PendingRequest>>>,
    // Track pending Nostr relay queries over data channel (keyed by subscription id)
    pending_nostr_queries: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<NostrRelayMessage>>>>,

    // Channel for incoming data messages
    #[allow(dead_code)]
    message_tx: mpsc::Sender<(DataMessage, Option<Vec<u8>>)>,
    #[allow(dead_code)]
    message_rx: Option<mpsc::Receiver<(DataMessage, Option<Vec<u8>>)>>,

    // Optional channel to notify signaling layer of state changes
    state_event_tx: Option<mpsc::Sender<PeerStateEvent>>,

    // Optional Nostr relay for text messages over data channel
    nostr_relay: Option<Arc<NostrRelay>>,
    // Optional channel for inbound relayless signaling mesh frames
    mesh_frame_tx: Option<mpsc::Sender<(PeerId, MeshNostrFrame)>>,
    // Optional Cashu quote negotiation state shared with signaling.
    cashu_quotes: Option<Arc<CashuQuoteState>>,
    // Per-peer HTL randomness profile (reused across traffic classes)
    htl_config: PeerHTLConfig,
}

impl Peer {
    /// Create a new peer connection
    pub async fn new(
        peer_id: PeerId,
        direction: PeerDirection,
        my_peer_id: PeerId,
        signaling_tx: mpsc::Sender<SignalingMessage>,
        stun_servers: Vec<String>,
    ) -> Result<Self> {
        Self::new_with_store_and_events(
            peer_id,
            direction,
            my_peer_id,
            signaling_tx,
            stun_servers,
            None,
            None,
            None,
            None,
            None,
        )
        .await
    }

    /// Create a new peer connection with content store
    pub async fn new_with_store(
        peer_id: PeerId,
        direction: PeerDirection,
        my_peer_id: PeerId,
        signaling_tx: mpsc::Sender<SignalingMessage>,
        stun_servers: Vec<String>,
        store: Option<Arc<dyn ContentStore>>,
    ) -> Result<Self> {
        Self::new_with_store_and_events(
            peer_id,
            direction,
            my_peer_id,
            signaling_tx,
            stun_servers,
            store,
            None,
            None,
            None,
            None,
        )
        .await
    }

    /// Create a new peer connection with content store and state event channel
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn new_with_store_and_events(
        peer_id: PeerId,
        direction: PeerDirection,
        my_peer_id: PeerId,
        signaling_tx: mpsc::Sender<SignalingMessage>,
        stun_servers: Vec<String>,
        store: Option<Arc<dyn ContentStore>>,
        state_event_tx: Option<mpsc::Sender<PeerStateEvent>>,
        nostr_relay: Option<Arc<NostrRelay>>,
        mesh_frame_tx: Option<mpsc::Sender<(PeerId, MeshNostrFrame)>>,
        cashu_quotes: Option<Arc<CashuQuoteState>>,
    ) -> Result<Self> {
        // Create WebRTC API
        let mut m = MediaEngine::default();
        m.register_default_codecs()?;

        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut m)?;

        // Enable mDNS temporarily for debugging
        // Previously disabled due to https://github.com/webrtc-rs/webrtc/issues/616
        let setting_engine = SettingEngine::default();
        // Note: mDNS enabled by default

        let api = APIBuilder::new()
            .with_media_engine(m)
            .with_interceptor_registry(registry)
            .with_setting_engine(setting_engine)
            .build();

        // Configure ICE servers
        let ice_servers: Vec<RTCIceServer> = stun_servers
            .iter()
            .map(|url| RTCIceServer {
                urls: vec![url.clone()],
                ..Default::default()
            })
            .collect();

        let config = RTCConfiguration {
            ice_servers,
            ..Default::default()
        };

        let pc = Arc::new(api.new_peer_connection(config).await?);
        let (message_tx, message_rx) = mpsc::channel(100);
        Ok(Self {
            peer_id,
            direction,
            created_at: std::time::Instant::now(),
            connected_at: None,
            pc,
            data_channel: Arc::new(Mutex::new(None)),
            signaling_tx,
            my_peer_id,
            store,
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            pending_nostr_queries: Arc::new(Mutex::new(HashMap::new())),
            message_tx,
            message_rx: Some(message_rx),
            state_event_tx,
            nostr_relay,
            mesh_frame_tx,
            cashu_quotes,
            htl_config: PeerHTLConfig::random(),
        })
    }

    /// Set content store
    pub fn set_store(&mut self, store: Arc<dyn ContentStore>) {
        self.store = Some(store);
    }

    /// Get connection state
    pub fn state(&self) -> RTCPeerConnectionState {
        self.pc.connection_state()
    }

    /// Get signaling state
    pub fn signaling_state(&self) -> webrtc::peer_connection::signaling_state::RTCSignalingState {
        self.pc.signaling_state()
    }

    /// Check if connected
    pub fn is_connected(&self) -> bool {
        self.pc.connection_state() == RTCPeerConnectionState::Connected
    }

    pub fn htl_config(&self) -> &PeerHTLConfig {
        &self.htl_config
    }

    /// Setup event handlers for the peer connection
    pub async fn setup_handlers(&self) -> Result<()> {
        let peer_id = self.peer_id.clone();
        let signaling_tx = self.signaling_tx.clone();
        let my_peer_id_str = self.my_peer_id.to_string();
        let target_peer_id = self.peer_id.to_string();

        // Handle ICE candidates - work MUST be inside the returned future
        self.pc
            .on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
                let signaling_tx = signaling_tx.clone();
                let my_peer_id_str = my_peer_id_str.clone();
                let target_peer_id = target_peer_id.clone();

                Box::pin(async move {
                    if let Some(c) = candidate {
                        if let Ok(init) = c.to_json() {
                            info!(
                                "ICE candidate generated: {}",
                                &init.candidate[..init.candidate.len().min(60)]
                            );
                            let msg = SignalingMessage::Candidate {
                                peer_id: my_peer_id_str.clone(),
                                target_peer_id: target_peer_id.clone(),
                                candidate: init.candidate,
                                sdp_m_line_index: init.sdp_mline_index,
                                sdp_mid: init.sdp_mid,
                            };
                            if let Err(e) = signaling_tx.send(msg).await {
                                error!("Failed to send ICE candidate: {}", e);
                            }
                        }
                    }
                })
            }));

        // Handle connection state changes - work MUST be inside the returned future
        let peer_id_log = peer_id.clone();
        let state_event_tx = self.state_event_tx.clone();
        self.pc
            .on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
                let peer_id = peer_id_log.clone();
                let state_event_tx = state_event_tx.clone();
                Box::pin(async move {
                    info!("Peer {} connection state: {:?}", peer_id.short(), state);

                    // Notify signaling layer of state changes
                    if let Some(tx) = state_event_tx {
                        let event = match state {
                            RTCPeerConnectionState::Connected => {
                                Some(PeerStateEvent::Connected(peer_id))
                            }
                            RTCPeerConnectionState::Failed => Some(PeerStateEvent::Failed(peer_id)),
                            RTCPeerConnectionState::Disconnected
                            | RTCPeerConnectionState::Closed => {
                                Some(PeerStateEvent::Disconnected(peer_id))
                            }
                            _ => None,
                        };
                        if let Some(event) = event {
                            if let Err(e) = tx.send(event).await {
                                error!("Failed to send peer state event: {}", e);
                            }
                        }
                    }
                })
            }));

        Ok(())
    }

    /// Initiate connection (create offer) - for outbound connections
    pub async fn connect(&self) -> Result<serde_json::Value> {
        println!("[Peer {}] Creating data channel...", self.peer_id.short());
        // Create data channel first
        // Use unordered for better performance - protocol is stateless (each message self-describes)
        let dc_init = RTCDataChannelInit {
            ordered: Some(false),
            ..Default::default()
        };
        let dc = self
            .pc
            .create_data_channel("hashtree", Some(dc_init))
            .await?;
        println!(
            "[Peer {}] Data channel created, setting up handlers...",
            self.peer_id.short()
        );
        self.setup_data_channel(dc.clone()).await?;
        println!(
            "[Peer {}] Handlers set up, storing data channel...",
            self.peer_id.short()
        );
        {
            let mut dc_guard = self.data_channel.lock().await;
            *dc_guard = Some(dc);
        }
        println!("[Peer {}] Data channel stored", self.peer_id.short());

        // Create offer and wait for ICE gathering to complete
        // This ensures all ICE candidates are embedded in the SDP
        let offer = self.pc.create_offer(None).await?;
        let mut gathering_complete = self.pc.gathering_complete_promise().await;
        self.pc.set_local_description(offer).await?;

        // Wait for ICE gathering to complete (with timeout)
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            gathering_complete.recv(),
        )
        .await;

        // Get the local description with ICE candidates embedded
        let local_desc = self
            .pc
            .local_description()
            .await
            .ok_or_else(|| anyhow::anyhow!("No local description after gathering"))?;

        debug!(
            "Offer created, SDP len: {}, ice_gathering: {:?}",
            local_desc.sdp.len(),
            self.pc.ice_gathering_state()
        );

        // Return offer as JSON
        let offer_json = serde_json::json!({
            "type": local_desc.sdp_type.to_string().to_lowercase(),
            "sdp": local_desc.sdp
        });

        Ok(offer_json)
    }

    /// Handle incoming offer and create answer
    pub async fn handle_offer(&self, offer: serde_json::Value) -> Result<serde_json::Value> {
        let sdp = offer
            .get("sdp")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing SDP in offer"))?;

        // Setup data channel handler BEFORE set_remote_description
        // This ensures the handler is registered before any data channel events fire
        let peer_id = self.peer_id.clone();
        let message_tx = self.message_tx.clone();
        let pending_requests = self.pending_requests.clone();
        let pending_nostr_queries = self.pending_nostr_queries.clone();
        let store = self.store.clone();
        let data_channel_holder = self.data_channel.clone();
        let nostr_relay = self.nostr_relay.clone();
        let mesh_frame_tx = self.mesh_frame_tx.clone();
        let cashu_quotes = self.cashu_quotes.clone();
        let peer_pubkey = Some(self.peer_id.pubkey.clone());

        self.pc
            .on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
                let peer_id = peer_id.clone();
                let message_tx = message_tx.clone();
                let pending_requests = pending_requests.clone();
                let pending_nostr_queries = pending_nostr_queries.clone();
                let store = store.clone();
                let data_channel_holder = data_channel_holder.clone();
                let nostr_relay = nostr_relay.clone();
                let mesh_frame_tx = mesh_frame_tx.clone();
                let cashu_quotes = cashu_quotes.clone();
                let peer_pubkey = peer_pubkey.clone();

                // Work MUST be inside the returned future
                Box::pin(async move {
                    info!(
                        "Peer {} received data channel: {}",
                        peer_id.short(),
                        dc.label()
                    );

                    // Store the received data channel
                    {
                        let mut dc_guard = data_channel_holder.lock().await;
                        *dc_guard = Some(dc.clone());
                    }

                    // Set up message handlers
                    Self::setup_dc_handlers(
                        dc.clone(),
                        peer_id,
                        message_tx,
                        pending_requests,
                        pending_nostr_queries.clone(),
                        store,
                        nostr_relay,
                        mesh_frame_tx,
                        cashu_quotes,
                        peer_pubkey,
                    )
                    .await;
                })
            }));

        // Set remote description after handler is registered
        let offer_desc = RTCSessionDescription::offer(sdp.to_string())?;
        self.pc.set_remote_description(offer_desc).await?;

        // Create answer and wait for ICE gathering to complete
        // This ensures all ICE candidates are embedded in the SDP
        let answer = self.pc.create_answer(None).await?;
        let mut gathering_complete = self.pc.gathering_complete_promise().await;
        self.pc.set_local_description(answer).await?;

        // Wait for ICE gathering to complete (with timeout)
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            gathering_complete.recv(),
        )
        .await;

        // Get the local description with ICE candidates embedded
        let local_desc = self
            .pc
            .local_description()
            .await
            .ok_or_else(|| anyhow::anyhow!("No local description after gathering"))?;

        debug!(
            "Answer created, SDP len: {}, ice_gathering: {:?}",
            local_desc.sdp.len(),
            self.pc.ice_gathering_state()
        );

        let answer_json = serde_json::json!({
            "type": local_desc.sdp_type.to_string().to_lowercase(),
            "sdp": local_desc.sdp
        });

        Ok(answer_json)
    }

    /// Handle incoming answer
    pub async fn handle_answer(&self, answer: serde_json::Value) -> Result<()> {
        let sdp = answer
            .get("sdp")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing SDP in answer"))?;

        let answer_desc = RTCSessionDescription::answer(sdp.to_string())?;
        self.pc.set_remote_description(answer_desc).await?;

        Ok(())
    }

    /// Handle incoming ICE candidate
    pub async fn handle_candidate(&self, candidate: serde_json::Value) -> Result<()> {
        let candidate_str = candidate
            .get("candidate")
            .and_then(|c| c.as_str())
            .unwrap_or("");

        let sdp_mid = candidate
            .get("sdpMid")
            .and_then(|m| m.as_str())
            .map(|s| s.to_string());

        let sdp_mline_index = candidate
            .get("sdpMLineIndex")
            .and_then(|i| i.as_u64())
            .map(|i| i as u16);

        if !candidate_str.is_empty() {
            use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
            let init = RTCIceCandidateInit {
                candidate: candidate_str.to_string(),
                sdp_mid,
                sdp_mline_index,
                username_fragment: candidate
                    .get("usernameFragment")
                    .and_then(|u| u.as_str())
                    .map(|s| s.to_string()),
            };
            self.pc.add_ice_candidate(init).await?;
        }

        Ok(())
    }

    /// Setup data channel handlers
    async fn setup_data_channel(&self, dc: Arc<RTCDataChannel>) -> Result<()> {
        let peer_id = self.peer_id.clone();
        let message_tx = self.message_tx.clone();
        let pending_requests = self.pending_requests.clone();
        let store = self.store.clone();
        let nostr_relay = self.nostr_relay.clone();
        let mesh_frame_tx = self.mesh_frame_tx.clone();
        let cashu_quotes = self.cashu_quotes.clone();
        let peer_pubkey = Some(self.peer_id.pubkey.clone());

        Self::setup_dc_handlers(
            dc,
            peer_id,
            message_tx,
            pending_requests,
            self.pending_nostr_queries.clone(),
            store,
            nostr_relay,
            mesh_frame_tx,
            cashu_quotes,
            peer_pubkey,
        )
        .await;
        Ok(())
    }

    /// Setup handlers for a data channel (shared between outbound and inbound)
    #[allow(clippy::too_many_arguments)]
    async fn setup_dc_handlers(
        dc: Arc<RTCDataChannel>,
        peer_id: PeerId,
        message_tx: mpsc::Sender<(DataMessage, Option<Vec<u8>>)>,
        pending_requests: Arc<Mutex<HashMap<String, PendingRequest>>>,
        pending_nostr_queries: Arc<
            Mutex<HashMap<String, mpsc::UnboundedSender<NostrRelayMessage>>>,
        >,
        store: Option<Arc<dyn ContentStore>>,
        nostr_relay: Option<Arc<NostrRelay>>,
        mesh_frame_tx: Option<mpsc::Sender<(PeerId, MeshNostrFrame)>>,
        cashu_quotes: Option<Arc<CashuQuoteState>>,
        peer_pubkey: Option<String>,
    ) {
        let label = dc.label().to_string();
        let peer_short = peer_id.short();

        // Track pending binary data (request_id -> expected after response)
        let _pending_binary: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));

        let open_notify = nostr_relay.as_ref().map(|_| Arc::new(Notify::new()));
        if let Some(ref notify) = open_notify {
            if dc.ready_state() == RTCDataChannelState::Open {
                // `notify_one` stores a permit if no waiter is active yet.
                notify.notify_one();
            }
        }

        let mut nostr_client_id: Option<u64> = None;
        if let Some(relay) = nostr_relay.clone() {
            let client_id = relay.next_client_id();
            let (nostr_tx, mut nostr_rx) = mpsc::unbounded_channel::<String>();
            relay
                .register_client(client_id, nostr_tx, peer_pubkey.clone())
                .await;
            nostr_client_id = Some(client_id);

            if let Some(notify) = open_notify.clone() {
                let dc_for_send = dc.clone();
                tokio::spawn(async move {
                    notify.notified().await;
                    while let Some(text) = nostr_rx.recv().await {
                        if dc_for_send.send_text(text).await.is_err() {
                            break;
                        }
                    }
                });
            }
        }

        if let (Some(relay), Some(client_id)) = (nostr_relay.clone(), nostr_client_id) {
            dc.on_close(Box::new(move || {
                let relay = relay.clone();
                Box::pin(async move {
                    relay.unregister_client(client_id).await;
                })
            }));
        }

        let open_notify_clone = open_notify.clone();
        let peer_short_open = peer_short.clone();
        let label_clone = label.clone();
        dc.on_open(Box::new(move || {
            let peer_short_open = peer_short_open.clone();
            let label_clone = label_clone.clone();
            let open_notify = open_notify_clone.clone();
            // Work MUST be inside the returned future
            Box::pin(async move {
                info!(
                    "[Peer {}] Data channel '{}' open",
                    peer_short_open, label_clone
                );
                if let Some(notify) = open_notify {
                    notify.notify_one();
                }
            })
        }));

        let dc_for_msg = dc.clone();
        let peer_short_msg = peer_short.clone();
        let _pending_binary_clone = _pending_binary.clone();
        let store_clone = store.clone();
        let nostr_relay_for_msg = nostr_relay.clone();
        let nostr_client_id_for_msg = nostr_client_id;
        let pending_nostr_queries_for_msg = pending_nostr_queries.clone();
        let mesh_frame_tx_for_msg = mesh_frame_tx.clone();
        let peer_id_for_msg = peer_id.clone();

        dc.on_message(Box::new(move |msg: DataChannelMessage| {
            let dc = dc_for_msg.clone();
            let peer_short = peer_short_msg.clone();
            let pending_requests = pending_requests.clone();
            let _pending_binary = _pending_binary_clone.clone();
            let _message_tx = message_tx.clone();
            let store = store_clone.clone();
            let nostr_relay = nostr_relay_for_msg.clone();
            let nostr_client_id = nostr_client_id_for_msg;
            let pending_nostr_queries = pending_nostr_queries_for_msg.clone();
            let mesh_frame_tx = mesh_frame_tx_for_msg.clone();
            let cashu_quotes = cashu_quotes.clone();
            let peer_id = peer_id_for_msg.clone();
            let msg_data = msg.data.clone();

            // Work MUST be inside the returned future
            Box::pin(async move {
                if msg.is_string {
                    if let Ok(text) = std::str::from_utf8(&msg_data) {
                        if let Ok(mesh_frame) = serde_json::from_str::<MeshNostrFrame>(text) {
                            match validate_mesh_frame(&mesh_frame) {
                                Ok(()) => {
                                    if let Some(tx) = mesh_frame_tx {
                                        let _ = tx.send((peer_id.clone(), mesh_frame)).await;
                                    }
                                    return;
                                }
                                Err(reason) => {
                                    debug!(
                                        "[Peer {}] Ignoring invalid mesh frame: {}",
                                        peer_short, reason
                                    );
                                }
                            }
                        }

                        // First, route relay responses to pending local queries.
                        if let Ok(relay_msg) = NostrRelayMessage::from_json(text) {
                            if let Some(sub_id) = relay_subscription_id(&relay_msg) {
                                let sender = {
                                    let pending = pending_nostr_queries.lock().await;
                                    pending.get(&sub_id).cloned()
                                };
                                if let Some(tx) = sender {
                                    debug!(
                                        "[Peer {}] Routed Nostr relay message for subscription {}",
                                        peer_short, sub_id
                                    );
                                    let _ = tx.send(relay_msg);
                                    return;
                                } else {
                                    debug!(
                                        "[Peer {}] Dropping Nostr relay message for unknown subscription {}",
                                        peer_short, sub_id
                                    );
                                }
                            }
                        }

                        // Otherwise treat it as a client message to be handled by local relay.
                        if let Some(relay) = nostr_relay {
                            if let Ok(nostr_msg) = NostrClientMessage::from_json(text) {
                                if let Some(client_id) = nostr_client_id {
                                    relay.handle_client_message(client_id, nostr_msg).await;
                                }
                            }
                        }
                    }
                    return;
                }
                // All messages are binary with type prefix + MessagePack body
                debug!(
                    "[Peer {}] Received {} bytes on data channel",
                    peer_short,
                    msg_data.len()
                );
                match parse_message(&msg_data) {
                    Ok(data_msg) => match data_msg {
                        DataMessage::Request(req) => {
                            let hash_hex = hash_to_hex(&req.h);
                            let hash_short = &hash_hex[..8.min(hash_hex.len())];
                            info!("[Peer {}] Received request for {}", peer_short, hash_short);

                            if let Some(cashu_quotes) = cashu_quotes.as_ref() {
                                if cashu_quotes
                                    .should_refuse_requests_from_peer(&peer_id.to_string())
                                    .await
                                {
                                    info!(
                                        "[Peer {}] Refusing request from peer with unpaid defaults",
                                        peer_short
                                    );
                                    return;
                                }
                            }

                            let quoted_settlement = if let Some(quote_id) = req.q {
                                let Some(cashu_quotes) = cashu_quotes.as_ref() else {
                                    info!(
                                        "[Peer {}] Ignoring quoted request without Cashu settlement state",
                                        peer_short
                                    );
                                    return;
                                };
                                match cashu_quotes
                                    .take_valid_quote(&peer_id.to_string(), &req.h, quote_id)
                                    .await
                                {
                                    Some(settlement) => Some((quote_id, settlement)),
                                    None => {
                                        info!(
                                            "[Peer {}] Ignoring request with invalid or expired quote {}",
                                            peer_short, quote_id
                                        );
                                        return;
                                    }
                                }
                            } else {
                                None
                            };

                            // Handle request - look up in store
                            let data = if let Some(ref store) = store {
                                match store.get(&hash_hex) {
                                    Ok(Some(data)) => {
                                        info!(
                                            "[Peer {}] Found {} in store ({} bytes)",
                                            peer_short,
                                            hash_short,
                                            data.len()
                                        );
                                        Some(data)
                                    }
                                    Ok(None) => {
                                        info!(
                                            "[Peer {}] Hash {} not in store",
                                            peer_short, hash_short
                                        );
                                        None
                                    }
                                    Err(e) => {
                                        warn!("[Peer {}] Store error: {}", peer_short, e);
                                        None
                                    }
                                }
                            } else {
                                warn!(
                                    "[Peer {}] No store configured - cannot serve requests",
                                    peer_short
                                );
                                None
                            };

                            // Send response only if we have data
                            if let Some(data) = data {
                                let data_len = data.len();
                                if let (Some(cashu_quotes), Some((quote_id, settlement))) =
                                    (cashu_quotes.as_ref(), quoted_settlement)
                                {
                                    match cashu_quotes
                                        .prepare_quoted_transfer(
                                            &peer_id.to_string(),
                                            &req.h,
                                            quote_id,
                                            &settlement,
                                            data,
                                        )
                                        .await
                                    {
                                        Some((first_chunk, first_expected)) => {
                                            if send_quoted_chunk(
                                                &dc,
                                                &peer_id,
                                                &peer_short,
                                                cashu_quotes,
                                                first_chunk,
                                                first_expected,
                                            )
                                            .await
                                            {
                                                info!(
                                                    "[Peer {}] Started quoted chunked response for {} ({} bytes)",
                                                    peer_short, hash_short, data_len
                                                );
                                            }
                                        }
                                        None => {
                                            warn!(
                                                "[Peer {}] Failed to prepare quoted transfer for {}",
                                                peer_short, hash_short
                                            );
                                        }
                                    }
                                } else {
                                    let response = DataResponse {
                                        h: req.h,
                                        d: data,
                                        i: None,
                                        n: None,
                                    };
                                    if let Ok(wire) = encode_response(&response) {
                                        if let Err(e) = dc.send(&Bytes::from(wire)).await {
                                            error!(
                                                "[Peer {}] Failed to send response: {}",
                                                peer_short, e
                                            );
                                        } else {
                                            info!(
                                                "[Peer {}] Sent response for {} ({} bytes)",
                                                peer_short, hash_short, data_len
                                            );
                                        }
                                    }
                                }
                            } else {
                                info!("[Peer {}] Content not found for {}", peer_short, hash_short);
                            }
                        }
                        DataMessage::Response(res) => {
                            let hash_hex = hash_to_hex(&res.h);
                            let hash_short = &hash_hex[..8.min(hash_hex.len())];
                            debug!(
                                "[Peer {}] Received response for {} ({} bytes)",
                                peer_short,
                                hash_short,
                                res.d.len()
                            );

                            // Resolve the pending request by hash
                            let mut pending = pending_requests.lock().await;
                            if let Some(req) = pending.remove(&hash_hex) {
                                let _ = req.response_tx.send(Some(res.d));
                            }
                        }
                        DataMessage::QuoteRequest(req) => {
                            let response = handle_quote_request_message(
                                &peer_short,
                                &peer_id,
                                &store,
                                cashu_quotes.as_ref(),
                                &req,
                            )
                            .await;
                            if let Some(response) = response {
                                if let Ok(wire) = encode_quote_response(&response) {
                                    if let Err(e) = dc.send(&Bytes::from(wire)).await {
                                        warn!(
                                            "[Peer {}] Failed to send quote response: {}",
                                            peer_short, e
                                        );
                                    }
                                }
                            }
                        }
                        DataMessage::QuoteResponse(res) => {
                            if let Some(cashu_quotes) = cashu_quotes.as_ref() {
                                let _ = cashu_quotes
                                    .handle_quote_response(&peer_id.to_string(), res)
                                    .await;
                            }
                        }
                        DataMessage::Chunk(chunk) => {
                            process_chunk_message(
                                &peer_short,
                                &peer_id,
                                &dc,
                                &pending_requests,
                                cashu_quotes.as_ref(),
                                chunk,
                            )
                            .await;
                        }
                        DataMessage::Payment(req) => {
                            let outcome =
                                handle_payment_message(&peer_id, cashu_quotes.as_ref(), &req).await;
                            if let Ok(wire) = encode_payment_ack(&outcome.ack) {
                                if let Err(e) = dc.send(&Bytes::from(wire)).await {
                                    warn!(
                                        "[Peer {}] Failed to send payment ack: {}",
                                        peer_short, e
                                    );
                                }
                            }
                            if let (Some(cashu_quotes), Some((next_chunk, next_expected))) =
                                (cashu_quotes.as_ref(), outcome.next_chunk)
                            {
                                let _ = send_quoted_chunk(
                                    &dc,
                                    &peer_id,
                                    &peer_short,
                                    cashu_quotes,
                                    next_chunk,
                                    next_expected,
                                )
                                .await;
                            }
                        }
                        DataMessage::PaymentAck(res) => {
                            handle_payment_ack_message(
                                &peer_short,
                                &peer_id,
                                &dc,
                                &pending_requests,
                                cashu_quotes.as_ref(),
                                res,
                            )
                            .await;
                        }
                    },
                    Err(e) => {
                        warn!("[Peer {}] Failed to parse message: {:?}", peer_short, e);
                        // Log hex dump of first 50 bytes for debugging
                        let hex_dump: String = msg_data
                            .iter()
                            .take(50)
                            .map(|b| format!("{:02x}", b))
                            .collect();
                        warn!("[Peer {}] Message hex: {}", peer_short, hex_dump);
                    }
                }
            })
        }));
    }

    /// Check if data channel is ready
    pub fn has_data_channel(&self) -> bool {
        // Use try_lock for non-async context
        self.data_channel
            .try_lock()
            .map(|guard| {
                guard
                    .as_ref()
                    .map(|dc| dc.ready_state() == RTCDataChannelState::Open)
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    /// Request content by hash from this peer
    pub async fn request(&self, hash_hex: &str) -> Result<Option<Vec<u8>>> {
        self.request_with_timeout(hash_hex, std::time::Duration::from_secs(10))
            .await
    }

    /// Request content by hash from this peer with an explicit timeout.
    pub async fn request_with_timeout(
        &self,
        hash_hex: &str,
        timeout: std::time::Duration,
    ) -> Result<Option<Vec<u8>>> {
        let dc_guard = self.data_channel.lock().await;
        let dc = dc_guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No data channel"))?
            .clone();
        drop(dc_guard); // Release lock before async operations

        // Convert hex to binary hash
        let hash = hex::decode(hash_hex).map_err(|e| anyhow::anyhow!("Invalid hex hash: {}", e))?;

        // Create response channel
        let (tx, rx) = oneshot::channel();

        // Store pending request (keyed by hash hex)
        {
            let mut pending = self.pending_requests.lock().await;
            pending.insert(
                hash_hex.to_string(),
                PendingRequest::standard(hash.clone(), tx),
            );
        }

        // Send request with blob-request default HTL (fresh request from us)
        let req = DataRequest {
            h: hash,
            htl: BLOB_REQUEST_POLICY.max_htl,
            q: None,
        };
        let wire = encode_request(&req)?;
        dc.send(&Bytes::from(wire)).await?;

        debug!(
            "[Peer {}] Sent request for {}",
            self.peer_id.short(),
            &hash_hex[..8.min(hash_hex.len())]
        );

        // Wait for response with timeout
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(data)) => Ok(data),
            Ok(Err(_)) => {
                // Channel closed
                Ok(None)
            }
            Err(_) => {
                // Timeout - clean up pending request
                let mut pending = self.pending_requests.lock().await;
                pending.remove(hash_hex);
                Ok(None)
            }
        }
    }

    /// Query a peer's embedded Nostr relay over the WebRTC data channel.
    /// Returns all events received before EOSE/timeout.
    pub async fn query_nostr_events(
        &self,
        filters: Vec<NostrFilter>,
        timeout: std::time::Duration,
    ) -> Result<Vec<nostr::Event>> {
        let dc_guard = self.data_channel.lock().await;
        let dc = dc_guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No data channel"))?
            .clone();
        drop(dc_guard);

        let subscription_id = NostrSubscriptionId::generate();
        let subscription_key = subscription_id.to_string();
        let (tx, mut rx) = mpsc::unbounded_channel::<NostrRelayMessage>();

        {
            let mut pending = self.pending_nostr_queries.lock().await;
            pending.insert(subscription_key.clone(), tx);
        }

        let req = NostrClientMessage::req(subscription_id.clone(), filters);
        if let Err(e) = dc.send_text(req.as_json()).await {
            let mut pending = self.pending_nostr_queries.lock().await;
            pending.remove(&subscription_key);
            return Err(e.into());
        }
        debug!(
            "[Peer {}] Sent Nostr REQ subscription {}",
            self.peer_id.short(),
            subscription_id
        );

        let mut events = Vec::new();
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline - now;

            let next = tokio::time::timeout(remaining, rx.recv()).await;
            match next {
                Ok(Some(NostrRelayMessage::Event {
                    subscription_id: sid,
                    event,
                })) if sid == subscription_id => {
                    debug!(
                        "[Peer {}] Received Nostr EVENT for subscription {}",
                        self.peer_id.short(),
                        subscription_id
                    );
                    events.push(*event);
                }
                Ok(Some(NostrRelayMessage::EndOfStoredEvents(sid))) if sid == subscription_id => {
                    debug!(
                        "[Peer {}] Received Nostr EOSE for subscription {}",
                        self.peer_id.short(),
                        subscription_id
                    );
                    break;
                }
                Ok(Some(NostrRelayMessage::Closed {
                    subscription_id: sid,
                    message,
                })) if sid == subscription_id => {
                    warn!(
                        "[Peer {}] Nostr query closed for subscription {}: {}",
                        self.peer_id.short(),
                        subscription_id,
                        message
                    );
                    break;
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => {
                    warn!(
                        "[Peer {}] Nostr query timed out for subscription {}",
                        self.peer_id.short(),
                        subscription_id
                    );
                    break;
                }
            }
        }

        let close = NostrClientMessage::close(subscription_id.clone());
        let _ = dc.send_text(close.as_json()).await;

        let mut pending = self.pending_nostr_queries.lock().await;
        pending.remove(&subscription_key);
        debug!(
            "[Peer {}] Nostr query subscription {} collected {} event(s)",
            self.peer_id.short(),
            subscription_id,
            events.len()
        );

        Ok(events)
    }

    /// Send a mesh signaling frame as text over the data channel.
    pub async fn send_mesh_frame_text(&self, frame: &MeshNostrFrame) -> Result<()> {
        let dc_guard = self.data_channel.lock().await;
        let dc = dc_guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No data channel"))?
            .clone();
        drop(dc_guard);

        let text = serde_json::to_string(frame)?;
        dc.send_text(text).await?;
        Ok(())
    }

    /// Send a message over the data channel
    pub async fn send_message(&self, msg: &DataMessage) -> Result<()> {
        let dc_guard = self.data_channel.lock().await;
        if let Some(ref dc) = *dc_guard {
            let wire = encode_message(msg)?;
            dc.send(&Bytes::from(wire)).await?;
        }
        Ok(())
    }

    /// Close the connection
    pub async fn close(&self) -> Result<()> {
        {
            let dc_guard = self.data_channel.lock().await;
            if let Some(ref dc) = *dc_guard {
                dc.close().await?;
            }
        }
        self.pc.close().await?;
        Ok(())
    }
}

fn relay_subscription_id(msg: &NostrRelayMessage) -> Option<String> {
    match msg {
        NostrRelayMessage::Event {
            subscription_id, ..
        } => Some(subscription_id.to_string()),
        NostrRelayMessage::EndOfStoredEvents(subscription_id) => Some(subscription_id.to_string()),
        NostrRelayMessage::Closed {
            subscription_id, ..
        } => Some(subscription_id.to_string()),
        NostrRelayMessage::Count {
            subscription_id, ..
        } => Some(subscription_id.to_string()),
        _ => None,
    }
}

#[async_trait]
impl RoutedPeerLink for Peer {
    async fn send(&self, data: Vec<u8>) -> std::result::Result<(), RoutedTransportError> {
        let dc = self
            .data_channel
            .lock()
            .await
            .as_ref()
            .cloned()
            .ok_or(RoutedTransportError::NotConnected)?;
        dc.send(&Bytes::from(data))
            .await
            .map(|_| ())
            .map_err(|e| RoutedTransportError::SendFailed(e.to_string()))
    }

    async fn recv(&self) -> Option<Vec<u8>> {
        None
    }

    fn try_recv(&self) -> Option<Vec<u8>> {
        None
    }

    fn is_open(&self) -> bool {
        self.has_data_channel()
    }

    async fn close(&self) {
        let _ = Peer::close(self).await;
    }
}
