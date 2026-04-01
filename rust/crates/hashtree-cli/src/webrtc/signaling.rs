//! WebRTC signaling over Nostr relays
//!
//! Protocol (compatible with hashtree-ts):
//! - All signaling uses ephemeral kind 25050
//! - Hello messages: #l: "hello" tag, broadcast for peer discovery (unencrypted)
//! - Directed signaling (offer, answer, candidate, candidates): NIP-17 style
//!   gift wrap for privacy - wrapped with ephemeral key, #p tag with recipient
//!
//! Security: Directed messages use gift wrapping with ephemeral keys so that
//! relays cannot see the actual sender or correlate messages.

use anyhow::Result;
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use hashtree_network::{
    decode_signaling_event, encode_signaling_event, run_hedged_waves, sync_selector_peers,
    ClassifyRequest as SharedClassifyRequest, HedgedWaveAction, IceCandidate as SharedIceCandidate,
    MeshRouter, PeerLink as SharedPeerLink, PeerLinkFactory as SharedPeerLinkFactory, PeerSelector,
    SignalingTransport as SharedSignalingTransport, TransportError as SharedTransportError,
};
use nostr::{ClientMessage, Filter, JsonUtil, Keys, Kind, RelayMessage};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use super::bluetooth::{BluetoothMesh, BluetoothPeerRegistrar, BluetoothRuntimeContext};
use super::cashu::{CashuMintMetadataStore, CashuQuoteState, CashuRoutingConfig, NegotiatedQuote};
use super::local_bus::SharedLocalNostrBus;
use super::multicast::MulticastNostrBus;
use super::peer::{ContentStore, Peer, PendingRequest};
use super::root_events::{
    build_root_filter, hashtree_event_identifier, is_hashtree_labeled_event, pick_latest_event,
    root_event_from_peer, PeerRootEvent,
};
use super::session::MeshPeer;
use super::types::{
    decrement_htl_with_policy, encode_quote_request, encode_request, should_forward_htl,
    validate_mesh_frame, DataQuoteRequest, DataRequest, MeshNostrFrame, MeshNostrPayload,
    PeerDirection, PeerId, PeerPool, PeerStateEvent, PeerStatus, RequestDispatchConfig,
    SignalingMessage, TimedSeenSet, WebRTCConfig, HELLO_TAG, MESH_DEFAULT_HTL, MESH_EVENT_POLICY,
    WEBRTC_KIND,
};
use super::wifi_aware::{mobile_wifi_aware_bridge, WifiAwareNostrBus, WIFI_AWARE_SOURCE};
use crate::cashu_helper::CashuPaymentClient;
use crate::nostr_relay::NostrRelay;

/// Callback type for classifying peers into pools
pub type PeerClassifier = Arc<dyn Fn(&str) -> PeerPool + Send + Sync>;

/// Active data transport used for a peer session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PeerTransport {
    WebRtc,
    Bluetooth,
}

impl PeerTransport {
    pub const fn as_str(self) -> &'static str {
        match self {
            PeerTransport::WebRtc => "webrtc",
            PeerTransport::Bluetooth => "bluetooth",
        }
    }
}

impl std::fmt::Display for PeerTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str((*self).as_str())
    }
}

fn bluetooth_nostr_only_mode() -> bool {
    matches!(
        std::env::var("HTREE_BLUETOOTH_NOSTR_ONLY").ok().as_deref(),
        Some("1" | "true" | "TRUE" | "yes" | "YES")
    )
}

/// Signaling/discovery path through which a peer was seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PeerSignalPath {
    Relay,
    Multicast,
    WifiAware,
    Bluetooth,
}

impl PeerSignalPath {
    pub const fn as_str(self) -> &'static str {
        match self {
            PeerSignalPath::Relay => "relay",
            PeerSignalPath::Multicast => "multicast",
            PeerSignalPath::WifiAware => WIFI_AWARE_SOURCE,
            PeerSignalPath::Bluetooth => "bluetooth",
        }
    }

    pub fn from_source_name(source: &str) -> Self {
        match source {
            "multicast" => PeerSignalPath::Multicast,
            WIFI_AWARE_SOURCE => PeerSignalPath::WifiAware,
            "bluetooth" => PeerSignalPath::Bluetooth,
            _ => PeerSignalPath::Relay,
        }
    }
}

impl std::fmt::Display for PeerSignalPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str((*self).as_str())
    }
}

/// Connection state for a peer
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionState {
    Discovered,
    Connecting,
    Connected,
    Failed,
}

impl std::fmt::Display for ConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionState::Discovered => write!(f, "discovered"),
            ConnectionState::Connecting => write!(f, "connecting"),
            ConnectionState::Connected => write!(f, "connected"),
            ConnectionState::Failed => write!(f, "failed"),
        }
    }
}

/// Peer entry in the manager
pub struct PeerEntry {
    pub peer_id: PeerId,
    pub direction: PeerDirection,
    pub state: ConnectionState,
    pub last_seen: Instant,
    pub peer: Option<MeshPeer>,
    pub pool: PeerPool,
    pub transport: PeerTransport,
    pub signal_paths: BTreeSet<PeerSignalPath>,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

/// Shared state for the native mesh router.
pub struct WebRTCState {
    pub peers: RwLock<HashMap<String, PeerEntry>>,
    pub connected_count: std::sync::atomic::AtomicUsize,
    /// Total bytes sent across all peers (cumulative)
    pub bytes_sent: std::sync::atomic::AtomicU64,
    /// Total bytes received across all peers (cumulative)
    pub bytes_received: std::sync::atomic::AtomicU64,
    /// Relayless mesh frames received and accepted.
    pub mesh_received: std::sync::atomic::AtomicU64,
    /// Relayless mesh frames forwarded to peers.
    pub mesh_forwarded: std::sync::atomic::AtomicU64,
    /// Relayless mesh frames/events dropped due to dedupe.
    pub mesh_dropped_duplicate: std::sync::atomic::AtomicU64,
    /// Shared peer selector used by live retrieval; aligned with simulation strategies.
    peer_selector: Arc<RwLock<PeerSelector>>,
    /// Hedged dispatch policy for retrieval requests.
    request_dispatch: RequestDispatchConfig,
    /// Retrieval timeout for quote negotiation and single-peer fetches.
    request_timeout: Duration,
    /// Shared Cashu quote negotiation policy/state.
    cashu_quotes: Arc<CashuQuoteState>,
    /// Optional local buses such as multicast or BLE that carry signed Nostr
    /// envelopes for nearby/offline peers.
    local_buses: RwLock<Vec<SharedLocalNostrBus>>,
}
const SEEN_FRAME_CAP: usize = 4096;
const SEEN_FRAME_TTL: Duration = Duration::from_secs(120);
const SEEN_EVENT_CAP: usize = 8192;
const SEEN_EVENT_TTL: Duration = Duration::from_secs(600);

type PendingRequestsMap = Arc<Mutex<HashMap<String, PendingRequest>>>;
type ConnectedPeer = (
    String,
    PendingRequestsMap,
    Arc<webrtc::data_channel::RTCDataChannel>,
);
type ConnectedSession = (String, MeshPeer, PeerTransport);
type SharedProductionRouter = MeshRouter<RouterSignalingBridge, SharedRouterPeerFactory>;

async fn remember_peer_signal_path(state: &WebRTCState, peer_id: &str, source: &str) {
    if let Some(entry) = state.peers.write().await.get_mut(peer_id) {
        entry
            .signal_paths
            .insert(PeerSignalPath::from_source_name(source));
    }
}

#[derive(Clone)]
struct RouterSignalingBridge {
    peer_id: String,
    signaling_tx: mpsc::Sender<SignalingMessage>,
}

impl RouterSignalingBridge {
    fn new(peer_id: String, signaling_tx: mpsc::Sender<SignalingMessage>) -> Self {
        Self {
            peer_id,
            signaling_tx,
        }
    }
}

#[async_trait]
impl SharedSignalingTransport for RouterSignalingBridge {
    async fn connect(&self, _relays: &[String]) -> Result<(), SharedTransportError> {
        Ok(())
    }

    async fn disconnect(&self) {}

    async fn publish(&self, msg: SignalingMessage) -> Result<(), SharedTransportError> {
        self.signaling_tx
            .send(msg)
            .await
            .map_err(|e| SharedTransportError::SendFailed(e.to_string()))
    }

    async fn recv(&self) -> Option<SignalingMessage> {
        None
    }

    fn try_recv(&self) -> Option<SignalingMessage> {
        None
    }

    fn peer_id(&self) -> &str {
        &self.peer_id
    }
}

struct SharedRouterPeerFactory {
    my_peer_id: PeerId,
    signaling_tx: mpsc::Sender<SignalingMessage>,
    stun_servers: Vec<String>,
    store: Option<Arc<dyn ContentStore>>,
    state: Arc<WebRTCState>,
    state_event_tx: mpsc::Sender<PeerStateEvent>,
    nostr_relay: Option<Arc<NostrRelay>>,
    mesh_frame_tx: mpsc::Sender<(PeerId, MeshNostrFrame)>,
    peer_classifier: PeerClassifier,
    peers: RwLock<HashMap<String, Arc<Peer>>>,
}

impl SharedRouterPeerFactory {
    fn new(
        my_peer_id: PeerId,
        signaling_tx: mpsc::Sender<SignalingMessage>,
        stun_servers: Vec<String>,
        store: Option<Arc<dyn ContentStore>>,
        state: Arc<WebRTCState>,
        state_event_tx: mpsc::Sender<PeerStateEvent>,
        nostr_relay: Option<Arc<NostrRelay>>,
        mesh_frame_tx: mpsc::Sender<(PeerId, MeshNostrFrame)>,
        peer_classifier: PeerClassifier,
    ) -> Self {
        Self {
            my_peer_id,
            signaling_tx,
            stun_servers,
            store,
            state,
            state_event_tx,
            nostr_relay,
            mesh_frame_tx,
            peer_classifier,
            peers: RwLock::new(HashMap::new()),
        }
    }

    async fn register_peer(&self, peer_id: PeerId, direction: PeerDirection, peer: Arc<Peer>) {
        let peer_key = peer_id.to_string();
        let pool = (self.peer_classifier)(&peer_id.pubkey);
        self.peers
            .write()
            .await
            .insert(peer_key.clone(), peer.clone());

        let mut peers = self.state.peers.write().await;
        peers.insert(
            peer_key,
            PeerEntry {
                peer_id,
                direction,
                state: ConnectionState::Connecting,
                last_seen: Instant::now(),
                peer: Some(MeshPeer::WebRtc(peer)),
                pool,
                transport: PeerTransport::WebRtc,
                signal_paths: BTreeSet::from([PeerSignalPath::Relay]),
                bytes_sent: 0,
                bytes_received: 0,
            },
        );
    }

    async fn create_peer(
        &self,
        peer_id: PeerId,
        direction: PeerDirection,
    ) -> Result<Peer, SharedTransportError> {
        Peer::new_with_store_and_events(
            peer_id,
            direction,
            self.my_peer_id.clone(),
            self.signaling_tx.clone(),
            self.stun_servers.clone(),
            self.store.clone(),
            Some(self.state_event_tx.clone()),
            self.nostr_relay.clone(),
            Some(self.mesh_frame_tx.clone()),
            Some(self.state.cashu_quotes.clone()),
        )
        .await
        .map_err(|e| SharedTransportError::ConnectionFailed(e.to_string()))
    }
}

#[async_trait]
impl SharedPeerLinkFactory for SharedRouterPeerFactory {
    async fn create_offer(
        &self,
        target_peer_id: &str,
    ) -> Result<(Arc<dyn SharedPeerLink>, String), SharedTransportError> {
        let target_peer = PeerId::from_string(target_peer_id).ok_or_else(|| {
            SharedTransportError::ConnectionFailed(format!("invalid peer id {target_peer_id}"))
        })?;
        let peer = Arc::new(
            self.create_peer(target_peer.clone(), PeerDirection::Outbound)
                .await?,
        );
        peer.setup_handlers()
            .await
            .map_err(|e| SharedTransportError::ConnectionFailed(e.to_string()))?;
        let offer = peer
            .connect()
            .await
            .map_err(|e| SharedTransportError::ConnectionFailed(e.to_string()))?;
        let sdp = offer
            .get("sdp")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                SharedTransportError::ConnectionFailed("missing SDP in CLI peer offer".to_string())
            })?
            .to_string();
        self.register_peer(target_peer, PeerDirection::Outbound, peer.clone())
            .await;
        Ok((peer as Arc<dyn SharedPeerLink>, sdp))
    }

    async fn accept_offer(
        &self,
        from_peer_id: &str,
        offer_sdp: &str,
    ) -> Result<(Arc<dyn SharedPeerLink>, String), SharedTransportError> {
        let from_peer = PeerId::from_string(from_peer_id).ok_or_else(|| {
            SharedTransportError::ConnectionFailed(format!("invalid peer id {from_peer_id}"))
        })?;
        let peer = Arc::new(
            self.create_peer(from_peer.clone(), PeerDirection::Inbound)
                .await?,
        );
        peer.setup_handlers()
            .await
            .map_err(|e| SharedTransportError::ConnectionFailed(e.to_string()))?;
        let answer = peer
            .handle_offer(serde_json::json!({ "type": "offer", "sdp": offer_sdp }))
            .await
            .map_err(|e| SharedTransportError::ConnectionFailed(e.to_string()))?;
        let sdp = answer
            .get("sdp")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                SharedTransportError::ConnectionFailed("missing SDP in CLI peer answer".to_string())
            })?
            .to_string();
        self.register_peer(from_peer, PeerDirection::Inbound, peer.clone())
            .await;
        Ok((peer as Arc<dyn SharedPeerLink>, sdp))
    }

    async fn handle_answer(
        &self,
        target_peer_id: &str,
        answer_sdp: &str,
    ) -> Result<Arc<dyn SharedPeerLink>, SharedTransportError> {
        let peer = self
            .peers
            .read()
            .await
            .get(target_peer_id)
            .cloned()
            .ok_or_else(|| {
                SharedTransportError::ConnectionFailed(format!(
                    "missing outbound peer for {target_peer_id}"
                ))
            })?;
        peer.handle_answer(serde_json::json!({ "type": "answer", "sdp": answer_sdp }))
            .await
            .map_err(|e| SharedTransportError::ConnectionFailed(e.to_string()))?;
        Ok(peer as Arc<dyn SharedPeerLink>)
    }

    async fn handle_candidate(
        &self,
        peer_id: &str,
        candidate: SharedIceCandidate,
    ) -> Result<(), SharedTransportError> {
        let peer = self.peers.read().await.get(peer_id).cloned();
        if let Some(peer) = peer {
            peer.handle_candidate(serde_json::json!({
                "candidate": candidate.candidate,
                "sdpMLineIndex": candidate.sdp_m_line_index,
                "sdpMid": candidate.sdp_mid,
            }))
            .await
            .map_err(|e| SharedTransportError::ConnectionFailed(e.to_string()))?;
        }
        Ok(())
    }

    async fn remove_peer(&self, peer_id: &str) -> Result<(), SharedTransportError> {
        self.peers.write().await.remove(peer_id);
        Ok(())
    }
}

impl WebRTCState {
    pub fn new() -> Self {
        let cfg = WebRTCConfig::default();
        Self::new_with_routing_and_cashu(
            cfg.request_selection_strategy,
            cfg.request_fairness_enabled,
            cfg.request_dispatch,
            Duration::from_millis(cfg.message_timeout_ms),
            CashuRoutingConfig::default(),
            None,
            None,
        )
    }

    pub fn new_with_routing(
        selection_strategy: super::types::SelectionStrategy,
        fairness_enabled: bool,
        request_dispatch: RequestDispatchConfig,
    ) -> Self {
        let cfg = WebRTCConfig::default();
        Self::new_with_routing_and_cashu(
            selection_strategy,
            fairness_enabled,
            request_dispatch,
            Duration::from_millis(cfg.message_timeout_ms),
            CashuRoutingConfig::default(),
            None,
            None,
        )
    }

    pub fn new_with_routing_and_cashu(
        selection_strategy: super::types::SelectionStrategy,
        fairness_enabled: bool,
        request_dispatch: RequestDispatchConfig,
        request_timeout: Duration,
        cashu_routing: CashuRoutingConfig,
        payment_client: Option<Arc<dyn CashuPaymentClient>>,
        mint_metadata: Option<Arc<CashuMintMetadataStore>>,
    ) -> Self {
        let mut selector = PeerSelector::with_strategy(selection_strategy);
        selector.set_fairness(fairness_enabled);
        let peer_selector = Arc::new(RwLock::new(selector));
        let cashu_quotes = Arc::new(if let Some(mint_metadata) = mint_metadata {
            CashuQuoteState::new_with_mint_metadata(
                cashu_routing,
                peer_selector.clone(),
                payment_client,
                mint_metadata,
            )
        } else {
            CashuQuoteState::new(cashu_routing, peer_selector.clone(), payment_client)
        });
        Self {
            peers: RwLock::new(HashMap::new()),
            connected_count: std::sync::atomic::AtomicUsize::new(0),
            bytes_sent: std::sync::atomic::AtomicU64::new(0),
            bytes_received: std::sync::atomic::AtomicU64::new(0),
            mesh_received: std::sync::atomic::AtomicU64::new(0),
            mesh_forwarded: std::sync::atomic::AtomicU64::new(0),
            mesh_dropped_duplicate: std::sync::atomic::AtomicU64::new(0),
            peer_selector,
            request_dispatch,
            request_timeout,
            cashu_quotes,
            local_buses: RwLock::new(Vec::new()),
        }
    }

    pub async fn set_local_buses(&self, buses: Vec<SharedLocalNostrBus>) {
        *self.local_buses.write().await = buses;
    }

    pub async fn add_local_bus(&self, bus: SharedLocalNostrBus) {
        self.local_buses.write().await.push(bus);
    }

    pub async fn set_multicast_bus(&self, bus: Option<Arc<MulticastNostrBus>>) {
        let buses = bus
            .into_iter()
            .map(|bus| bus as SharedLocalNostrBus)
            .collect();
        self.set_local_buses(buses).await;
    }

    /// Drop all live peer sessions and clear topology-specific state while
    /// keeping cumulative bandwidth counters intact.
    pub async fn reset_runtime_state(&self) {
        self.set_local_buses(Vec::new()).await;
        let peers = {
            let mut peers = self.peers.write().await;
            std::mem::take(&mut *peers)
        };
        self.connected_count
            .store(0, std::sync::atomic::Ordering::Relaxed);
        for entry in peers.into_values() {
            if let Some(peer) = entry.peer {
                let _ = peer.close().await;
            }
        }
    }

    /// Get current bandwidth stats (bytes sent/received)
    pub fn get_bandwidth(&self) -> (u64, u64) {
        (
            self.bytes_sent.load(std::sync::atomic::Ordering::Relaxed),
            self.bytes_received
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    pub fn get_mesh_stats(&self) -> (u64, u64, u64) {
        (
            self.mesh_received
                .load(std::sync::atomic::Ordering::Relaxed),
            self.mesh_forwarded
                .load(std::sync::atomic::Ordering::Relaxed),
            self.mesh_dropped_duplicate
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    pub fn record_mesh_received(&self) {
        self.mesh_received
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn record_mesh_forwarded(&self, count: u64) {
        self.mesh_forwarded
            .fetch_add(count, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn record_mesh_duplicate_drop(&self) {
        self.mesh_dropped_duplicate
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Record bytes sent (global + per-peer)
    pub async fn record_sent(&self, peer_id: &str, bytes: u64) {
        self.bytes_sent
            .fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
        if let Some(entry) = self.peers.write().await.get_mut(peer_id) {
            entry.bytes_sent += bytes;
        }
    }

    /// Record bytes received (global + per-peer)
    pub async fn record_received(&self, peer_id: &str, bytes: u64) {
        self.bytes_received
            .fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
        if let Some(entry) = self.peers.write().await.get_mut(peer_id) {
            entry.bytes_received += bytes;
        }
    }

    /// Request content by hash from connected peers
    /// Queries peers in adaptive selector order with hedged fanout waves.
    /// Returns the first successful response, or None if no peer has it
    pub async fn request_from_peers(&self, hash_hex: &str) -> Option<Vec<u8>> {
        self.request_from_peers_with_source(hash_hex)
            .await
            .map(|(data, _peer_id)| data)
    }

    /// Request content by hash from connected peers, returning data and source peer.
    pub async fn request_from_peers_with_source(
        &self,
        hash_hex: &str,
    ) -> Option<(Vec<u8>, String)> {
        use super::types::BLOB_REQUEST_POLICY;

        let peers = self.peers.read().await;

        let peer_refs: Vec<_> = peers
            .values()
            .filter(|p| p.state == ConnectionState::Connected && p.peer.is_some())
            .filter_map(|p| {
                p.peer
                    .clone()
                    .map(|peer| (p.peer_id.to_string(), peer, p.transport))
            })
            .collect();

        drop(peers); // Release the read lock

        let mut connected_peers: Vec<ConnectedPeer> = Vec::new();
        let mut connected_sessions: Vec<ConnectedSession> = Vec::new();
        for (peer_id, peer, transport) in peer_refs {
            if !peer.is_ready() {
                continue;
            }
            if bluetooth_nostr_only_mode() && transport == PeerTransport::Bluetooth {
                continue;
            }
            if let Some(webrtc_peer) = peer.as_webrtc() {
                let dc_guard = webrtc_peer.data_channel.lock().await;
                if let Some(dc) = dc_guard.as_ref() {
                    connected_peers.push((
                        peer_id.clone(),
                        webrtc_peer.pending_requests.clone(),
                        dc.clone(),
                    ));
                }
            }
            connected_sessions.push((peer_id, peer, transport));
        }

        if connected_sessions.is_empty() {
            debug!(
                "No connected peers to query for {}",
                &hash_hex[..8.min(hash_hex.len())]
            );
            return None;
        }

        // Convert hex to binary hash once
        let hash_bytes = match hex::decode(hash_hex) {
            Ok(b) => b,
            Err(_) => return None,
        };

        let expected_hash: [u8; 32] = match hash_bytes.as_slice().try_into() {
            Ok(h) => h,
            Err(_) => {
                debug!(
                    "Invalid hash length {}, expected 32 bytes",
                    hash_bytes.len()
                );
                return None;
            }
        };

        let connected_peer_ids: Vec<String> = connected_sessions
            .iter()
            .map(|(peer_id, _, _)| peer_id.clone())
            .collect();
        sync_selector_peers(self.peer_selector.as_ref(), &connected_peer_ids).await;

        let ordered_peer_ids = self.peer_selector.write().await.select_peers();
        let mut quote_by_peer: HashMap<
            String,
            (
                PendingRequestsMap,
                Arc<webrtc::data_channel::RTCDataChannel>,
            ),
        > = connected_peers
            .iter()
            .cloned()
            .map(|(peer_id, pending, dc)| (peer_id, (pending, dc)))
            .collect();
        let mut ordered_quote_peers: Vec<ConnectedPeer> = Vec::new();
        for peer_id in &ordered_peer_ids {
            if let Some((pending, dc)) = quote_by_peer.remove(peer_id) {
                ordered_quote_peers.push((peer_id.clone(), pending, dc));
            }
        }
        for (peer_id, (pending, dc)) in quote_by_peer {
            ordered_quote_peers.push((peer_id, pending, dc));
        }

        let mut by_peer: HashMap<String, (MeshPeer, PeerTransport)> = connected_sessions
            .into_iter()
            .map(|(peer_id, peer, transport)| (peer_id, (peer, transport)))
            .collect();

        let mut ordered_peers: Vec<ConnectedSession> = Vec::new();
        for peer_id in ordered_peer_ids {
            if let Some((peer, transport)) = by_peer.remove(&peer_id) {
                ordered_peers.push((peer_id, peer, transport));
            }
        }
        for (peer_id, (peer, transport)) in by_peer {
            ordered_peers.push((peer_id, peer, transport));
        }

        debug!(
            "Querying {} peers for {} with shared hedged scheduler",
            ordered_peers.len(),
            &hash_hex[..8.min(hash_hex.len())],
        );

        if let Some((requested_mint, payment_sat, quote_ttl_ms)) =
            self.cashu_quotes.requester_quote_terms().await
        {
            if let Some(quote) = self
                .request_quote_from_peers(
                    &hash_bytes,
                    requested_mint,
                    payment_sat,
                    quote_ttl_ms,
                    &ordered_quote_peers,
                )
                .await
            {
                if let Some(data) = self
                    .request_from_single_peer(
                        hash_hex,
                        &hash_bytes,
                        expected_hash,
                        &quote.peer_id,
                        Some(&quote),
                        &ordered_quote_peers,
                    )
                    .await
                {
                    debug!(
                        "Got quoted response from peer {} for {}",
                        quote.peer_id,
                        &hash_hex[..8.min(hash_hex.len())]
                    );
                    return Some((data, quote.peer_id));
                }
            }
        }

        let request = DataRequest {
            h: hash_bytes.clone(),
            htl: BLOB_REQUEST_POLICY.max_htl,
            q: None,
        };
        let wire = match encode_request(&request) {
            Ok(w) => w,
            Err(_) => return None,
        };
        let wire_len = wire.len() as u64;
        let current_result_rx = Arc::new(Mutex::new(None));
        if let Some((data, peer_id)) = run_hedged_waves(
            ordered_peers.len(),
            self.request_dispatch,
            self.request_timeout,
            |range| {
                let wave_peers = ordered_peers[range].to_vec();
                let (result_tx, result_rx) =
                    mpsc::channel::<(String, Instant, Result<Option<Vec<u8>>>)>(wave_peers.len());
                let current_result_rx = current_result_rx.clone();
                let hash_hex = hash_hex.to_string();
                async move {
                    *current_result_rx.lock().await = Some(result_rx);
                    let sent = wave_peers.len();
                    for (peer_id, peer, transport) in wave_peers {
                        if transport != PeerTransport::Bluetooth {
                            self.record_sent(&peer_id, wire_len).await;
                        }
                        self.peer_selector
                            .write()
                            .await
                            .record_request(&peer_id, wire_len);

                        let result_tx = result_tx.clone();
                        let peer_id_for_task = peer_id.clone();
                        let peer = peer.clone();
                        let hash_hex = hash_hex.clone();
                        let per_request_timeout = self.request_timeout;
                        tokio::spawn(async move {
                            let started = Instant::now();
                            let result = peer.request(&hash_hex, per_request_timeout).await;
                            let _ = result_tx.send((peer_id_for_task, started, result)).await;
                        });
                    }
                    drop(result_tx);
                    sent
                }
            },
            |wait| {
                let current_result_rx = current_result_rx.clone();
                async move {
                    let mut current_result_rx = current_result_rx.lock().await;
                    let Some(result_rx) = current_result_rx.as_mut() else {
                        return HedgedWaveAction::Abort;
                    };
                    let deadline = Instant::now() + wait;
                    loop {
                        let now = Instant::now();
                        if now >= deadline {
                            return HedgedWaveAction::Continue;
                        }
                        let remaining = deadline.saturating_duration_since(now);
                        match tokio::time::timeout(remaining, result_rx.recv()).await {
                            Ok(Some((peer_id, started, Ok(Some(data))))) => {
                                let rtt_ms = started.elapsed().as_millis() as u64;
                                if hashtree_core::sha256(&data) == expected_hash {
                                    let should_record = {
                                        let peers = self.peers.read().await;
                                        peers
                                            .get(&peer_id)
                                            .map(|entry| {
                                                entry.transport != PeerTransport::Bluetooth
                                            })
                                            .unwrap_or(true)
                                    };
                                    if should_record {
                                        self.record_received(&peer_id, data.len() as u64).await;
                                    }
                                    self.peer_selector.write().await.record_success(
                                        &peer_id,
                                        rtt_ms,
                                        data.len() as u64,
                                    );
                                    return HedgedWaveAction::Success((data, peer_id));
                                }
                                self.peer_selector.write().await.record_failure(&peer_id);
                            }
                            Ok(Some((peer_id, _, Ok(None)))) | Ok(Some((peer_id, _, Err(_)))) => {
                                self.peer_selector.write().await.record_timeout(&peer_id);
                            }
                            Ok(None) | Err(_) => return HedgedWaveAction::Continue,
                        }
                    }
                }
            },
        )
        .await
        {
            debug!(
                "Got response from peer {} for {}",
                peer_id,
                &hash_hex[..8.min(hash_hex.len())]
            );
            return Some((data, peer_id));
        }

        debug!(
            "No peer had data for {}",
            &hash_hex[..8.min(hash_hex.len())]
        );
        None
    }

    async fn request_quote_from_peers(
        &self,
        hash_bytes: &[u8],
        requested_mint: String,
        payment_sat: u64,
        quote_ttl_ms: u32,
        ordered_peers: &[ConnectedPeer],
    ) -> Option<NegotiatedQuote> {
        if ordered_peers.is_empty() || quote_ttl_ms == 0 {
            return None;
        }

        let hash_hex = hex::encode(hash_bytes);
        let rx = self
            .cashu_quotes
            .register_pending_quote(hash_hex.clone(), Some(requested_mint.clone()), payment_sat)
            .await;
        let quote_request = DataQuoteRequest {
            h: hash_bytes.to_vec(),
            p: payment_sat,
            t: quote_ttl_ms,
            m: Some(requested_mint),
        };
        let wire = match encode_quote_request(&quote_request) {
            Ok(wire) => wire,
            Err(_) => {
                self.cashu_quotes.clear_pending_quote(&hash_hex).await;
                return None;
            }
        };
        let rx = Arc::new(Mutex::new(rx));
        let result = run_hedged_waves(
            ordered_peers.len(),
            self.request_dispatch,
            self.request_timeout,
            |range| {
                let wave_peers = ordered_peers[range].to_vec();
                let wire = wire.clone();
                async move {
                    let mut sent = 0usize;
                    for (_, _, dc) in wave_peers {
                        if dc.send(&bytes::Bytes::copy_from_slice(&wire)).await.is_ok() {
                            sent += 1;
                        }
                    }
                    sent
                }
            },
            |wait| {
                let rx = rx.clone();
                async move {
                    let mut rx = rx.lock().await;
                    match tokio::time::timeout(wait, &mut *rx).await {
                        Ok(Ok(Some(quote))) => HedgedWaveAction::Success(quote),
                        Ok(Ok(None)) | Ok(Err(_)) => HedgedWaveAction::Abort,
                        Err(_) => HedgedWaveAction::Continue,
                    }
                }
            },
        )
        .await;

        self.cashu_quotes.clear_pending_quote(&hash_hex).await;
        result
    }

    async fn request_from_single_peer(
        &self,
        hash_hex: &str,
        hash_bytes: &[u8],
        expected_hash: [u8; 32],
        target_peer_id: &str,
        quote: Option<&NegotiatedQuote>,
        ordered_peers: &[ConnectedPeer],
    ) -> Option<Vec<u8>> {
        use super::types::BLOB_REQUEST_POLICY;

        let (pending_requests, dc) = ordered_peers
            .iter()
            .find(|(peer_id, _, _)| peer_id == target_peer_id)
            .map(|(_, pending_requests, dc)| (pending_requests.clone(), dc.clone()))?;

        let request = DataRequest {
            h: hash_bytes.to_vec(),
            htl: BLOB_REQUEST_POLICY.max_htl,
            q: quote.map(|quote| quote.quote_id),
        };
        let wire = encode_request(&request).ok()?;
        let wire_len = wire.len() as u64;
        let sent_at = Instant::now();
        let (tx, mut rx) = tokio::sync::oneshot::channel();

        {
            let mut pending = pending_requests.lock().await;
            pending.insert(
                hash_hex.to_string(),
                if let Some(quote) = quote {
                    PendingRequest::quoted(
                        hash_bytes.to_vec(),
                        tx,
                        quote.quote_id,
                        quote.mint_url.clone().unwrap_or_default(),
                        quote.payment_sat,
                    )
                } else {
                    PendingRequest::standard(hash_bytes.to_vec(), tx)
                },
            );
        }

        if dc
            .send(&bytes::Bytes::copy_from_slice(&wire))
            .await
            .is_err()
        {
            let mut pending = pending_requests.lock().await;
            pending.remove(hash_hex);
            self.peer_selector
                .write()
                .await
                .record_failure(target_peer_id);
            return None;
        }

        self.record_sent(target_peer_id, wire_len).await;
        self.peer_selector
            .write()
            .await
            .record_request(target_peer_id, wire_len);

        let wait_timeout = if let Some(quote) = quote {
            let multiplier = quote.payment_sat.clamp(1, 32) as u128;
            let extra_ms = self
                .cashu_quotes
                .settlement_timeout()
                .as_millis()
                .saturating_mul(multiplier);
            self.request_timeout + Duration::from_millis(extra_ms.min(u64::MAX as u128) as u64)
        } else {
            self.request_timeout
        };

        match tokio::time::timeout(wait_timeout, &mut rx).await {
            Ok(Ok(Some(data))) if hashtree_core::sha256(&data) == expected_hash => {
                let rtt_ms = sent_at.elapsed().as_millis() as u64;
                self.record_received(target_peer_id, data.len() as u64)
                    .await;
                self.peer_selector.write().await.record_success(
                    target_peer_id,
                    rtt_ms,
                    data.len() as u64,
                );
                Some(data)
            }
            Ok(Ok(Some(_))) => {
                self.peer_selector
                    .write()
                    .await
                    .record_failure(target_peer_id);
                let pending = pending_requests.lock().await.remove(hash_hex);
                if let Some(pending) = pending {
                    if let Some(quoted) = pending.quoted {
                        if let Some(in_flight) = quoted.in_flight_payment {
                            let _ = self
                                .cashu_quotes
                                .revoke_payment_token(&in_flight.mint_url, &in_flight.operation_id)
                                .await;
                        }
                    }
                }
                None
            }
            Ok(Ok(None)) | Ok(Err(_)) | Err(_) => {
                let pending = pending_requests.lock().await.remove(hash_hex);
                if let Some(pending) = pending {
                    if let Some(quoted) = pending.quoted {
                        if let Some(in_flight) = quoted.in_flight_payment {
                            let _ = self
                                .cashu_quotes
                                .revoke_payment_token(&in_flight.mint_url, &in_flight.operation_id)
                                .await;
                        }
                    }
                }
                self.peer_selector
                    .write()
                    .await
                    .record_timeout(target_peer_id);
                None
            }
        }
    }

    /// Resolve a hashtree root event through connected peers using Nostr REQ/EOSE over WebRTC.
    pub async fn resolve_root_from_peers(
        &self,
        owner_pubkey: &str,
        tree_name: &str,
        per_peer_timeout: Duration,
    ) -> Option<PeerRootEvent> {
        let filter = build_root_filter(owner_pubkey, tree_name)?;

        let peer_refs: Vec<_> = {
            let peers = self.peers.read().await;
            peers
                .values()
                .filter(|entry| entry.state == ConnectionState::Connected)
                .filter(|entry| {
                    !bluetooth_nostr_only_mode() || entry.transport != PeerTransport::Bluetooth
                })
                .filter_map(|entry| {
                    let peer = entry.peer.as_ref()?;
                    if !peer.is_ready() {
                        return None;
                    }
                    Some((entry.peer_id.short(), peer.clone()))
                })
                .collect()
        };

        for (peer_short, peer) in peer_refs {
            debug!(
                "Querying peer {} for root event {}/{}",
                peer_short, owner_pubkey, tree_name
            );
            let events = match peer
                .query_nostr_events(vec![filter.clone()], per_peer_timeout)
                .await
            {
                Ok(events) => events,
                Err(e) => {
                    debug!(
                        "Peer {} Nostr query failed for {}/{}: {}",
                        peer_short, owner_pubkey, tree_name, e
                    );
                    continue;
                }
            };
            debug!(
                "Peer {} returned {} Nostr event(s) for {}/{}",
                peer_short,
                events.len(),
                owner_pubkey,
                tree_name
            );

            let latest = pick_latest_event(events.iter().filter(|event| {
                hashtree_event_identifier(event).as_deref() == Some(tree_name)
                    && is_hashtree_labeled_event(event)
            }));
            if let Some(event) = latest {
                if let Some(root) = root_event_from_peer(event, &peer_short, tree_name) {
                    debug!(
                        "Resolved {}/{} via peer {} event {}",
                        owner_pubkey,
                        tree_name,
                        peer_short,
                        event.id.to_hex()
                    );
                    return Some(root);
                }
            }
        }

        None
    }

    pub async fn resolve_root_from_local_buses_with_source(
        &self,
        owner_pubkey: &str,
        tree_name: &str,
        timeout: Duration,
    ) -> Option<(&'static str, PeerRootEvent)> {
        let buses = self.local_buses.read().await.clone();
        for bus in buses {
            if let Some(root) = bus.query_root(owner_pubkey, tree_name, timeout).await {
                return Some((bus.source_name(), root));
            }
        }
        None
    }

    pub async fn resolve_root_from_local_buses(
        &self,
        owner_pubkey: &str,
        tree_name: &str,
        timeout: Duration,
    ) -> Option<PeerRootEvent> {
        self.resolve_root_from_local_buses_with_source(owner_pubkey, tree_name, timeout)
            .await
            .map(|(_, root)| root)
    }

    pub async fn resolve_root_from_multicast(
        &self,
        owner_pubkey: &str,
        tree_name: &str,
        timeout: Duration,
    ) -> Option<PeerRootEvent> {
        self.resolve_root_from_local_buses(owner_pubkey, tree_name, timeout)
            .await
    }
}

impl Default for WebRTCState {
    fn default() -> Self {
        Self::new()
    }
}

/// Native mesh manager handles peer discovery and transport fan-out.
pub struct WebRTCManager {
    config: WebRTCConfig,
    my_peer_id: PeerId,
    keys: Keys,
    state: Arc<WebRTCState>,
    shutdown: Arc<tokio::sync::watch::Sender<bool>>,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
    /// Channel to send signaling messages to relays
    signaling_tx: mpsc::Sender<SignalingMessage>,
    signaling_rx: Option<mpsc::Receiver<SignalingMessage>>,
    /// Optional content store for serving hash requests
    store: Option<Arc<dyn ContentStore>>,
    /// Peer classifier for pool assignment
    peer_classifier: PeerClassifier,
    /// Optional Nostr relay for data-channel relay messages
    nostr_relay: Option<Arc<NostrRelay>>,
    local_buses: Vec<SharedLocalNostrBus>,
    /// Channel for peer state events (connection success/failure)
    state_event_tx: mpsc::Sender<PeerStateEvent>,
    state_event_rx: Option<mpsc::Receiver<PeerStateEvent>>,
    /// Channel for relayless mesh signaling frames received from peers.
    mesh_frame_tx: mpsc::Sender<(PeerId, MeshNostrFrame)>,
    mesh_frame_rx: Option<mpsc::Receiver<(PeerId, MeshNostrFrame)>>,
    shared_router: Option<Arc<SharedProductionRouter>>,
    seen_frame_ids: Arc<Mutex<TimedSeenSet>>,
    seen_event_ids: Arc<Mutex<TimedSeenSet>>,
}

impl WebRTCManager {
    /// Create a new WebRTC manager
    pub fn new(keys: Keys, config: WebRTCConfig) -> Self {
        let pubkey = keys.public_key().to_hex();
        let my_peer_id = PeerId::new(pubkey);
        let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
        let (signaling_tx, signaling_rx) = mpsc::channel(100);
        let (state_event_tx, state_event_rx) = mpsc::channel(100);
        let (mesh_frame_tx, mesh_frame_rx) = mpsc::channel(256);
        let state = Arc::new(WebRTCState::new_with_routing_and_cashu(
            config.request_selection_strategy,
            config.request_fairness_enabled,
            config.request_dispatch,
            Duration::from_millis(config.message_timeout_ms),
            CashuRoutingConfig::default(),
            None,
            None,
        ));

        // Default classifier: all peers go to 'other' pool
        let peer_classifier: PeerClassifier = Arc::new(|_| PeerPool::Other);

        Self {
            config,
            my_peer_id,
            keys,
            state,
            shutdown: Arc::new(shutdown),
            shutdown_rx,
            signaling_tx,
            signaling_rx: Some(signaling_rx),
            store: None,
            peer_classifier,
            nostr_relay: None,
            local_buses: Vec::new(),
            state_event_tx,
            state_event_rx: Some(state_event_rx),
            mesh_frame_tx,
            mesh_frame_rx: Some(mesh_frame_rx),
            shared_router: None,
            seen_frame_ids: Arc::new(Mutex::new(TimedSeenSet::new(
                SEEN_FRAME_CAP,
                SEEN_FRAME_TTL,
            ))),
            seen_event_ids: Arc::new(Mutex::new(TimedSeenSet::new(
                SEEN_EVENT_CAP,
                SEEN_EVENT_TTL,
            ))),
        }
    }

    /// Create a new WebRTC manager reusing an existing shared state object.
    pub fn new_with_state(keys: Keys, config: WebRTCConfig, state: Arc<WebRTCState>) -> Self {
        let mut manager = Self::new(keys, config);
        manager.state = state;
        manager
    }

    /// Create a new WebRTC manager with a peer classifier
    pub fn new_with_classifier(
        keys: Keys,
        config: WebRTCConfig,
        classifier: PeerClassifier,
    ) -> Self {
        let mut manager = Self::new(keys, config);
        manager.peer_classifier = classifier;
        manager
    }

    /// Create a new WebRTC manager with a content store for serving hash requests
    pub fn new_with_store(keys: Keys, config: WebRTCConfig, store: Arc<dyn ContentStore>) -> Self {
        let mut manager = Self::new(keys, config);
        manager.store = Some(store);
        manager
    }

    /// Create a new WebRTC manager with store and classifier
    pub fn new_with_store_and_classifier(
        keys: Keys,
        config: WebRTCConfig,
        store: Arc<dyn ContentStore>,
        classifier: PeerClassifier,
    ) -> Self {
        Self::new_with_store_and_classifier_and_cashu(
            keys,
            config,
            store,
            classifier,
            CashuRoutingConfig::default(),
            None,
            None,
        )
    }

    pub fn new_with_state_and_store_and_classifier(
        keys: Keys,
        config: WebRTCConfig,
        state: Arc<WebRTCState>,
        store: Arc<dyn ContentStore>,
        classifier: PeerClassifier,
    ) -> Self {
        let mut manager = Self::new_with_state(keys, config, state);
        manager.store = Some(store);
        manager.peer_classifier = classifier;
        manager
    }

    pub fn new_with_store_and_classifier_and_cashu(
        keys: Keys,
        config: WebRTCConfig,
        store: Arc<dyn ContentStore>,
        classifier: PeerClassifier,
        cashu_routing: CashuRoutingConfig,
        payment_client: Option<Arc<dyn CashuPaymentClient>>,
        mint_metadata: Option<Arc<CashuMintMetadataStore>>,
    ) -> Self {
        let mut manager = Self::new(keys, config);
        manager.state = Arc::new(WebRTCState::new_with_routing_and_cashu(
            manager.config.request_selection_strategy,
            manager.config.request_fairness_enabled,
            manager.config.request_dispatch,
            Duration::from_millis(manager.config.message_timeout_ms),
            cashu_routing,
            payment_client,
            mint_metadata,
        ));
        manager.store = Some(store);
        manager.peer_classifier = classifier;
        manager
    }

    /// Set the content store for serving hash requests
    pub fn set_store(&mut self, store: Arc<dyn ContentStore>) {
        self.store = Some(store);
    }

    /// Set the peer classifier
    pub fn set_peer_classifier(&mut self, classifier: PeerClassifier) {
        self.peer_classifier = classifier;
    }

    /// Set the Nostr relay for data-channel relay messages
    pub fn set_nostr_relay(&mut self, relay: Arc<NostrRelay>) {
        self.nostr_relay = Some(relay);
    }

    /// Get my peer ID
    pub fn my_peer_id(&self) -> &PeerId {
        &self.my_peer_id
    }

    /// Get shared state for external access
    pub fn state(&self) -> Arc<WebRTCState> {
        self.state.clone()
    }

    /// Cloneable shutdown handle for external lifecycle control.
    pub fn shutdown_signal(&self) -> Arc<tokio::sync::watch::Sender<bool>> {
        self.shutdown.clone()
    }

    /// Signal shutdown
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    /// Get connected peer count
    pub async fn connected_count(&self) -> usize {
        self.state
            .connected_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Get all peer statuses
    pub async fn peer_statuses(&self) -> Vec<PeerStatus> {
        self.state
            .peers
            .read()
            .await
            .values()
            .map(|p| PeerStatus {
                peer_id: p.peer_id.to_string(),
                pubkey: p.peer_id.pubkey.clone(),
                state: p.state.to_string(),
                direction: p.direction,
                connected_at: Some(p.last_seen),
                pool: p.pool,
            })
            .collect()
    }

    /// Get pool counts
    /// Returns (follows_connected, follows_active, other_connected, other_active)
    /// "active" = Connected or Connecting (excludes Discovered and Failed)
    pub async fn get_pool_counts(&self) -> (usize, usize, usize, usize) {
        let peers = self.state.peers.read().await;
        let mut follows_connected = 0;
        let mut follows_active = 0;
        let mut other_connected = 0;
        let mut other_active = 0;

        for entry in peers.values() {
            // Only count Connected or Connecting as "active" connections
            // Discovered peers are just seen hellos, not real connections
            let is_active = entry.state == ConnectionState::Connected
                || entry.state == ConnectionState::Connecting;

            match entry.pool {
                PeerPool::Follows => {
                    if is_active {
                        follows_active += 1;
                    }
                    if entry.state == ConnectionState::Connected {
                        follows_connected += 1;
                    }
                }
                PeerPool::Other => {
                    if is_active {
                        other_active += 1;
                    }
                    if entry.state == ConnectionState::Connected {
                        other_connected += 1;
                    }
                }
            }
        }

        (
            follows_connected,
            follows_active,
            other_connected,
            other_active,
        )
    }

    fn local_hello_message(&self) -> SignalingMessage {
        SignalingMessage::Hello {
            peer_id: self.my_peer_id.to_string(),
            roots: Vec::new(),
        }
    }

    fn local_bus_max_peers(&self, source: &str) -> Option<usize> {
        match source {
            "multicast" => Some(self.config.multicast.max_peers),
            WIFI_AWARE_SOURCE => Some(self.config.wifi_aware.max_peers),
            _ => None,
        }
    }

    fn can_track_local_bus_peer(
        &self,
        source: &str,
        peer_key: &str,
        peers: &HashMap<String, PeerEntry>,
    ) -> bool {
        let Some(max_peers) = self.local_bus_max_peers(source) else {
            return true;
        };
        if peers.contains_key(peer_key) {
            return true;
        }
        if max_peers == 0 {
            return false;
        }
        let signal_path = PeerSignalPath::from_source_name(source);
        peers
            .values()
            .filter(|entry| {
                entry.signal_paths.contains(&signal_path) && entry.state != ConnectionState::Failed
            })
            .count()
            < max_peers
    }

    /// Start the native peer router - connects transports and handles signaling.
    pub async fn run(&mut self) -> Result<()> {
        info!(
            "Starting peer router with peer ID: {}",
            self.my_peer_id.short()
        );

        let (event_tx, mut event_rx) = mpsc::channel::<(String, nostr::Event)>(100);

        // Take the signaling receiver
        let mut signaling_rx = self
            .signaling_rx
            .take()
            .expect("signaling_rx already taken");

        // Take the state event receiver
        let mut state_event_rx = self
            .state_event_rx
            .take()
            .expect("state_event_rx already taken");
        let mut mesh_frame_rx = self
            .mesh_frame_rx
            .take()
            .expect("mesh_frame_rx already taken");

        if self.config.bluetooth.is_enabled() {
            let bluetooth = BluetoothMesh::new(self.config.bluetooth.clone());
            let context = BluetoothRuntimeContext {
                my_peer_id: self.my_peer_id.clone(),
                store: if bluetooth_nostr_only_mode() {
                    None
                } else {
                    self.store.clone()
                },
                nostr_relay: self.nostr_relay.clone(),
                mesh_frame_tx: self.mesh_frame_tx.clone(),
                registrar: BluetoothPeerRegistrar::new(
                    self.state.clone(),
                    self.peer_classifier.clone(),
                    self.config.pools.clone(),
                    self.config.bluetooth.max_peers,
                ),
            };
            let _ = bluetooth.start(context).await;
        }

        // Create a shared write channel for all relay tasks
        let (relay_write_tx, _) = tokio::sync::broadcast::channel::<SignalingMessage>(100);

        // Spawn relay connections
        for relay_url in &self.config.relays {
            let url = relay_url.clone();
            let event_tx = event_tx.clone();
            let shutdown_rx = self.shutdown_rx.clone();
            let keys = self.keys.clone();
            let relay_write_rx = relay_write_tx.subscribe();

            tokio::spawn(async move {
                if let Err(e) =
                    Self::relay_task(url.clone(), event_tx, shutdown_rx, keys, relay_write_rx).await
                {
                    error!("Relay {} error: {}", url, e);
                }
            });
        }

        if self.config.multicast.is_enabled() {
            if let Some(relay) = self.nostr_relay.clone() {
                match MulticastNostrBus::bind(
                    self.config.multicast.clone(),
                    self.keys.clone(),
                    relay,
                )
                .await
                {
                    Ok(bus) => {
                        let local_bus: SharedLocalNostrBus = bus.clone();
                        self.state.add_local_bus(local_bus.clone()).await;
                        self.local_buses.push(local_bus);
                        let shutdown_rx = self.shutdown_rx.clone();
                        let signaling_tx = event_tx.clone();
                        tokio::spawn(async move {
                            if let Err(err) = bus.run(shutdown_rx, signaling_tx).await {
                                error!("Multicast bus error: {}", err);
                            }
                        });
                    }
                    Err(err) => {
                        warn!("Failed to start multicast bus: {}", err);
                    }
                }
            } else {
                warn!("Multicast enabled but Nostr relay is unavailable");
            }
        }

        if self.config.wifi_aware.is_enabled() {
            if let Some(relay) = self.nostr_relay.clone() {
                if let Some(bridge) = mobile_wifi_aware_bridge() {
                    let bus = WifiAwareNostrBus::new(
                        self.config.wifi_aware.clone(),
                        self.keys.clone(),
                        relay,
                        bridge,
                    );
                    let local_bus: SharedLocalNostrBus = bus.clone();
                    self.state.add_local_bus(local_bus.clone()).await;
                    self.local_buses.push(local_bus);
                    let shutdown_rx = self.shutdown_rx.clone();
                    let signaling_tx = event_tx.clone();
                    let local_peer_id = self.my_peer_id.to_string();
                    tokio::spawn(async move {
                        if let Err(err) = bus.run(local_peer_id, shutdown_rx, signaling_tx).await {
                            error!("Wi-Fi Aware bus error: {}", err);
                        }
                    });
                } else {
                    warn!("Wi-Fi Aware enabled but no mobile bridge is installed");
                }
            } else {
                warn!("Wi-Fi Aware enabled but Nostr relay is unavailable");
            }
        }

        if self.config.signaling_enabled {
            let transport = Arc::new(RouterSignalingBridge::new(
                self.my_peer_id.to_string(),
                self.signaling_tx.clone(),
            ));
            let factory = Arc::new(SharedRouterPeerFactory::new(
                self.my_peer_id.clone(),
                self.signaling_tx.clone(),
                self.config.stun_servers.clone(),
                self.store.clone(),
                self.state.clone(),
                self.state_event_tx.clone(),
                self.nostr_relay.clone(),
                self.mesh_frame_tx.clone(),
                self.peer_classifier.clone(),
            ));
            let (classifier_tx, mut classifier_rx) = mpsc::channel::<SharedClassifyRequest>(32);
            let classifier = self.peer_classifier.clone();
            tokio::spawn(async move {
                while let Some(request) = classifier_rx.recv().await {
                    let _ = request.response.send(classifier(&request.pubkey));
                }
            });

            let mut router = MeshRouter::new(
                self.my_peer_id.to_string(),
                transport,
                factory.clone(),
                self.config.pools.clone(),
                self.config.debug,
            );
            router.set_classifier(classifier_tx);
            self.shared_router = Some(Arc::new(router));
        }

        // Process incoming events and outgoing signaling messages
        let mut shutdown_rx = self.shutdown_rx.clone();
        // Cleanup interval - run every 30 seconds as a fallback (not for real-time sync)
        let mut cleanup_interval = tokio::time::interval(Duration::from_secs(30));
        let mut hello_ticker =
            tokio::time::interval(Duration::from_millis(self.config.hello_interval_ms));
        if self.config.signaling_enabled {
            if let Some(shared_router) = self.shared_router.as_ref() {
                let _ = shared_router.send_hello(Vec::new()).await;
            } else {
                self.dispatch_signaling_message(self.local_hello_message(), &relay_write_tx)
                    .await;
            }
        }
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        info!("WebRTC manager shutting down");
                        break;
                    }
                }
                Some((relay, event)) = event_rx.recv() => {
                    if let Err(e) = self
                        .handle_event(&relay, &event, self.shared_router.as_ref())
                        .await
                    {
                        debug!("Error handling event from {}: {}", relay, e);
                    }
                }
                Some(msg) = signaling_rx.recv() => {
                    self.dispatch_signaling_message(msg, &relay_write_tx).await;
                }
                Some(event) = state_event_rx.recv() => {
                    // Handle peer state events (connected, failed, disconnected)
                    self.handle_peer_state_event(event, &relay_write_tx).await;
                }
                Some((from_peer_id, frame)) = mesh_frame_rx.recv() => {
                    self.handle_mesh_frame(from_peer_id, frame).await;
                }
                _ = hello_ticker.tick(), if self.config.signaling_enabled => {
                    if let Some(shared_router) = self.shared_router.as_ref() {
                        let _ = shared_router.send_hello(Vec::new()).await;
                    } else {
                        self.dispatch_signaling_message(self.local_hello_message(), &relay_write_tx)
                            .await;
                    }
                }
                _ = cleanup_interval.tick() => {
                    // Periodic cleanup of stale peers and state sync (fallback)
                    self.cleanup_stale_peers().await;
                }
            }
        }

        Ok(())
    }

    /// Connect to a single relay and handle messages
    async fn relay_task(
        url: String,
        event_tx: mpsc::Sender<(String, nostr::Event)>,
        mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
        keys: Keys,
        mut signaling_rx: tokio::sync::broadcast::Receiver<SignalingMessage>,
    ) -> Result<()> {
        info!("Connecting to relay: {}", url);

        let (ws_stream, _) = connect_async(&url).await?;
        let (mut write, mut read) = ws_stream.split();

        // Subscribe to webrtc events - two filters:
        // 1. Hello messages: kind 25050 with #l: "hello" tag
        // 2. Directed messages: kind 25050 with #p tag (our pubkey)
        let hello_filter = Filter::new()
            .kind(Kind::Ephemeral(WEBRTC_KIND as u16))
            .custom_tag(
                nostr::SingleLetterTag::lowercase(nostr::Alphabet::L),
                vec![HELLO_TAG],
            )
            .since(nostr::Timestamp::now() - Duration::from_secs(60));

        let directed_filter = Filter::new()
            .kind(Kind::Ephemeral(WEBRTC_KIND as u16))
            .custom_tag(
                nostr::SingleLetterTag::lowercase(nostr::Alphabet::P),
                vec![keys.public_key().to_hex()],
            )
            .since(nostr::Timestamp::now() - Duration::from_secs(60));

        let sub_id = nostr::SubscriptionId::generate();
        let sub_msg = ClientMessage::req(sub_id.clone(), vec![hello_filter, directed_filter]);
        write.send(Message::Text(sub_msg.as_json())).await?;

        info!(
            "Subscribed to {} for WebRTC events (kind {})",
            url, WEBRTC_KIND
        );

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
                // Handle outgoing signaling messages
                Ok(signaling_msg) = signaling_rx.recv() => {
                    info!("Sending {} via {}", signaling_msg.msg_type(), url);
                    if let Ok(event) = Self::create_signaling_event(&keys, &signaling_msg).await {
                        let event_id = event.id.to_string();
                        let msg = ClientMessage::event(event);
                        if write.send(Message::Text(msg.as_json())).await.is_ok() {
                            info!("Sent {} to {} (event id: {})", signaling_msg.msg_type(), url, &event_id[..16]);
                        }
                    }
                }
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            if let Ok(RelayMessage::Event { event, .. }) =
                                RelayMessage::from_json(&text)
                            {
                                let _ = event_tx.send((url.clone(), *event)).await;
                            }
                        }
                        Some(Err(e)) => {
                            error!("WebSocket error from {}: {}", url, e);
                            break;
                        }
                        None => {
                            warn!("WebSocket closed: {}", url);
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }

        Ok(())
    }

    async fn mark_seen_frame_id(&self, frame_id: String) -> bool {
        let mut seen = self.seen_frame_ids.lock().await;
        seen.insert_if_new(frame_id)
    }

    async fn mark_seen_event_id(&self, event_id: String) -> bool {
        let mut seen = self.seen_event_ids.lock().await;
        seen.insert_if_new(event_id)
    }

    async fn dispatch_signaling_message(
        &self,
        msg: SignalingMessage,
        relay_write_tx: &tokio::sync::broadcast::Sender<SignalingMessage>,
    ) {
        if !self.config.signaling_enabled {
            debug!(
                "Skipping signaling message {} because WebRTC signaling is disabled",
                msg.msg_type()
            );
            return;
        }

        if relay_write_tx.send(msg.clone()).is_err() {
            debug!(
                "No relay subscribers for signaling message {}",
                msg.msg_type()
            );
        }

        let event = match Self::create_signaling_event(&self.keys, &msg).await {
            Ok(event) => event,
            Err(e) => {
                debug!("Failed to create signaling event for mesh dispatch: {}", e);
                return;
            }
        };

        for bus in &self.local_buses {
            if let Err(err) = bus.broadcast_event(&event).await {
                debug!(
                    "Failed to broadcast signaling event over {} ({}): {}",
                    bus.source_name(),
                    msg.msg_type(),
                    err
                );
            }
        }

        let mut frame =
            MeshNostrFrame::new_event(event, &self.my_peer_id.to_string(), MESH_DEFAULT_HTL);
        if !self.mark_seen_frame_id(frame.frame_id.clone()).await {
            self.state.record_mesh_duplicate_drop();
            return;
        }
        if !self.mark_seen_event_id(frame.event().id.to_hex()).await {
            self.state.record_mesh_duplicate_drop();
            return;
        }

        // Keep the sender peer id stable even if this is forwarded later.
        frame.sender_peer_id = self.my_peer_id.to_string();
        let forwarded = self.forward_mesh_frame(&frame, None).await;
        if forwarded > 0 {
            self.state.record_mesh_forwarded(forwarded as u64);
        }
    }

    async fn forward_mesh_frame(
        &self,
        frame: &MeshNostrFrame,
        exclude_peer_id: Option<&str>,
    ) -> usize {
        let peers = self.state.peers.read().await;
        let peer_refs: Vec<_> = peers
            .values()
            .filter(|entry| entry.state == ConnectionState::Connected)
            .filter(|entry| {
                entry
                    .peer
                    .as_ref()
                    .map(|peer| peer.is_ready())
                    .unwrap_or(false)
            })
            .filter(|entry| {
                exclude_peer_id
                    .map(|exclude| exclude != entry.peer_id.to_string())
                    .unwrap_or(true)
            })
            .filter_map(|entry| {
                entry.peer.as_ref().map(|peer| {
                    (
                        entry.peer_id.to_string(),
                        entry.peer_id.short(),
                        peer.clone(),
                        peer.htl_config(),
                    )
                })
            })
            .collect();
        drop(peers);

        let mut forwarded = 0usize;
        for (_peer_key, peer_short, peer, htl_cfg) in peer_refs {
            let next_htl = decrement_htl_with_policy(frame.htl, &MESH_EVENT_POLICY, &htl_cfg);
            if !should_forward_htl(next_htl) {
                continue;
            }

            let mut outbound = frame.clone();
            outbound.htl = next_htl;
            if peer.send_mesh_frame_text(&outbound).await.is_ok() {
                forwarded += 1;
            } else {
                debug!("Failed to forward mesh frame to {}", peer_short);
            }
        }

        forwarded
    }

    async fn handle_mesh_frame(&self, from_peer_id: PeerId, frame: MeshNostrFrame) {
        if let Err(reason) = validate_mesh_frame(&frame) {
            debug!(
                "Ignoring mesh frame from {} (invalid: {})",
                from_peer_id.short(),
                reason
            );
            return;
        }

        if !self.mark_seen_frame_id(frame.frame_id.clone()).await {
            self.state.record_mesh_duplicate_drop();
            return;
        }

        let event = match &frame.payload {
            MeshNostrPayload::Event { event } => event.clone(),
        };

        if !self.mark_seen_event_id(event.id.to_hex()).await {
            self.state.record_mesh_duplicate_drop();
            return;
        }

        if event.verify().is_err() {
            debug!(
                "Ignoring mesh event from {} due to invalid signature",
                from_peer_id.short()
            );
            return;
        }

        self.state.record_mesh_received();

        if let Err(e) = self
            .handle_event("mesh", &event, self.shared_router.as_ref())
            .await
        {
            debug!(
                "Error handling mesh event from {}: {}",
                from_peer_id.short(),
                e
            );
        }

        let forwarded = self
            .forward_mesh_frame(&frame, Some(&from_peer_id.to_string()))
            .await;
        if forwarded > 0 {
            self.state.record_mesh_forwarded(forwarded as u64);
        }
    }

    /// Create a signaling event
    ///
    /// For directed messages (offer, answer, candidate, candidates), use NIP-17 style
    /// gift wrapping with ephemeral keys for privacy.
    /// Hello messages use kind 25050 with #l: "hello" tag and peerId.
    async fn create_signaling_event(keys: &Keys, msg: &SignalingMessage) -> Result<nostr::Event> {
        encode_signaling_event(
            keys,
            msg.peer_id(),
            msg,
            Kind::Ephemeral(WEBRTC_KIND as u16),
        )
        .map_err(|e| anyhow::anyhow!(e.to_string()))
    }

    /// Handle an incoming event
    ///
    /// Messages may be:
    /// 1. Hello messages: kind 25050 with #l: "hello" tag and peerId
    /// 2. Gift-wrapped directed messages: kind 25050 with #p tag, encrypted with ephemeral key
    async fn handle_event(
        &self,
        relay: &str,
        event: &nostr::Event,
        shared_router: Option<&Arc<SharedProductionRouter>>,
    ) -> Result<()> {
        if !self.config.signaling_enabled {
            return Ok(());
        }

        let Some(shared_router) = shared_router else {
            return Ok(());
        };

        let Some(msg) = decode_signaling_event(
            event,
            &self.my_peer_id.to_string(),
            &self.keys.public_key().to_hex(),
            &self.keys,
        ) else {
            return Ok(());
        };

        if matches!(
            msg,
            SignalingMessage::Hello { .. } | SignalingMessage::Offer { .. }
        ) {
            let peers = self.state.peers.read().await;
            if !self.can_track_local_bus_peer(relay, msg.peer_id(), &peers) {
                return Ok(());
            }
        }

        debug!(
            "Received {} from {} via {}",
            msg.msg_type(),
            msg.peer_id(),
            relay
        );
        let peer_id = msg.peer_id().to_string();
        shared_router
            .handle_message(msg)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        remember_peer_signal_path(self.state.as_ref(), &peer_id, relay).await;

        Ok(())
    }

    /// Handle peer state change events from peer connections
    async fn handle_peer_state_event(
        &self,
        event: PeerStateEvent,
        relay_write_tx: &tokio::sync::broadcast::Sender<SignalingMessage>,
    ) {
        match event {
            PeerStateEvent::Connected(peer_id) => {
                let peer_key = peer_id.to_string();
                let mut emit_hello = false;
                let mut peers = self.state.peers.write().await;
                if let Some(entry) = peers.get_mut(&peer_key) {
                    if entry.state != ConnectionState::Connected {
                        info!("Peer {} connected (via state event)", peer_id.short());
                        entry.state = ConnectionState::Connected;
                        emit_hello = true;
                        // Update connected count
                        self.state
                            .connected_count
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                drop(peers);
                if emit_hello {
                    if let Some(shared_router) = self.shared_router.as_ref() {
                        let _ = shared_router.send_hello(Vec::new()).await;
                    } else {
                        self.dispatch_signaling_message(self.local_hello_message(), relay_write_tx)
                            .await;
                    }
                }
            }
            PeerStateEvent::Failed(peer_id) => {
                let peer_key = peer_id.to_string();
                info!(
                    "Peer {} connection failed - removing from pool",
                    peer_id.short()
                );
                let removed = {
                    let mut peers = self.state.peers.write().await;
                    peers.remove(&peer_key)
                };
                if let Some(entry) = removed {
                    // Decrement connected count if was connected
                    if entry.state == ConnectionState::Connected {
                        self.state
                            .connected_count
                            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    // Close the peer connection if it exists
                    if let Some(peer) = entry.peer {
                        let _ = peer.close().await;
                    }
                }
                if let Some(shared_router) = self.shared_router.as_ref() {
                    if let Some(channel) = shared_router.remove_peer(&peer_key).await {
                        channel.close().await;
                    }
                }
            }
            PeerStateEvent::Disconnected(peer_id) => {
                let peer_key = peer_id.to_string();
                info!("Peer {} disconnected - removing from pool", peer_id.short());
                let removed = {
                    let mut peers = self.state.peers.write().await;
                    peers.remove(&peer_key)
                };
                if let Some(entry) = removed {
                    // Decrement connected count if was connected
                    if entry.state == ConnectionState::Connected {
                        self.state
                            .connected_count
                            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    // Close the peer connection if it exists
                    if let Some(peer) = entry.peer {
                        let _ = peer.close().await;
                    }
                }
                if let Some(shared_router) = self.shared_router.as_ref() {
                    if let Some(channel) = shared_router.remove_peer(&peer_key).await {
                        channel.close().await;
                    }
                }
            }
        }
    }

    /// Cleanup stale peers and sync connection states (fallback, runs every 30s)
    async fn cleanup_stale_peers(&self) {
        let mut peers = self.state.peers.write().await;
        let mut connected_count = 0;
        let mut to_remove = Vec::new();
        let stale_timeout = Duration::from_secs(60); // Remove peers stuck in Discovered/Connecting for 60s

        for (key, entry) in peers.iter_mut() {
            if let Some(ref peer) = entry.peer {
                // Sync connected state as fallback (in case event was missed)
                if peer.is_connected() {
                    if entry.state != ConnectionState::Connected {
                        info!(
                            "Peer {} is now connected (sync fallback)",
                            entry.peer_id.short()
                        );
                        entry.state = ConnectionState::Connected;
                    }
                    connected_count += 1;
                } else if entry.state == ConnectionState::Connected {
                    info!(
                        "Removing disconnected peer {} after transport closed",
                        entry.peer_id.short()
                    );
                    to_remove.push(key.clone());
                } else if entry.state == ConnectionState::Connecting
                    && entry.last_seen.elapsed() > stale_timeout
                {
                    // Peer stuck in Connecting for too long - mark for removal
                    info!(
                        "Removing stale peer {} (stuck in Connecting for {:?})",
                        entry.peer_id.short(),
                        entry.last_seen.elapsed()
                    );
                    to_remove.push(key.clone());
                }
            } else if entry.state == ConnectionState::Discovered
                && entry.last_seen.elapsed() > stale_timeout
            {
                // Discovered peer with no actual connection - remove
                debug!("Removing stale discovered peer {}", entry.peer_id.short());
                to_remove.push(key.clone());
            }
        }

        // Remove stale peers
        let mut removed_peers = Vec::new();
        for key in to_remove {
            if let Some(entry) = peers.remove(&key) {
                removed_peers.push(entry);
            }
        }
        drop(peers);

        for entry in removed_peers {
            if let Some(peer) = entry.peer {
                let _ = peer.close().await;
            }
        }

        self.state
            .connected_count
            .store(connected_count, std::sync::atomic::Ordering::Relaxed);
    }
}

// Keep the old PeerState for backward compatibility with tests
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct PeerState {
    pub peer_id: PeerId,
    pub direction: PeerDirection,
    pub state: String,
    pub last_seen: Instant,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webrtc::root_events::PeerRootEvent;
    use crate::webrtc::session::TestMeshPeer;
    use crate::webrtc::SelectionStrategy;
    use anyhow::Result as AnyResult;
    use async_trait::async_trait;
    use hashtree_network::{build_hedged_wave_plan, normalize_dispatch_config};
    use nostr::{EventBuilder, Keys, Tag};
    use std::time::Duration;

    struct TestLocalBus {
        source: &'static str,
        root: Option<PeerRootEvent>,
    }

    #[async_trait]
    impl super::super::LocalNostrBus for TestLocalBus {
        fn source_name(&self) -> &'static str {
            self.source
        }

        async fn broadcast_event(&self, _event: &nostr::Event) -> AnyResult<()> {
            Ok(())
        }

        async fn query_root(
            &self,
            _owner_pubkey: &str,
            _tree_name: &str,
            _timeout: Duration,
        ) -> Option<PeerRootEvent> {
            self.root.clone()
        }
    }

    #[test]
    fn root_event_from_peer_extracts_tags() {
        let keys = Keys::generate();
        let hash = "ab".repeat(32);
        let event = EventBuilder::new(
            Kind::Custom(super::super::root_events::HASHTREE_KIND),
            "",
            [
                Tag::parse(&["d", "repo"]).unwrap(),
                Tag::parse(&["l", super::super::root_events::HASHTREE_LABEL]).unwrap(),
                Tag::parse(&["hash", &hash]).unwrap(),
                Tag::parse(&["encryptedKey", &"11".repeat(32)]).unwrap(),
            ],
        )
        .to_event(&keys)
        .unwrap();

        let parsed = root_event_from_peer(&event, "peer-a", "repo").unwrap();
        let expected_encrypted = "11".repeat(32);
        assert_eq!(parsed.hash, hash);
        assert_eq!(parsed.peer_id, "peer-a");
        assert_eq!(
            parsed.encrypted_key.as_deref(),
            Some(expected_encrypted.as_str())
        );
        assert!(parsed.key.is_none());
    }

    #[test]
    fn pick_latest_event_prefers_higher_event_id_on_timestamp_tie() {
        let keys = Keys::generate();
        let created_at = nostr::Timestamp::from_secs(1_700_000_000);
        let event_a = EventBuilder::new(
            Kind::Custom(super::super::root_events::HASHTREE_KIND),
            "",
            [],
        )
        .custom_created_at(created_at)
        .to_event(&keys)
        .unwrap();
        let event_b = EventBuilder::new(
            Kind::Custom(super::super::root_events::HASHTREE_KIND),
            "",
            [],
        )
        .custom_created_at(created_at)
        .to_event(&keys)
        .unwrap();

        let expected = if event_a.id > event_b.id {
            event_a.id
        } else {
            event_b.id
        };
        let picked = pick_latest_event([&event_a, &event_b]).unwrap();
        assert_eq!(picked.id, expected);
    }

    #[tokio::test]
    async fn resolve_root_from_local_buses_returns_source_and_first_match() {
        let state = WebRTCState::new();
        let root = PeerRootEvent {
            hash: "ab".repeat(32),
            key: None,
            encrypted_key: None,
            self_encrypted_key: None,
            event_id: "event-1".to_string(),
            created_at: 1,
            peer_id: "bus-peer".to_string(),
        };

        state
            .set_local_buses(vec![
                Arc::new(TestLocalBus {
                    source: "empty",
                    root: None,
                }),
                Arc::new(TestLocalBus {
                    source: "mock-bus",
                    root: Some(root.clone()),
                }),
            ])
            .await;

        let resolved = state
            .resolve_root_from_local_buses_with_source("owner", "tree", Duration::from_millis(10))
            .await
            .expect("expected root from local bus");

        assert_eq!(resolved.0, "mock-bus");
        assert_eq!(resolved.1.hash, root.hash);
        assert_eq!(resolved.1.peer_id, root.peer_id);
    }

    #[tokio::test]
    async fn can_track_local_bus_peer_enforces_wifi_aware_limit() {
        let keys = Keys::generate();
        let mut config = WebRTCConfig::default();
        config.wifi_aware.enabled = true;
        config.wifi_aware.max_peers = 1;
        let manager = WebRTCManager::new(keys, config);
        let existing_peer = PeerId::new("peer-a".to_string());
        let existing_key = existing_peer.to_string();
        let mut peers = HashMap::new();
        peers.insert(
            existing_key.clone(),
            PeerEntry {
                peer_id: existing_peer,
                direction: PeerDirection::Outbound,
                state: ConnectionState::Discovered,
                last_seen: Instant::now(),
                peer: None,
                pool: PeerPool::Other,
                transport: PeerTransport::WebRtc,
                signal_paths: BTreeSet::from([PeerSignalPath::WifiAware]),
                bytes_sent: 0,
                bytes_received: 0,
            },
        );

        assert!(manager.can_track_local_bus_peer(WIFI_AWARE_SOURCE, &existing_key, &peers,));
        assert!(!manager.can_track_local_bus_peer(WIFI_AWARE_SOURCE, "peer-b:sess-b", &peers,));
        assert!(manager.can_track_local_bus_peer("relay", "peer-c:sess-c", &peers));
    }

    #[tokio::test]
    async fn request_from_peers_with_source_accepts_generic_mesh_peers() {
        let state = WebRTCState::new();
        let data = b"offline-over-ble".to_vec();
        let hash_hex = hex::encode(hashtree_core::sha256(&data));

        state.peers.write().await.insert(
            "peer-a".to_string(),
            PeerEntry {
                peer_id: PeerId::new("peer-a-pub".to_string()),
                direction: PeerDirection::Outbound,
                state: ConnectionState::Connected,
                last_seen: Instant::now(),
                peer: Some(MeshPeer::mock_for_tests(TestMeshPeer::with_response(Some(
                    data.clone(),
                )))),
                pool: PeerPool::Other,
                transport: PeerTransport::Bluetooth,
                signal_paths: BTreeSet::from([PeerSignalPath::Bluetooth]),
                bytes_sent: 0,
                bytes_received: 0,
            },
        );

        let resolved = state
            .request_from_peers_with_source(&hash_hex)
            .await
            .expect("expected mock mesh peer response");

        assert_eq!(resolved.0, data);
        assert_eq!(resolved.1, "peer-a-pub");
    }

    #[tokio::test]
    async fn request_from_peers_with_source_waits_full_timeout_for_last_generic_peer() {
        let state = WebRTCState::new_with_routing_and_cashu(
            SelectionStrategy::TitForTat,
            true,
            RequestDispatchConfig {
                initial_fanout: 1,
                hedge_fanout: 1,
                max_fanout: 1,
                hedge_interval_ms: 50,
            },
            Duration::from_millis(400),
            CashuRoutingConfig::default(),
            None,
            None,
        );
        let data = b"slow-offline-over-ble".to_vec();
        let hash_hex = hex::encode(hashtree_core::sha256(&data));

        state.peers.write().await.insert(
            "peer-a".to_string(),
            PeerEntry {
                peer_id: PeerId::new("peer-a-pub".to_string()),
                direction: PeerDirection::Outbound,
                state: ConnectionState::Connected,
                last_seen: Instant::now(),
                peer: Some(MeshPeer::mock_for_tests(
                    TestMeshPeer::with_delayed_response(
                        Some(data.clone()),
                        Duration::from_millis(200),
                    ),
                )),
                pool: PeerPool::Other,
                transport: PeerTransport::Bluetooth,
                signal_paths: BTreeSet::from([PeerSignalPath::Bluetooth]),
                bytes_sent: 0,
                bytes_received: 0,
            },
        );

        let resolved = state
            .request_from_peers_with_source(&hash_hex)
            .await
            .expect("expected delayed mock mesh peer response");

        assert_eq!(resolved.0, data);
        assert_eq!(resolved.1, "peer-a-pub");
    }

    #[tokio::test]
    async fn dispatch_signaling_message_is_noop_when_signaling_disabled() {
        let keys = Keys::generate();
        let mut config = WebRTCConfig::default();
        config.signaling_enabled = false;
        let manager = WebRTCManager::new(keys, config);
        let peer_id = PeerId::new("peer-a-pub".to_string());
        let peer_key = peer_id.to_string();
        let peer = MeshPeer::mock_for_tests(TestMeshPeer::with_response(None));
        let peer_ref = peer.mock_ref().expect("mock peer").clone();

        manager.state.peers.write().await.insert(
            peer_key,
            PeerEntry {
                peer_id,
                direction: PeerDirection::Outbound,
                state: ConnectionState::Connected,
                last_seen: Instant::now(),
                peer: Some(peer),
                pool: PeerPool::Other,
                transport: PeerTransport::Bluetooth,
                signal_paths: BTreeSet::from([PeerSignalPath::Bluetooth]),
                bytes_sent: 0,
                bytes_received: 0,
            },
        );

        let (relay_tx, _) = tokio::sync::broadcast::channel(4);
        manager
            .dispatch_signaling_message(
                SignalingMessage::Hello {
                    peer_id: manager.my_peer_id.to_string(),
                    roots: Vec::new(),
                },
                &relay_tx,
            )
            .await;

        assert_eq!(peer_ref.sent_frame_count().await, 0);
    }

    #[tokio::test]
    async fn failed_peer_cleanup_does_not_hold_peer_map_lock_while_closing() {
        let keys = Keys::generate();
        let manager = Arc::new(WebRTCManager::new(keys, WebRTCConfig::default()));
        let peer_id = PeerId::new("peer-a-pub".to_string());
        let peer_key = peer_id.to_string();

        manager.state.peers.write().await.insert(
            peer_key.clone(),
            PeerEntry {
                peer_id: peer_id.clone(),
                direction: PeerDirection::Outbound,
                state: ConnectionState::Connected,
                last_seen: Instant::now(),
                peer: Some(MeshPeer::mock_for_tests(TestMeshPeer::with_delayed_close(
                    Duration::from_millis(200),
                ))),
                pool: PeerPool::Other,
                transport: PeerTransport::Bluetooth,
                signal_paths: BTreeSet::from([PeerSignalPath::Bluetooth]),
                bytes_sent: 0,
                bytes_received: 0,
            },
        );

        let (relay_tx, _) = tokio::sync::broadcast::channel(4);
        let manager_for_task = manager.clone();
        let peer_id_for_task = peer_id.clone();
        let cleanup_task = tokio::spawn(async move {
            manager_for_task
                .handle_peer_state_event(PeerStateEvent::Failed(peer_id_for_task), &relay_tx)
                .await;
        });

        tokio::time::sleep(Duration::from_millis(20)).await;

        let remaining = tokio::time::timeout(Duration::from_millis(50), async {
            manager.state.peers.read().await.len()
        })
        .await
        .expect("peer map read should not block on close");

        assert_eq!(remaining, 0);
        cleanup_task.await.expect("cleanup task");
    }

    #[tokio::test]
    async fn resolve_root_from_peers_does_not_hold_peer_map_lock_while_querying() {
        let keys = Keys::generate();
        let manager = Arc::new(WebRTCManager::new(keys.clone(), WebRTCConfig::default()));
        let owner_keys = Keys::generate();
        let owner_pubkey = owner_keys.public_key().to_hex();
        let tree_name = "video";
        let hash = "ab".repeat(32);
        let event = EventBuilder::new(
            Kind::Custom(super::super::root_events::HASHTREE_KIND),
            "",
            [
                Tag::parse(&["d", tree_name]).unwrap(),
                Tag::parse(&["l", super::super::root_events::HASHTREE_LABEL]).unwrap(),
                Tag::parse(&["hash", &hash]).unwrap(),
            ],
        )
        .to_event(&owner_keys)
        .unwrap();

        let peer_id = PeerId::new("peer-a-pub".to_string());
        let peer_key = peer_id.to_string();

        manager.state.peers.write().await.insert(
            peer_key.clone(),
            PeerEntry {
                peer_id,
                direction: PeerDirection::Outbound,
                state: ConnectionState::Connected,
                last_seen: Instant::now(),
                peer: Some(MeshPeer::mock_for_tests(TestMeshPeer::with_delayed_events(
                    vec![event],
                    Duration::from_millis(200),
                ))),
                pool: PeerPool::Other,
                transport: PeerTransport::Bluetooth,
                signal_paths: BTreeSet::from([PeerSignalPath::Bluetooth]),
                bytes_sent: 0,
                bytes_received: 0,
            },
        );

        let manager_for_task = manager.clone();
        let owner_pubkey_for_task = owner_pubkey.clone();
        let resolve_task = tokio::spawn(async move {
            manager_for_task
                .state
                .resolve_root_from_peers(
                    &owner_pubkey_for_task,
                    tree_name,
                    Duration::from_millis(500),
                )
                .await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;

        let manager_for_writer = manager.clone();
        let peer_key_for_writer = peer_key.clone();
        let writer_task = tokio::spawn(async move {
            let mut peers = manager_for_writer.state.peers.write().await;
            if let Some(entry) = peers.get_mut(&peer_key_for_writer) {
                entry.bytes_received += 1;
            }
        });

        tokio::time::sleep(Duration::from_millis(20)).await;

        let status_count = tokio::time::timeout(Duration::from_millis(50), async {
            manager.state.peers.read().await.len()
        })
        .await
        .expect("peer map read should not block on root query");

        assert_eq!(status_count, 1);
        assert!(resolve_task.await.expect("resolve task").is_some());
        writer_task.await.expect("writer task");
    }

    #[test]
    fn test_formal_timed_seen_set_rejects_duplicates() {
        let mut seen = TimedSeenSet::new(4, Duration::from_secs(60));
        assert!(seen.insert_if_new("frame-1".to_string()));
        assert!(!seen.insert_if_new("frame-1".to_string()));
        assert!(seen.insert_if_new("frame-2".to_string()));
    }

    #[test]
    fn test_formal_timed_seen_set_evicts_oldest_when_capacity_exceeded() {
        let mut seen = TimedSeenSet::new(2, Duration::from_secs(60));
        assert!(seen.insert_if_new("a".to_string()));
        assert!(seen.insert_if_new("b".to_string()));
        assert!(seen.insert_if_new("c".to_string()));

        // "a" should be evicted due to cap=2, so re-insert becomes new again.
        assert!(seen.insert_if_new("a".to_string()));
        assert!(!seen.insert_if_new("a".to_string()));
    }

    #[test]
    fn test_request_dispatch_normalization_caps_to_available_peers() {
        let normalized = normalize_dispatch_config(
            RequestDispatchConfig {
                initial_fanout: 8,
                hedge_fanout: 6,
                max_fanout: 5,
                hedge_interval_ms: 120,
            },
            3,
        );
        assert_eq!(normalized.max_fanout, 3);
        assert_eq!(normalized.initial_fanout, 3);
        assert_eq!(normalized.hedge_fanout, 3);
    }

    #[test]
    fn test_hedged_wave_plan_matches_dispatch_policy() {
        let plan = build_hedged_wave_plan(
            7,
            RequestDispatchConfig {
                initial_fanout: 2,
                hedge_fanout: 3,
                max_fanout: 6,
                hedge_interval_ms: 120,
            },
        );
        assert_eq!(plan, vec![2, 3, 1]);
    }
}
