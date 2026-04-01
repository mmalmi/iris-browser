//! Shared routed mesh store core.
//!
//! This module provides a concrete store wrapper that works with any local storage
//! backend plus any signaling transport and peer-link factory. Both production
//! (Nostr websockets + WebRTC) and simulation (mocks) use this same code.

use async_trait::async_trait;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::hash::{Hash as _, Hasher};
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, Mutex, RwLock};

use hashtree_core::{Hash, Store, StoreError};

use crate::peer_selector::{PeerMetadataSnapshot, PeerSelector, SelectionStrategy};
use crate::protocol::{
    create_quote_request, create_quote_response_available, create_quote_response_unavailable,
    create_request, create_request_with_quote, create_response, encode_quote_request,
    encode_quote_response, encode_request, encode_response, hash_to_key, parse_message,
    DataMessage, DataQuoteRequest, DataQuoteResponse,
};
use crate::signaling::MeshRouter;
use crate::transport::{PeerLinkFactory, SignalingTransport, TransportError};
use crate::types::{PeerHTLConfig, SignalingMessage, MAX_HTL};

// Keep the on-disk namespace stable across the crate rename so existing peer
// metadata does not disappear for users upgrading from the old package name.
const PEER_METADATA_POINTER_SLOT_KEY: &[u8] = b"hashtree-webrtc/peer-metadata/latest/v1";

/// Pending request awaiting response
struct PendingRequest {
    response_tx: oneshot::Sender<Option<Vec<u8>>>,
    started_at: Instant,
    queried_peers: Vec<String>,
}

struct PendingQuoteRequest {
    response_tx: oneshot::Sender<Option<NegotiatedQuote>>,
    preferred_mint_url: Option<String>,
    offered_payment_sat: u64,
}

#[derive(Debug, Clone)]
struct NegotiatedQuote {
    peer_id: String,
    quote_id: u64,
    #[allow(dead_code)]
    mint_url: Option<String>,
}

struct IssuedQuote {
    expires_at: Instant,
    #[allow(dead_code)]
    payment_sat: u64,
    #[allow(dead_code)]
    mint_url: Option<String>,
}

/// Aggregate stats from draining currently available peer-link messages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DataPumpStats {
    pub processed: usize,
    pub request_messages: usize,
    pub response_messages: usize,
    pub quote_request_messages: u64,
    pub quote_response_messages: u64,
    pub processed_bytes: u64,
}

/// Request dispatch strategy for peer queries.
///
/// `MeshStoreCore` supports two practical retrieval modes:
/// - Flood (`usize::MAX` fanout): maximize success/latency at bandwidth cost.
/// - Staged hedging: probe a subset first, then expand.
#[derive(Debug, Clone, Copy)]
pub struct RequestDispatchConfig {
    /// Number of peers queried immediately.
    pub initial_fanout: usize,
    /// Number of additional peers to query on each hedge step.
    pub hedge_fanout: usize,
    /// Total peers allowed for this request.
    pub max_fanout: usize,
    /// Delay between hedge waves (ms). `0` means send all waves immediately.
    pub hedge_interval_ms: u64,
}

impl Default for RequestDispatchConfig {
    fn default() -> Self {
        Self {
            initial_fanout: usize::MAX,
            hedge_fanout: usize::MAX,
            max_fanout: usize::MAX,
            hedge_interval_ms: 0,
        }
    }
}

/// Normalize fanout config against current peer availability.
pub fn normalize_dispatch_config(
    dispatch: RequestDispatchConfig,
    available_peers: usize,
) -> RequestDispatchConfig {
    let mut cfg = dispatch;
    let cap = if cfg.max_fanout == 0 {
        available_peers
    } else {
        cfg.max_fanout.min(available_peers)
    };
    cfg.max_fanout = cap;
    cfg.initial_fanout = if cfg.initial_fanout == 0 {
        1
    } else {
        cfg.initial_fanout.min(cap.max(1))
    };
    cfg.hedge_fanout = if cfg.hedge_fanout == 0 {
        1
    } else {
        cfg.hedge_fanout.min(cap.max(1))
    };
    cfg
}

/// Build wave sizes for staged hedged dispatch.
pub fn build_hedged_wave_plan(peer_count: usize, dispatch: RequestDispatchConfig) -> Vec<usize> {
    if peer_count == 0 {
        return Vec::new();
    }
    let cap = dispatch.max_fanout.min(peer_count);
    if cap == 0 {
        return Vec::new();
    }

    let mut plan = Vec::new();
    let mut sent = 0usize;
    let first = dispatch.initial_fanout.min(cap).max(1);
    plan.push(first);
    sent += first;

    while sent < cap {
        let next = dispatch.hedge_fanout.min(cap - sent).max(1);
        plan.push(next);
        sent += next;
    }
    plan
}

/// Outcome returned after waiting on a hedged dispatch wave.
#[derive(Debug)]
pub enum HedgedWaveAction<T> {
    Continue,
    Success(T),
    Abort,
}

/// Run a staged hedged dispatch over peer index ranges.
///
/// This scheduler is shared by the reusable `MeshStoreCore` and the native
/// `hashtree-cli` mesh path so tests and production use the same wave timing.
pub async fn run_hedged_waves<T, SendWave, SendWaveFut, WaitWave, WaitWaveFut>(
    peer_count: usize,
    dispatch: RequestDispatchConfig,
    request_timeout: Duration,
    mut send_wave: SendWave,
    mut wait_wave: WaitWave,
) -> Option<T>
where
    SendWave: FnMut(Range<usize>) -> SendWaveFut,
    SendWaveFut: Future<Output = usize>,
    WaitWave: FnMut(Duration) -> WaitWaveFut,
    WaitWaveFut: Future<Output = HedgedWaveAction<T>>,
{
    let dispatch = normalize_dispatch_config(dispatch, peer_count);
    let wave_plan = build_hedged_wave_plan(peer_count, dispatch);
    if wave_plan.is_empty() {
        return None;
    }

    let deadline = Instant::now() + request_timeout;
    let mut sent_total = 0usize;
    let mut next_peer_idx = 0usize;

    for (wave_idx, wave_size) in wave_plan.iter().copied().enumerate() {
        let from = next_peer_idx;
        let to = (next_peer_idx + wave_size).min(peer_count);
        next_peer_idx = to;

        if from == to {
            continue;
        }

        sent_total += send_wave(from..to).await;
        if sent_total == 0 {
            if next_peer_idx >= peer_count {
                break;
            }
            continue;
        }

        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = deadline.saturating_duration_since(now);
        let is_last_wave = wave_idx + 1 == wave_plan.len() || next_peer_idx >= peer_count;
        let wait = if is_last_wave {
            remaining
        } else if dispatch.hedge_interval_ms == 0 {
            Duration::ZERO
        } else {
            Duration::from_millis(dispatch.hedge_interval_ms).min(remaining)
        };

        if wait.is_zero() {
            continue;
        }

        match wait_wave(wait).await {
            HedgedWaveAction::Continue => {}
            HedgedWaveAction::Success(value) => return Some(value),
            HedgedWaveAction::Abort => break,
        }
    }

    None
}

/// Keep selector membership aligned with currently connected peer IDs.
pub async fn sync_selector_peers(selector: &RwLock<PeerSelector>, current_peer_ids: &[String]) {
    let mut selector = selector.write().await;
    let current: HashSet<&str> = current_peer_ids.iter().map(String::as_str).collect();
    let known: Vec<String> = selector.all_stats().map(|s| s.peer_id.clone()).collect();
    for peer_id in known {
        if !current.contains(peer_id.as_str()) {
            selector.remove_peer(&peer_id);
        }
    }
    for peer_id in current_peer_ids {
        selector.add_peer(peer_id.clone());
    }
}

/// Response behavior profile for simulation/game-theory actors.
///
/// Defaults to honest behavior (always respond correctly, no extra delay).
#[derive(Debug, Clone, Copy)]
pub struct ResponseBehaviorConfig {
    /// Probability that a node drops a response even when it has data.
    pub drop_response_prob: f64,
    /// Probability that a node responds with corrupted payload.
    pub corrupt_response_prob: f64,
    /// Optional response delay to model slow/incompetent peers.
    pub extra_delay_ms: u64,
}

impl Default for ResponseBehaviorConfig {
    fn default() -> Self {
        Self {
            drop_response_prob: 0.0,
            corrupt_response_prob: 0.0,
            extra_delay_ms: 0,
        }
    }
}

impl ResponseBehaviorConfig {
    fn normalized(self) -> Self {
        Self {
            drop_response_prob: self.drop_response_prob.clamp(0.0, 1.0),
            corrupt_response_prob: self.corrupt_response_prob.clamp(0.0, 1.0),
            extra_delay_ms: self.extra_delay_ms,
        }
    }
}

/// Routing policy for request ordering + dispatch fanout.
#[derive(Debug, Clone)]
pub struct MeshRoutingConfig {
    pub selection_strategy: SelectionStrategy,
    pub fairness_enabled: bool,
    /// Blend weight for payment-priority ranking in selector (`0.0` disables).
    pub cashu_payment_weight: f64,
    /// Refuse serving peers that have reached this many unpaid post-delivery settlements.
    /// `0` disables refusal and only keeps metadata/downranking.
    pub cashu_payment_default_block_threshold: u64,
    /// Cashu mint URLs this node is willing to use for settlement.
    pub cashu_accepted_mints: Vec<String>,
    /// Preferred Cashu mint URL when initiating paid retrieval.
    pub cashu_default_mint: Option<String>,
    /// Baseline cap for accepting a peer-suggested mint outside the trusted set.
    pub cashu_peer_suggested_mint_base_cap_sat: u64,
    /// Additional sats allowed per successful delivery from that peer.
    pub cashu_peer_suggested_mint_success_step_sat: u64,
    /// Additional sats allowed per successful post-delivery payment received from that peer.
    pub cashu_peer_suggested_mint_receipt_step_sat: u64,
    /// Hard upper bound for any single peer-suggested mint quote we accept.
    pub cashu_peer_suggested_mint_max_cap_sat: u64,
    pub dispatch: RequestDispatchConfig,
    pub response_behavior: ResponseBehaviorConfig,
}

impl Default for MeshRoutingConfig {
    fn default() -> Self {
        Self {
            selection_strategy: SelectionStrategy::Weighted,
            fairness_enabled: true,
            cashu_payment_weight: 0.0,
            cashu_payment_default_block_threshold: 0,
            cashu_accepted_mints: Vec::new(),
            cashu_default_mint: None,
            cashu_peer_suggested_mint_base_cap_sat: 0,
            cashu_peer_suggested_mint_success_step_sat: 0,
            cashu_peer_suggested_mint_receipt_step_sat: 0,
            cashu_peer_suggested_mint_max_cap_sat: 0,
            dispatch: RequestDispatchConfig::default(),
            response_behavior: ResponseBehaviorConfig::default(),
        }
    }
}

/// Routed mesh store core that works with any storage backend and transport
/// implementation.
///
/// This is the shared code between production and simulation.
/// - Production: `MeshStoreCore<LmdbStore, NostrRelayTransport, WebRtcPeerLinkFactory>`
/// - Simulation: `MeshStoreCore<MemoryStore, MockRelayTransport, MockConnectionFactory>`
pub struct MeshStoreCore<S, R, F>
where
    S: Store + Send + Sync + 'static,
    R: SignalingTransport + Send + Sync + 'static,
    F: PeerLinkFactory + Send + Sync + 'static,
{
    /// Local backing store
    local_store: Arc<S>,
    /// Mesh router (handles peer discovery and connection)
    signaling: Arc<MeshRouter<R, F>>,
    /// Per-peer HTL config
    htl_configs: RwLock<HashMap<String, PeerHTLConfig>>,
    /// Pending requests we sent
    pending_requests: RwLock<HashMap<String, PendingRequest>>,
    /// Pending quote negotiations keyed by requested hash.
    pending_quotes: RwLock<HashMap<String, PendingQuoteRequest>>,
    /// Quotes we issued to peers and will accept exactly once until expiry.
    issued_quotes: RwLock<HashMap<(String, String, u64), IssuedQuote>>,
    /// Monotonic quote identifier generator.
    next_quote_id: RwLock<u64>,
    /// Adaptive selector for peer ordering.
    peer_selector: RwLock<PeerSelector>,
    /// Routing/dispatch configuration.
    routing: MeshRoutingConfig,
    /// Request timeout
    request_timeout: Duration,
    /// Debug mode
    debug: bool,
    /// Running flag
    running: RwLock<bool>,
}

impl<S, R, F> MeshStoreCore<S, R, F>
where
    S: Store + Send + Sync + 'static,
    R: SignalingTransport + Send + Sync + 'static,
    F: PeerLinkFactory + Send + Sync + 'static,
{
    /// Create a new routed mesh store core.
    pub fn new(
        local_store: Arc<S>,
        signaling: Arc<MeshRouter<R, F>>,
        request_timeout: Duration,
        debug: bool,
    ) -> Self {
        Self::new_with_routing(
            local_store,
            signaling,
            request_timeout,
            debug,
            Default::default(),
        )
    }

    /// Create a new routed mesh store core with explicit routing configuration.
    pub fn new_with_routing(
        local_store: Arc<S>,
        signaling: Arc<MeshRouter<R, F>>,
        request_timeout: Duration,
        debug: bool,
        routing: MeshRoutingConfig,
    ) -> Self {
        let mut selector = PeerSelector::with_strategy(routing.selection_strategy);
        selector.set_fairness(routing.fairness_enabled);
        selector.set_cashu_payment_weight(routing.cashu_payment_weight);
        Self {
            local_store,
            signaling,
            htl_configs: RwLock::new(HashMap::new()),
            pending_requests: RwLock::new(HashMap::new()),
            pending_quotes: RwLock::new(HashMap::new()),
            issued_quotes: RwLock::new(HashMap::new()),
            next_quote_id: RwLock::new(1),
            peer_selector: RwLock::new(selector),
            routing,
            request_timeout,
            debug,
            running: RwLock::new(false),
        }
    }

    /// Start the store (begin listening for messages)
    pub async fn start(&self) -> Result<(), TransportError> {
        *self.running.write().await = true;

        // Send initial hello
        self.signaling.send_hello(vec![]).await?;

        Ok(())
    }

    /// Stop the store
    pub async fn stop(&self) {
        *self.running.write().await = false;
    }

    /// Process incoming signaling message
    pub async fn process_signaling(&self, msg: SignalingMessage) -> Result<(), TransportError> {
        // When a new peer connects, initialize their HTL config
        let peer_id = msg.peer_id().to_string();
        {
            let mut configs = self.htl_configs.write().await;
            if !configs.contains_key(&peer_id) {
                configs.insert(peer_id.clone(), PeerHTLConfig::random());
            }
        }
        self.peer_selector.write().await.add_peer(peer_id);

        self.signaling.handle_message(msg).await
    }

    /// Get signaling manager reference
    pub fn signaling(&self) -> &Arc<MeshRouter<R, F>> {
        &self.signaling
    }

    fn response_behavior(&self) -> ResponseBehaviorConfig {
        self.routing.response_behavior.normalized()
    }

    fn deterministic_actor_draw_for(peer_id: &str, hash: &Hash, salt: u64) -> f64 {
        let mut hasher = DefaultHasher::new();
        peer_id.hash(&mut hasher);
        hash.hash(&mut hasher);
        salt.hash(&mut hasher);
        let v = hasher.finish();
        (v as f64) / (u64::MAX as f64)
    }

    fn deterministic_actor_draw(&self, hash: &Hash, salt: u64) -> f64 {
        Self::deterministic_actor_draw_for(self.signaling.peer_id(), hash, salt)
    }

    fn peer_metadata_pointer_slot_hash() -> Hash {
        hashtree_core::sha256(PEER_METADATA_POINTER_SLOT_KEY)
    }

    fn decode_hash_hex(hash_hex: &str) -> Result<Hash, StoreError> {
        let bytes = hex::decode(hash_hex)
            .map_err(|e| StoreError::Other(format!("Invalid hash hex: {e}")))?;
        if bytes.len() != 32 {
            return Err(StoreError::Other(format!(
                "Invalid hash length {}, expected 32 bytes",
                bytes.len()
            )));
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&bytes);
        Ok(hash)
    }

    fn should_drop_response(&self, hash: &Hash) -> bool {
        let p = self.response_behavior().drop_response_prob;
        if p <= 0.0 {
            return false;
        }
        self.deterministic_actor_draw(hash, 0xD0_D0_D0_D0_D0_D0_D0_D0) < p
    }

    fn should_corrupt_response(&self, hash: &Hash) -> bool {
        let p = self.response_behavior().corrupt_response_prob;
        if p <= 0.0 {
            return false;
        }
        self.deterministic_actor_draw(hash, 0xC0_C0_C0_C0_C0_C0_C0_C0) < p
    }

    async fn ordered_connected_peers(&self) -> Vec<String> {
        let current_peer_ids = self.signaling.peer_ids().await;
        if current_peer_ids.is_empty() {
            return Vec::new();
        }

        sync_selector_peers(&self.peer_selector, &current_peer_ids).await;
        let current_set: HashSet<&str> = current_peer_ids.iter().map(String::as_str).collect();
        let mut ordered_peer_ids = self.peer_selector.write().await.select_peers();
        ordered_peer_ids.retain(|peer_id| current_set.contains(peer_id.as_str()));
        if ordered_peer_ids.is_empty() {
            let mut fallback = current_peer_ids;
            fallback.sort();
            return fallback;
        }
        ordered_peer_ids
    }

    fn requested_quote_mint(&self) -> Option<&str> {
        if let Some(default_mint) = self.routing.cashu_default_mint.as_deref() {
            if self.routing.cashu_accepted_mints.is_empty()
                || self
                    .routing
                    .cashu_accepted_mints
                    .iter()
                    .any(|mint| mint == default_mint)
            {
                return Some(default_mint);
            }
        }

        self.routing
            .cashu_accepted_mints
            .first()
            .map(String::as_str)
    }

    fn choose_quote_mint(&self, requested_mint: Option<&str>) -> Option<String> {
        if let Some(requested_mint) = requested_mint {
            if self.accepts_quote_mint(Some(requested_mint)) {
                return Some(requested_mint.to_string());
            }
        }
        if let Some(default_mint) = self.routing.cashu_default_mint.as_ref() {
            return Some(default_mint.clone());
        }
        if let Some(first_mint) = self.routing.cashu_accepted_mints.first() {
            return Some(first_mint.clone());
        }
        requested_mint.map(str::to_string)
    }

    fn accepts_quote_mint(&self, mint_url: Option<&str>) -> bool {
        if self.routing.cashu_accepted_mints.is_empty() {
            return true;
        }

        let Some(mint_url) = mint_url else {
            return false;
        };
        self.routing
            .cashu_accepted_mints
            .iter()
            .any(|mint| mint == mint_url)
    }

    fn trusts_quote_mint(&self, mint_url: Option<&str>) -> bool {
        let Some(mint_url) = mint_url else {
            return self.routing.cashu_default_mint.is_none()
                && self.routing.cashu_accepted_mints.is_empty();
        };
        self.routing.cashu_default_mint.as_deref() == Some(mint_url)
            || self
                .routing
                .cashu_accepted_mints
                .iter()
                .any(|mint| mint == mint_url)
    }

    async fn peer_suggested_mint_cap_sat(&self, peer_id: &str) -> u64 {
        let base = self.routing.cashu_peer_suggested_mint_base_cap_sat;
        if base == 0 {
            return 0;
        }

        let selector = self.peer_selector.read().await;
        let Some(stats) = selector.get_stats(peer_id) else {
            let max_cap = self.routing.cashu_peer_suggested_mint_max_cap_sat;
            return if max_cap > 0 { base.min(max_cap) } else { base };
        };

        if stats.cashu_payment_defaults > 0
            && stats.cashu_payment_defaults >= stats.cashu_payment_receipts
        {
            return 0;
        }

        let success_bonus = stats
            .successes
            .saturating_mul(self.routing.cashu_peer_suggested_mint_success_step_sat);
        let receipt_bonus = stats
            .cashu_payment_receipts
            .saturating_mul(self.routing.cashu_peer_suggested_mint_receipt_step_sat);
        let mut cap = base
            .saturating_add(success_bonus)
            .saturating_add(receipt_bonus);
        let max_cap = self.routing.cashu_peer_suggested_mint_max_cap_sat;
        if max_cap > 0 {
            cap = cap.min(max_cap);
        }
        cap
    }

    async fn should_accept_quote_response(
        &self,
        from_peer: &str,
        preferred_mint_url: Option<&str>,
        offered_payment_sat: u64,
        res: &DataQuoteResponse,
    ) -> bool {
        let Some(payment_sat) = res.p else {
            return false;
        };
        if payment_sat > offered_payment_sat {
            return false;
        }

        let response_mint = res.m.as_deref();
        if response_mint == preferred_mint_url {
            return true;
        }
        if self.trusts_quote_mint(response_mint) {
            return true;
        }
        if response_mint.is_none() {
            return false;
        }

        payment_sat <= self.peer_suggested_mint_cap_sat(from_peer).await
    }

    async fn issue_quote(
        &self,
        peer_id: &str,
        hash_key: &str,
        payment_sat: u64,
        ttl_ms: u32,
        mint_url: Option<&str>,
    ) -> u64 {
        let quote_id = {
            let mut next = self.next_quote_id.write().await;
            let quote_id = *next;
            *next = next.saturating_add(1);
            quote_id
        };

        let expires_at = Instant::now() + Duration::from_millis(ttl_ms as u64);
        self.issued_quotes.write().await.insert(
            (peer_id.to_string(), hash_key.to_string(), quote_id),
            IssuedQuote {
                expires_at,
                payment_sat,
                mint_url: mint_url.map(str::to_string),
            },
        );
        quote_id
    }

    async fn take_valid_quote(&self, peer_id: &str, hash_key: &str, quote_id: u64) -> bool {
        let key = (peer_id.to_string(), hash_key.to_string(), quote_id);
        let Some(quote) = self.issued_quotes.write().await.remove(&key) else {
            return false;
        };
        quote.expires_at > Instant::now()
    }

    async fn send_request_to_peer(
        &self,
        peer_id: &str,
        hash: &Hash,
        quote_id: Option<u64>,
    ) -> bool {
        let channel = match self.signaling.get_channel(peer_id).await {
            Some(c) => c,
            None => return false,
        };

        let htl_config = {
            let configs = self.htl_configs.read().await;
            configs
                .get(peer_id)
                .cloned()
                .unwrap_or_else(PeerHTLConfig::random)
        };

        let send_htl = htl_config.decrement(MAX_HTL);
        let req = match quote_id {
            Some(quote_id) => create_request_with_quote(hash, send_htl, quote_id),
            None => create_request(hash, send_htl),
        };
        let request_bytes = encode_request(&req);

        {
            let mut selector = self.peer_selector.write().await;
            selector.record_request(peer_id, request_bytes.len() as u64);
        }

        match channel.send(request_bytes).await {
            Ok(()) => true,
            Err(_) => {
                self.peer_selector.write().await.record_failure(peer_id);
                false
            }
        }
    }

    async fn send_quote_request_to_peer(
        &self,
        peer_id: &str,
        hash: &Hash,
        payment_sat: u64,
        ttl_ms: u32,
        mint_url: Option<&str>,
    ) -> bool {
        let channel = match self.signaling.get_channel(peer_id).await {
            Some(c) => c,
            None => return false,
        };

        let req = create_quote_request(hash, ttl_ms, payment_sat, mint_url);
        let request_bytes = encode_quote_request(&req);

        match channel.send(request_bytes).await {
            Ok(()) => true,
            Err(_) => false,
        }
    }

    /// Get peer count
    pub async fn peer_count(&self) -> usize {
        self.signaling.peer_count().await
    }

    /// Check if we need more peers
    pub async fn needs_peers(&self) -> bool {
        self.signaling.needs_peers().await
    }

    /// Re-broadcast hello to refresh discovery as topology changes.
    pub async fn send_hello(&self) -> Result<(), TransportError> {
        self.signaling.send_hello(vec![]).await
    }

    /// Drain all currently available peer-link messages and handle them.
    ///
    /// This keeps the message pump logic shared between simulation and the
    /// default production wrapper instead of duplicating per-channel loops.
    pub async fn drain_available_data_messages(&self) -> DataPumpStats {
        let mut stats = DataPumpStats::default();
        let peer_ids = self.signaling.peer_ids().await;
        for peer_id in peer_ids {
            let Some(channel) = self.signaling.get_channel(&peer_id).await else {
                continue;
            };

            while let Some(data) = channel.try_recv() {
                stats.processed += 1;
                stats.processed_bytes += data.len() as u64;
                if let Some(msg) = parse_message(&data) {
                    match msg {
                        DataMessage::Request(_) => stats.request_messages += 1,
                        DataMessage::Response(_) => stats.response_messages += 1,
                        DataMessage::QuoteRequest(_) => stats.quote_request_messages += 1,
                        DataMessage::QuoteResponse(_) => stats.quote_response_messages += 1,
                    }
                }
                self.handle_data_message(&peer_id, &data).await;
            }
        }
        stats
    }

    /// Apply an out-of-band payment credit to a peer's routing priority.
    pub async fn record_cashu_payment_for_peer(&self, peer_id: &str, amount_sat: u64) {
        self.peer_selector
            .write()
            .await
            .record_cashu_payment(peer_id, amount_sat);
    }

    /// Record a post-delivery payment we received from a peer.
    pub async fn record_cashu_receipt_from_peer(&self, peer_id: &str, amount_sat: u64) {
        self.peer_selector
            .write()
            .await
            .record_cashu_receipt(peer_id, amount_sat);
    }

    /// Record that a peer failed to pay after we delivered successfully.
    pub async fn record_cashu_payment_default_from_peer(&self, peer_id: &str) {
        self.peer_selector
            .write()
            .await
            .record_cashu_payment_default(peer_id);
    }

    /// Snapshot routing/selection summary for inspection/debugging.
    pub async fn selector_summary(&self) -> crate::peer_selector::SelectorSummary {
        self.peer_selector.read().await.summary()
    }

    fn should_refuse_requests_from_peer(&self, selector: &PeerSelector, peer_id: &str) -> bool {
        selector.is_peer_blocked_for_payment_defaults(
            peer_id,
            self.routing.cashu_payment_default_block_threshold,
        )
    }

    /// Export live peer metadata for inspection/debugging.
    pub async fn peer_metadata_snapshot(&self) -> PeerMetadataSnapshot {
        self.peer_selector
            .read()
            .await
            .export_peer_metadata_snapshot()
    }

    /// Snapshot current peer metadata and persist it into `local_store`.
    ///
    /// Uses content-addressed storage for the snapshot body and a reserved
    /// mutable pointer slot for the "latest snapshot hash".
    pub async fn persist_peer_metadata(&self) -> Result<Hash, StoreError> {
        let snapshot = self
            .peer_selector
            .read()
            .await
            .export_peer_metadata_snapshot();
        let bytes = serde_json::to_vec(&snapshot).map_err(|e| {
            StoreError::Other(format!("Failed to encode peer metadata snapshot: {e}"))
        })?;
        let snapshot_hash = hashtree_core::sha256(&bytes);
        let _ = self.local_store.put(snapshot_hash, bytes).await?;

        let pointer_slot = Self::peer_metadata_pointer_slot_hash();
        let pointer_bytes = hex::encode(snapshot_hash).into_bytes();
        let _ = self.local_store.delete(&pointer_slot).await?;
        let _ = self.local_store.put(pointer_slot, pointer_bytes).await?;

        Ok(snapshot_hash)
    }

    /// Load persisted peer metadata from `local_store` if available.
    pub async fn load_peer_metadata(&self) -> Result<bool, StoreError> {
        let pointer_slot = Self::peer_metadata_pointer_slot_hash();
        let Some(pointer_bytes) = self.local_store.get(&pointer_slot).await? else {
            return Ok(false);
        };
        let pointer_hex = std::str::from_utf8(&pointer_bytes).map_err(|e| {
            StoreError::Other(format!("Peer metadata pointer is not valid UTF-8: {e}"))
        })?;
        let snapshot_hash = Self::decode_hash_hex(pointer_hex.trim())?;

        let Some(snapshot_bytes) = self.local_store.get(&snapshot_hash).await? else {
            return Ok(false);
        };
        let snapshot: PeerMetadataSnapshot =
            serde_json::from_slice(&snapshot_bytes).map_err(|e| {
                StoreError::Other(format!("Failed to decode peer metadata snapshot: {e}"))
            })?;
        self.peer_selector
            .write()
            .await
            .import_peer_metadata_snapshot(&snapshot);
        Ok(true)
    }

    /// Request data from peers after negotiating a paid quote.
    ///
    /// If quote negotiation fails or the quoted peer does not deliver, the store
    /// falls back to the normal unpaid retrieval path to preserve liveness.
    pub async fn get_with_quote(
        &self,
        hash: &Hash,
        payment_sat: u64,
        quote_ttl: Duration,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        if let Some(data) = self.local_store.get(hash).await? {
            return Ok(Some(data));
        }
        Ok(self
            .request_from_peers_with_quote(hash, payment_sat, quote_ttl)
            .await)
    }

    async fn request_from_peers_with_quote(
        &self,
        hash: &Hash,
        payment_sat: u64,
        quote_ttl: Duration,
    ) -> Option<Vec<u8>> {
        let ordered_peer_ids = self.ordered_connected_peers().await;
        if ordered_peer_ids.is_empty() {
            return None;
        }

        if let Some(quote) = self
            .request_quote_from_peers(hash, payment_sat, quote_ttl, &ordered_peer_ids)
            .await
        {
            if let Some(data) = self
                .request_from_single_peer(hash, &quote.peer_id, Some(quote.quote_id))
                .await
            {
                return Some(data);
            }
        }

        self.request_from_ordered_peers(hash, &ordered_peer_ids)
            .await
    }

    async fn request_quote_from_peers(
        &self,
        hash: &Hash,
        payment_sat: u64,
        quote_ttl: Duration,
        ordered_peer_ids: &[String],
    ) -> Option<NegotiatedQuote> {
        if ordered_peer_ids.is_empty() {
            return None;
        }
        let ttl_ms = quote_ttl.as_millis().min(u32::MAX as u128) as u32;
        if ttl_ms == 0 {
            return None;
        }
        let requested_mint = self.requested_quote_mint().map(str::to_string);

        let hash_key = hash_to_key(hash);
        let (tx, rx) = oneshot::channel();
        self.pending_quotes.write().await.insert(
            hash_key.clone(),
            PendingQuoteRequest {
                response_tx: tx,
                preferred_mint_url: requested_mint.clone(),
                offered_payment_sat: payment_sat,
            },
        );

        let rx = Arc::new(Mutex::new(rx));
        let result = run_hedged_waves(
            ordered_peer_ids.len(),
            self.routing.dispatch,
            self.request_timeout,
            |range| {
                let wave_peer_ids = ordered_peer_ids[range].to_vec();
                let requested_mint = requested_mint.clone();
                let hash = *hash;
                async move {
                    let mut sent = 0usize;
                    for peer_id in wave_peer_ids {
                        if self
                            .send_quote_request_to_peer(
                                &peer_id,
                                &hash,
                                payment_sat,
                                ttl_ms,
                                requested_mint.as_deref(),
                            )
                            .await
                        {
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
        let _ = self.pending_quotes.write().await.remove(&hash_key);
        result
    }

    async fn request_from_single_peer(
        &self,
        hash: &Hash,
        peer_id: &str,
        quote_id: Option<u64>,
    ) -> Option<Vec<u8>> {
        let hash_key = hash_to_key(hash);
        let (tx, rx) = oneshot::channel();
        self.pending_requests.write().await.insert(
            hash_key.clone(),
            PendingRequest {
                response_tx: tx,
                started_at: Instant::now(),
                queried_peers: vec![peer_id.to_string()],
            },
        );

        let mut rx = rx;
        if !self.send_request_to_peer(peer_id, hash, quote_id).await {
            let _ = self.pending_requests.write().await.remove(&hash_key);
            return None;
        }

        if let Ok(Ok(Some(data))) = tokio::time::timeout(self.request_timeout, &mut rx).await {
            if hashtree_core::sha256(&data) == *hash {
                let _ = self.local_store.put(*hash, data.clone()).await;
                return Some(data);
            }
        }

        if let Some(pending) = self.pending_requests.write().await.remove(&hash_key) {
            for peer_id in pending.queried_peers {
                self.peer_selector.write().await.record_timeout(&peer_id);
            }
        }
        None
    }

    async fn request_from_ordered_peers(
        &self,
        hash: &Hash,
        ordered_peer_ids: &[String],
    ) -> Option<Vec<u8>> {
        let hash_key = hash_to_key(hash);
        let (tx, rx) = oneshot::channel();
        self.pending_requests.write().await.insert(
            hash_key.clone(),
            PendingRequest {
                response_tx: tx,
                started_at: Instant::now(),
                queried_peers: Vec::new(),
            },
        );

        let rx = Arc::new(Mutex::new(rx));
        let result = run_hedged_waves(
            ordered_peer_ids.len(),
            self.routing.dispatch,
            self.request_timeout,
            |range| {
                let wave_peer_ids = ordered_peer_ids[range].to_vec();
                let hash = *hash;
                let hash_key = hash_key.clone();
                async move {
                    let mut sent = 0usize;
                    for peer_id in wave_peer_ids {
                        if self.send_request_to_peer(&peer_id, &hash, None).await {
                            sent += 1;
                            if let Some(pending) =
                                self.pending_requests.write().await.get_mut(&hash_key)
                            {
                                pending.queried_peers.push(peer_id);
                            }
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
                        Ok(Ok(Some(data))) if hashtree_core::sha256(&data) == *hash => {
                            HedgedWaveAction::Success(data)
                        }
                        Ok(Ok(Some(_))) => HedgedWaveAction::Continue,
                        Ok(Ok(None)) | Ok(Err(_)) => HedgedWaveAction::Abort,
                        Err(_) => HedgedWaveAction::Continue,
                    }
                }
            },
        )
        .await;

        let Some(data) = result else {
            if let Some(pending) = self.pending_requests.write().await.remove(&hash_key) {
                for peer_id in pending.queried_peers {
                    self.peer_selector.write().await.record_timeout(&peer_id);
                }
            }
            return None;
        };

        let _ = self.local_store.put(*hash, data.clone()).await;
        Some(data)
    }

    /// Request data from peers
    async fn request_from_peers(&self, hash: &Hash) -> Option<Vec<u8>> {
        let ordered_peer_ids = self.ordered_connected_peers().await;
        if ordered_peer_ids.is_empty() {
            return None;
        }
        self.request_from_ordered_peers(hash, &ordered_peer_ids)
            .await
    }

    async fn complete_pending_response(&self, from_peer: &str, hash_key: String, payload: Vec<u8>) {
        if let Some(pending) = self.pending_requests.write().await.remove(&hash_key) {
            let rtt_ms = pending.started_at.elapsed().as_millis() as u64;
            self.peer_selector.write().await.record_success(
                from_peer,
                rtt_ms,
                payload.len() as u64,
            );
            let _ = pending.response_tx.send(Some(payload));
        }
    }

    async fn handle_quote_response_message(&self, from_peer: &str, res: DataQuoteResponse) {
        if !res.a {
            return;
        }

        let Some(quote_id) = res.q else {
            return;
        };

        let hash_key = hash_to_key(&res.h);
        let (preferred_mint_url, offered_payment_sat) = {
            let pending_quotes = self.pending_quotes.read().await;
            let Some(pending) = pending_quotes.get(&hash_key) else {
                return;
            };
            (
                pending.preferred_mint_url.clone(),
                pending.offered_payment_sat,
            )
        };
        if !self
            .should_accept_quote_response(
                from_peer,
                preferred_mint_url.as_deref(),
                offered_payment_sat,
                &res,
            )
            .await
        {
            return;
        }
        let mut pending_quotes = self.pending_quotes.write().await;
        if let Some(pending) = pending_quotes.remove(&hash_key) {
            let _ = pending.response_tx.send(Some(NegotiatedQuote {
                peer_id: from_peer.to_string(),
                quote_id,
                mint_url: res.m,
            }));
        }
    }

    async fn handle_response_message(&self, from_peer: &str, res: crate::protocol::DataResponse) {
        let hash_key = hash_to_key(&res.h);
        let hash = match crate::protocol::bytes_to_hash(&res.h) {
            Some(h) => h,
            None => return,
        };

        // Ignore malformed/corrupt payload and keep waiting for a valid response.
        if hashtree_core::sha256(&res.d) != hash {
            self.peer_selector.write().await.record_failure(from_peer);
            if self.debug {
                println!("[MeshStoreCore] Ignoring invalid response payload for {hash_key}");
            }
            return;
        }

        self.complete_pending_response(from_peer, hash_key, res.d)
            .await;
    }

    async fn handle_quote_request_message(&self, from_peer: &str, req: DataQuoteRequest) {
        let hash = match crate::protocol::bytes_to_hash(&req.h) {
            Some(h) => h,
            None => return,
        };
        let hash_key = hash_to_key(&hash);

        {
            let selector = self.peer_selector.read().await;
            if self.should_refuse_requests_from_peer(&selector, from_peer) {
                if self.debug {
                    println!(
                        "[MeshStoreCore] Refusing quote request from delinquent peer {}",
                        from_peer
                    );
                }
                return;
            }
        }

        let chosen_mint = self.choose_quote_mint(req.m.as_deref());
        let can_serve = self.local_store.has(&hash).await.ok().unwrap_or(false)
            && !self.should_drop_response(&hash)
            && !self.should_corrupt_response(&hash);

        let res = if can_serve {
            let quote_id = self
                .issue_quote(from_peer, &hash_key, req.p, req.t, chosen_mint.as_deref())
                .await;
            create_quote_response_available(&hash, quote_id, req.p, req.t, chosen_mint.as_deref())
        } else {
            create_quote_response_unavailable(&hash)
        };
        let response_bytes = encode_quote_response(&res);
        if let Some(channel) = self.signaling.get_channel(from_peer).await {
            let _ = channel.send(response_bytes).await;
        }
    }

    async fn handle_request_message(&self, from_peer: &str, req: crate::protocol::DataRequest) {
        let hash = match crate::protocol::bytes_to_hash(&req.h) {
            Some(h) => h,
            None => return,
        };
        let hash_key = hash_to_key(&hash);

        {
            let selector = self.peer_selector.read().await;
            if self.should_refuse_requests_from_peer(&selector, from_peer) {
                if self.debug {
                    println!(
                        "[MeshStoreCore] Refusing request from delinquent peer {}",
                        from_peer
                    );
                }
                return;
            }
        }

        if let Some(quote_id) = req.q {
            if !self.take_valid_quote(from_peer, &hash_key, quote_id).await {
                if self.debug {
                    println!(
                        "[MeshStoreCore] Refusing request with invalid or expired quote {} from {}",
                        quote_id, from_peer
                    );
                }
                return;
            }
        }

        // Check local store
        if let Ok(Some(mut data)) = self.local_store.get(&hash).await {
            if self.should_drop_response(&hash) {
                if self.debug {
                    println!(
                        "[MeshStoreCore] Dropping response for {} due to actor profile",
                        hash_to_key(&hash)
                    );
                }
                return;
            }

            let behavior = self.response_behavior();
            if behavior.extra_delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(behavior.extra_delay_ms)).await;
            }

            if self.should_corrupt_response(&hash) {
                if data.is_empty() {
                    data.push(0x80);
                } else {
                    data[0] ^= 0x80;
                }
            }

            // Send response
            let res = create_response(&hash, data);
            let response_bytes = encode_response(&res);
            if let Some(channel) = self.signaling.get_channel(from_peer).await {
                let _ = channel.send(response_bytes).await;
            }
        }
        // For now, don't forward - keep it simple
    }

    /// Handle incoming data message
    pub async fn handle_data_message(&self, from_peer: &str, data: &[u8]) {
        let parsed = match parse_message(data) {
            Some(m) => m,
            None => return,
        };

        match parsed {
            DataMessage::Request(req) => {
                self.handle_request_message(from_peer, req).await;
            }
            DataMessage::Response(res) => {
                self.handle_response_message(from_peer, res).await;
            }
            DataMessage::QuoteRequest(req) => {
                self.handle_quote_request_message(from_peer, req).await;
            }
            DataMessage::QuoteResponse(res) => {
                self.handle_quote_response_message(from_peer, res).await;
            }
        }
    }
}

#[async_trait]
impl<S, R, F> Store for MeshStoreCore<S, R, F>
where
    S: Store + Send + Sync + 'static,
    R: SignalingTransport + Send + Sync + 'static,
    F: PeerLinkFactory + Send + Sync + 'static,
{
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.local_store.put(hash, data).await
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        // Try local first
        if let Some(data) = self.local_store.get(hash).await? {
            return Ok(Some(data));
        }

        // Try peers
        Ok(self.request_from_peers(hash).await)
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.local_store.has(hash).await
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.local_store.delete(hash).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashtree_core::MemoryStore;
    use std::sync::Arc;
    use std::sync::OnceLock;
    use std::time::Duration;

    type TestStore = MeshStoreCore<
        MemoryStore,
        crate::mock::MockRelayTransport,
        crate::mock::MockConnectionFactory,
    >;

    struct TestNode {
        store: Arc<TestStore>,
        local_store: Arc<MemoryStore>,
        transport: Arc<crate::mock::MockRelayTransport>,
    }

    fn mock_network_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    fn make_test_store(local_store: Arc<MemoryStore>, node_id: &str) -> TestStore {
        make_test_store_with_routing(local_store, node_id, MeshRoutingConfig::default())
    }

    fn make_test_store_with_routing(
        local_store: Arc<MemoryStore>,
        node_id: &str,
        routing: MeshRoutingConfig,
    ) -> TestStore {
        let relay = crate::mock::MockRelay::new();
        let transport = Arc::new(relay.create_transport(node_id.to_string()));
        let conn_factory = Arc::new(crate::mock::MockConnectionFactory::new(
            node_id.to_string(),
            0,
        ));
        let signaling = Arc::new(crate::signaling::MeshRouter::new(
            node_id.to_string(),
            transport,
            conn_factory,
            crate::types::PoolSettings::default(),
            false,
        ));

        TestStore::new_with_routing(
            local_store,
            signaling,
            Duration::from_millis(200),
            false,
            routing,
        )
    }

    fn make_shared_test_node(
        relay: Arc<crate::mock::MockRelay>,
        node_id: &str,
        routing: MeshRoutingConfig,
    ) -> TestNode {
        let transport = Arc::new(relay.create_transport(node_id.to_string()));
        let conn_factory = Arc::new(crate::mock::MockConnectionFactory::new(
            node_id.to_string(),
            0,
        ));
        let signaling = Arc::new(crate::signaling::MeshRouter::new(
            node_id.to_string(),
            transport.clone(),
            conn_factory,
            crate::types::PoolSettings::default(),
            false,
        ));
        let local_store = Arc::new(MemoryStore::new());
        let store = Arc::new(TestStore::new_with_routing(
            local_store.clone(),
            signaling,
            Duration::from_millis(120),
            false,
            routing,
        ));

        TestNode {
            store,
            local_store,
            transport,
        }
    }

    async fn pump_test_signaling(nodes: &[&TestNode]) -> usize {
        let mut processed = 0usize;
        for node in nodes {
            while let Some(msg) = node.transport.try_recv() {
                node.store
                    .process_signaling(msg)
                    .await
                    .expect("process signaling");
                processed += 1;
            }
        }
        processed
    }

    async fn pump_test_data(nodes: &[&TestNode]) -> usize {
        let mut processed = 0usize;
        for node in nodes {
            let peer_ids = node.store.signaling().peer_ids().await;
            for peer_id in peer_ids {
                let Some(channel) = node.store.signaling().get_channel(&peer_id).await else {
                    continue;
                };
                while let Some(data) = channel.try_recv() {
                    node.store.handle_data_message(&peer_id, &data).await;
                    processed += 1;
                }
            }
        }
        processed
    }

    async fn pump_test_network(nodes: &[&TestNode], max_steps: usize) {
        for _ in 0..max_steps {
            let signaling = pump_test_signaling(nodes).await;
            let data = pump_test_data(nodes).await;
            if signaling + data == 0 {
                tokio::task::yield_now().await;
            }
        }
    }

    async fn run_get_with_pumps(
        requester: Arc<TestStore>,
        hash: Hash,
        nodes: &[&TestNode],
    ) -> Option<Vec<u8>> {
        let task = tokio::spawn(async move { requester.get(&hash).await.ok().flatten() });
        let started = Instant::now();

        loop {
            if task.is_finished() {
                return task.await.expect("request task join");
            }

            if started.elapsed() > Duration::from_secs(1) {
                task.abort();
                return None;
            }

            pump_test_network(nodes, 4).await;
        }
    }

    async fn run_bad_peer_series(strategy: SelectionStrategy) -> usize {
        let _guard = mock_network_lock().lock().await;
        crate::mock::clear_channel_registry().await;

        let relay = crate::mock::MockRelay::new();
        let requester = make_shared_test_node(
            relay.clone(),
            "requester-reject",
            MeshRoutingConfig {
                selection_strategy: strategy,
                fairness_enabled: false,
                dispatch: RequestDispatchConfig {
                    initial_fanout: 1,
                    hedge_fanout: 1,
                    max_fanout: 1,
                    hedge_interval_ms: 5,
                },
                ..Default::default()
            },
        );
        let bad = make_shared_test_node(
            relay.clone(),
            "a-bad",
            MeshRoutingConfig {
                response_behavior: ResponseBehaviorConfig {
                    drop_response_prob: 1.0,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let honest = make_shared_test_node(relay, "b-honest", MeshRoutingConfig::default());
        let nodes = [&requester, &bad, &honest];

        for node in &nodes {
            node.transport
                .connect(&[])
                .await
                .expect("connect transport");
            node.store.start().await.expect("start store");
        }
        pump_test_network(&nodes, 24).await;

        let mut successes = 0usize;
        for round in 0..6 {
            let payload = format!("payload-{round}").into_bytes();
            let hash = hashtree_core::sha256(&payload);
            let _ = bad.local_store.put(hash, payload.clone()).await;
            let _ = honest.local_store.put(hash, payload.clone()).await;

            let result = run_get_with_pumps(requester.store.clone(), hash, &nodes).await;
            if result.as_ref() == Some(&payload) {
                successes += 1;
            }
        }

        crate::mock::clear_channel_registry().await;
        successes
    }

    #[test]
    fn test_hedged_wave_plan_flood_all() {
        let plan = build_hedged_wave_plan(7, RequestDispatchConfig::default());
        assert_eq!(plan, vec![7]);
    }

    #[test]
    fn test_hedged_wave_plan_staged() {
        let plan = build_hedged_wave_plan(
            10,
            RequestDispatchConfig {
                initial_fanout: 2,
                hedge_fanout: 3,
                max_fanout: 8,
                hedge_interval_ms: 25,
            },
        );
        assert_eq!(plan, vec![2, 3, 3]);
    }

    #[tokio::test]
    async fn test_run_hedged_waves_uses_zero_interval_as_immediate_hedge() {
        let waits = Arc::new(std::sync::Mutex::new(Vec::new()));
        let waits_clone = waits.clone();

        let result = run_hedged_waves(
            3,
            RequestDispatchConfig {
                initial_fanout: 1,
                hedge_fanout: 1,
                max_fanout: 3,
                hedge_interval_ms: 0,
            },
            Duration::from_millis(25),
            |_range| async { 1 },
            move |wait| {
                let waits = waits_clone.clone();
                async move {
                    waits.lock().expect("wait lock").push(wait);
                    HedgedWaveAction::<()>::Continue
                }
            },
        )
        .await;

        assert!(result.is_none());
        let waits = waits.lock().expect("wait lock");
        assert_eq!(waits.len(), 1);
        assert!(waits[0] <= Duration::from_millis(25));
    }

    #[test]
    fn test_response_behavior_normalization_clamps_probs() {
        let raw = ResponseBehaviorConfig {
            drop_response_prob: -1.5,
            corrupt_response_prob: 9.0,
            extra_delay_ms: 12,
        };
        let normalized = raw.normalized();
        assert_eq!(normalized.drop_response_prob, 0.0);
        assert_eq!(normalized.corrupt_response_prob, 1.0);
        assert_eq!(normalized.extra_delay_ms, 12);
    }

    #[test]
    fn test_actor_draw_is_deterministic_per_peer_hash_and_salt() {
        let hash = hashtree_core::sha256(b"deterministic");
        let a = TestStore::deterministic_actor_draw_for("peer-a", &hash, 7);
        let b = TestStore::deterministic_actor_draw_for("peer-a", &hash, 7);
        assert!((a - b).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn test_load_peer_metadata_returns_false_when_missing() {
        let local_store = Arc::new(MemoryStore::new());
        let store = make_test_store(local_store, "0");
        assert!(!store.load_peer_metadata().await.expect("load result"));
    }

    #[tokio::test]
    async fn test_persist_and_load_peer_metadata_with_existing_store_adapter() {
        let local_store = Arc::new(MemoryStore::new());
        let writer = make_test_store(local_store.clone(), "0");
        {
            let mut selector = writer.peer_selector.write().await;
            selector.add_peer("npub1stable");
            selector.record_request("npub1stable", 64);
            selector.record_success("npub1stable", 35, 1024);
            selector.record_cashu_payment("npub1stable", 120);
            selector.record_cashu_receipt("npub1stable", 40);
            selector.record_cashu_payment_default("npub1stable");
        }

        let snapshot_hash = writer
            .persist_peer_metadata()
            .await
            .expect("persist peer metadata");
        assert!(local_store
            .get(&snapshot_hash)
            .await
            .expect("snapshot lookup")
            .is_some());

        let reader = make_test_store(local_store, "1");
        assert!(reader
            .load_peer_metadata()
            .await
            .expect("load peer metadata snapshot"));

        let mut selector = reader.peer_selector.write().await;
        selector.add_peer("npub1stable");
        let stats = selector
            .get_stats("npub1stable")
            .expect("restored peer stats");
        assert_eq!(stats.requests_sent, 1);
        assert_eq!(stats.successes, 1);
        assert_eq!(stats.cashu_paid_sat, 120);
        assert_eq!(stats.cashu_received_sat, 40);
        assert_eq!(stats.cashu_payment_receipts, 1);
        assert_eq!(stats.cashu_payment_defaults, 1);
    }

    #[tokio::test]
    async fn test_should_refuse_requests_from_peer_after_payment_defaults() {
        let local_store = Arc::new(MemoryStore::new());
        let store = make_test_store_with_routing(
            local_store,
            "0",
            MeshRoutingConfig {
                cashu_payment_default_block_threshold: 1,
                ..Default::default()
            },
        );
        store.record_cashu_payment_default_from_peer("peer-a").await;

        let selector = store.peer_selector.read().await;
        assert!(store.should_refuse_requests_from_peer(&selector, "peer-a"));
        assert!(!store.should_refuse_requests_from_peer(&selector, "peer-b"));
    }

    #[tokio::test]
    async fn test_take_valid_quote_consumes_once_and_rejects_expired_quotes() {
        let local_store = Arc::new(MemoryStore::new());
        let store = make_test_store(local_store, "0");
        let hash = hashtree_core::sha256(b"quote-test");
        let hash_key = hash_to_key(&hash);

        {
            let mut issued = store.issued_quotes.write().await;
            issued.insert(
                ("peer-a".to_string(), hash_key.clone(), 11),
                IssuedQuote {
                    expires_at: Instant::now() + Duration::from_secs(1),
                    payment_sat: 5,
                    mint_url: Some("https://mint-a.example".to_string()),
                },
            );
            issued.insert(
                ("peer-a".to_string(), hash_key.clone(), 12),
                IssuedQuote {
                    expires_at: Instant::now() - Duration::from_millis(1),
                    payment_sat: 5,
                    mint_url: Some("https://mint-a.example".to_string()),
                },
            );
        }

        assert!(store.take_valid_quote("peer-a", &hash_key, 11).await);
        assert!(!store.take_valid_quote("peer-a", &hash_key, 11).await);
        assert!(!store.take_valid_quote("peer-a", &hash_key, 12).await);
    }

    async fn run_quote_with_pumps(
        requester: Arc<TestStore>,
        hash: Hash,
        payment_sat: u64,
        quote_ttl: Duration,
        peer_ids: Vec<String>,
        nodes: &[&TestNode],
    ) -> Option<NegotiatedQuote> {
        let task = tokio::spawn(async move {
            requester
                .request_quote_from_peers(&hash, payment_sat, quote_ttl, &peer_ids)
                .await
        });
        let started = Instant::now();

        loop {
            if task.is_finished() {
                return task.await.expect("quote task join");
            }
            if started.elapsed() > Duration::from_secs(1) {
                task.abort();
                return None;
            }
            pump_test_network(nodes, 4).await;
        }
    }

    #[tokio::test]
    async fn test_request_quote_from_peers_rejects_unaccepted_mint() {
        let _guard = mock_network_lock().lock().await;
        crate::mock::clear_channel_registry().await;

        let relay = crate::mock::MockRelay::new();
        let requester = make_shared_test_node(
            relay.clone(),
            "requester-reject",
            MeshRoutingConfig {
                cashu_accepted_mints: vec!["https://mint-a.example".to_string()],
                cashu_default_mint: Some("https://mint-a.example".to_string()),
                dispatch: RequestDispatchConfig {
                    initial_fanout: 1,
                    hedge_fanout: 1,
                    max_fanout: 1,
                    hedge_interval_ms: 5,
                },
                ..Default::default()
            },
        );
        let provider = make_shared_test_node(
            relay,
            "provider-reject",
            MeshRoutingConfig {
                cashu_accepted_mints: vec!["https://mint-b.example".to_string()],
                ..Default::default()
            },
        );
        let nodes = [&requester, &provider];

        requester.transport.connect(&[]).await.expect("connect");
        provider.transport.connect(&[]).await.expect("connect");
        requester.store.start().await.expect("start");
        provider.store.start().await.expect("start");
        pump_test_network(&nodes, 24).await;

        let payload = b"quoted-data".to_vec();
        let hash = hashtree_core::sha256(&payload);
        provider.local_store.put(hash, payload).await.expect("put");

        let quote = run_quote_with_pumps(
            requester.store.clone(),
            hash,
            9,
            Duration::from_millis(80),
            vec!["provider-reject".to_string()],
            &nodes,
        )
        .await;
        assert!(
            quote.is_none(),
            "expected quote to be rejected on mint mismatch"
        );
    }

    #[tokio::test]
    async fn test_request_quote_from_peers_accepts_small_peer_suggested_mint_under_cap() {
        let _guard = mock_network_lock().lock().await;
        crate::mock::clear_channel_registry().await;

        let relay = crate::mock::MockRelay::new();
        let requester = make_shared_test_node(
            relay.clone(),
            "requester-suggested",
            MeshRoutingConfig {
                cashu_accepted_mints: vec!["https://mint-a.example".to_string()],
                cashu_default_mint: Some("https://mint-a.example".to_string()),
                cashu_peer_suggested_mint_base_cap_sat: 3,
                cashu_peer_suggested_mint_max_cap_sat: 3,
                dispatch: RequestDispatchConfig {
                    initial_fanout: 1,
                    hedge_fanout: 1,
                    max_fanout: 1,
                    hedge_interval_ms: 5,
                },
                ..Default::default()
            },
        );
        let provider = make_shared_test_node(
            relay,
            "provider-suggested",
            MeshRoutingConfig {
                cashu_accepted_mints: vec!["https://mint-b.example".to_string()],
                cashu_default_mint: Some("https://mint-b.example".to_string()),
                ..Default::default()
            },
        );
        let nodes = [&requester, &provider];

        requester.transport.connect(&[]).await.expect("connect");
        provider.transport.connect(&[]).await.expect("connect");
        requester.store.start().await.expect("start");
        provider.store.start().await.expect("start");
        pump_test_network(&nodes, 24).await;

        let payload = b"quoted-data".to_vec();
        let hash = hashtree_core::sha256(&payload);
        provider.local_store.put(hash, payload).await.expect("put");

        let quote = run_quote_with_pumps(
            requester.store.clone(),
            hash,
            3,
            Duration::from_millis(80),
            vec!["provider-suggested".to_string()],
            &nodes,
        )
        .await
        .expect("expected bounded peer-suggested mint quote");
        assert_eq!(quote.mint_url.as_deref(), Some("https://mint-b.example"));
    }

    #[tokio::test]
    async fn test_request_quote_from_peers_scales_peer_suggested_mint_cap_with_reputation() {
        let _guard = mock_network_lock().lock().await;
        crate::mock::clear_channel_registry().await;

        let relay = crate::mock::MockRelay::new();
        let requester = make_shared_test_node(
            relay.clone(),
            "requester-reputation",
            MeshRoutingConfig {
                cashu_accepted_mints: vec!["https://mint-a.example".to_string()],
                cashu_default_mint: Some("https://mint-a.example".to_string()),
                cashu_peer_suggested_mint_base_cap_sat: 1,
                cashu_peer_suggested_mint_success_step_sat: 1,
                cashu_peer_suggested_mint_receipt_step_sat: 2,
                cashu_peer_suggested_mint_max_cap_sat: 5,
                dispatch: RequestDispatchConfig {
                    initial_fanout: 1,
                    hedge_fanout: 1,
                    max_fanout: 1,
                    hedge_interval_ms: 5,
                },
                ..Default::default()
            },
        );
        let provider = make_shared_test_node(
            relay,
            "provider-reputation",
            MeshRoutingConfig {
                cashu_accepted_mints: vec!["https://mint-b.example".to_string()],
                cashu_default_mint: Some("https://mint-b.example".to_string()),
                ..Default::default()
            },
        );
        let nodes = [&requester, &provider];

        requester.transport.connect(&[]).await.expect("connect");
        provider.transport.connect(&[]).await.expect("connect");
        requester.store.start().await.expect("start");
        provider.store.start().await.expect("start");
        pump_test_network(&nodes, 24).await;

        let payload = b"quoted-data".to_vec();
        let hash = hashtree_core::sha256(&payload);
        provider.local_store.put(hash, payload).await.expect("put");

        let quote = run_quote_with_pumps(
            requester.store.clone(),
            hash,
            4,
            Duration::from_millis(80),
            vec!["provider-reputation".to_string()],
            &nodes,
        )
        .await;
        assert!(
            quote.is_none(),
            "new peer should not get a 4 sat untrusted-mint quote"
        );

        {
            let mut selector = requester.store.peer_selector.write().await;
            selector.add_peer("provider-reputation");
            selector.record_success("provider-reputation", 20, 1024);
            selector.record_cashu_receipt("provider-reputation", 2);
        }

        let quote = run_quote_with_pumps(
            requester.store.clone(),
            hash,
            4,
            Duration::from_millis(80),
            vec!["provider-reputation".to_string()],
            &nodes,
        )
        .await
        .expect("reputable peer should get larger bounded quote");
        assert_eq!(quote.mint_url.as_deref(), Some("https://mint-b.example"));

        requester
            .store
            .record_cashu_payment_default_from_peer("provider-reputation")
            .await;
        let quote = run_quote_with_pumps(
            requester.store.clone(),
            hash,
            4,
            Duration::from_millis(80),
            vec!["provider-reputation".to_string()],
            &nodes,
        )
        .await;
        assert!(
            quote.is_none(),
            "peer-suggested mint exposure should drop to zero after defaults exceed receipts"
        );
    }

    #[tokio::test]
    async fn test_request_quote_from_peers_returns_matching_mint() {
        let _guard = mock_network_lock().lock().await;
        crate::mock::clear_channel_registry().await;

        let relay = crate::mock::MockRelay::new();
        let requester = make_shared_test_node(
            relay.clone(),
            "requester-match",
            MeshRoutingConfig {
                cashu_accepted_mints: vec!["https://mint-a.example".to_string()],
                cashu_default_mint: Some("https://mint-a.example".to_string()),
                dispatch: RequestDispatchConfig {
                    initial_fanout: 1,
                    hedge_fanout: 1,
                    max_fanout: 1,
                    hedge_interval_ms: 5,
                },
                ..Default::default()
            },
        );
        let provider = make_shared_test_node(
            relay,
            "provider-match",
            MeshRoutingConfig {
                cashu_accepted_mints: vec!["https://mint-a.example".to_string()],
                ..Default::default()
            },
        );
        let nodes = [&requester, &provider];

        requester.transport.connect(&[]).await.expect("connect");
        provider.transport.connect(&[]).await.expect("connect");
        requester.store.start().await.expect("start");
        provider.store.start().await.expect("start");
        pump_test_network(&nodes, 24).await;

        let payload = b"quoted-data".to_vec();
        let hash = hashtree_core::sha256(&payload);
        provider.local_store.put(hash, payload).await.expect("put");

        let quote = run_quote_with_pumps(
            requester.store.clone(),
            hash,
            9,
            Duration::from_millis(80),
            vec!["provider-match".to_string()],
            &nodes,
        )
        .await
        .expect("expected quote");
        assert_eq!(quote.mint_url.as_deref(), Some("https://mint-a.example"));
    }

    #[tokio::test]
    async fn test_tit_for_tat_store_path_recovers_after_bad_peer_observation() {
        let tit_for_tat_successes = run_bad_peer_series(SelectionStrategy::TitForTat).await;

        assert!(
            tit_for_tat_successes >= 5,
            "expected tit-for-tat path to recover after the first consistently bad peer observation (successes={tit_for_tat_successes})"
        );
    }
}

/// Type alias for simulation store.
pub type SimMeshStore<S> =
    MeshStoreCore<S, crate::mock::MockRelayTransport, crate::mock::MockConnectionFactory>;

/// Type alias for the default production core (Nostr signaling + WebRTC links).
pub type ProductionMeshStore<S> =
    MeshStoreCore<S, crate::nostr::NostrRelayTransport, crate::real_factory::WebRtcPeerLinkFactory>;
