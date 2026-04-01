use anyhow::{anyhow, Result};
use async_trait::async_trait;
use nostr::{nips::nip19::FromBech32, Event, Filter, JsonUtil, Keys, PublicKey};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{mpsc, watch, Mutex};
use tracing::{debug, warn};

use super::root_events::{
    build_root_filter, is_hashtree_labeled_event, root_event_from_peer, PeerRootEvent,
    HASHTREE_KIND,
};
use super::LocalNostrBus;
use crate::nostr_relay::NostrRelay;

pub const WIFI_AWARE_SOURCE: &str = "wifi-aware";
const FRAME_VERSION: u8 = 1;
const FRAME_KIND_QUERY_ROOT: u8 = 1;
const FRAME_KIND_ROOT_RESPONSE: u8 = 2;
const FRAME_KIND_QUERY_DONE: u8 = 3;
const ROOT_FLAG_KEY: u8 = 1 << 0;
const ROOT_FLAG_ENCRYPTED_KEY: u8 = 1 << 1;
const ROOT_FLAG_SELF_ENCRYPTED_KEY: u8 = 1 << 2;

#[derive(Debug, Clone)]
pub struct WifiAwareConfig {
    pub enabled: bool,
    pub max_peers: usize,
    pub announce_interval_ms: u64,
}

impl WifiAwareConfig {
    pub fn is_enabled(&self) -> bool {
        self.enabled && self.max_peers > 0
    }
}

impl Default for WifiAwareConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_peers: 0,
            announce_interval_ms: 2_000,
        }
    }
}

#[derive(Debug, Clone)]
pub enum WifiAwareEvent {
    PeerDiscovered { peer_id: String },
    PeerLost { peer_id: String },
    Message { peer_id: String, payload: Vec<u8> },
}

#[async_trait]
pub trait MobileWifiAwareBridge: Send + Sync {
    async fn start(&self, local_peer_id: String) -> Result<mpsc::Receiver<WifiAwareEvent>>;

    async fn broadcast_message(&self, payload: Vec<u8>) -> Result<()>;
}

static MOBILE_WIFI_AWARE_BRIDGE: OnceLock<Arc<dyn MobileWifiAwareBridge>> = OnceLock::new();

pub fn install_mobile_wifi_aware_bridge(bridge: Arc<dyn MobileWifiAwareBridge>) -> Result<()> {
    MOBILE_WIFI_AWARE_BRIDGE
        .set(bridge)
        .map_err(|_| anyhow!("mobile wifi aware bridge already installed"))
}

pub(crate) fn mobile_wifi_aware_bridge() -> Option<Arc<dyn MobileWifiAwareBridge>> {
    MOBILE_WIFI_AWARE_BRIDGE.get().cloned()
}

pub struct WifiAwareNostrBus {
    config: WifiAwareConfig,
    keys: Keys,
    relay: Arc<NostrRelay>,
    bridge: Arc<dyn MobileWifiAwareBridge>,
    pending_queries: Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<PendingQueryMessage>>>>,
    announced_event_ids: Arc<Mutex<HashSet<String>>>,
}

enum PendingQueryMessage {
    Root(PeerRootEvent),
    Done,
}

enum WifiAwareFrame {
    QueryRoot {
        request_id: u64,
        owner_pubkey_hex: String,
        tree_name: String,
    },
    RootResponse {
        request_id: u64,
        root: PeerRootEvent,
    },
    QueryDone {
        request_id: u64,
    },
}

#[async_trait]
impl LocalNostrBus for WifiAwareNostrBus {
    fn source_name(&self) -> &'static str {
        WIFI_AWARE_SOURCE
    }

    async fn broadcast_event(&self, event: &Event) -> Result<()> {
        WifiAwareNostrBus::broadcast_event(self, event).await
    }

    async fn query_root(
        &self,
        owner_pubkey: &str,
        tree_name: &str,
        timeout: Duration,
    ) -> Option<PeerRootEvent> {
        WifiAwareNostrBus::query_root(self, owner_pubkey, tree_name, timeout).await
    }
}

impl WifiAwareNostrBus {
    pub fn new(
        config: WifiAwareConfig,
        keys: Keys,
        relay: Arc<NostrRelay>,
        bridge: Arc<dyn MobileWifiAwareBridge>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            keys,
            relay,
            bridge,
            pending_queries: Arc::new(Mutex::new(HashMap::new())),
            announced_event_ids: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    pub async fn run(
        self: Arc<Self>,
        local_peer_id: String,
        mut shutdown_rx: watch::Receiver<bool>,
        signaling_tx: mpsc::Sender<(String, Event)>,
    ) -> Result<()> {
        let mut announce_ticker = tokio::time::interval(Duration::from_millis(
            self.config.announce_interval_ms.max(1),
        ));
        let mut events = self.bridge.start(local_peer_id).await?;

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
                _ = announce_ticker.tick() => {
                    if let Err(err) = self.broadcast_known_root_updates().await {
                        debug!("wifi aware root announcement failed: {}", err);
                    }
                }
                maybe_event = events.recv() => {
                    match maybe_event {
                        Some(WifiAwareEvent::Message { peer_id, payload }) => {
                            self.handle_message(&peer_id, &payload, &signaling_tx).await;
                        }
                        Some(WifiAwareEvent::PeerDiscovered { peer_id }) => {
                            debug!("wifi aware peer discovered: {}", peer_id);
                        }
                        Some(WifiAwareEvent::PeerLost { peer_id }) => {
                            debug!("wifi aware peer lost: {}", peer_id);
                        }
                        None => break,
                    }
                }
            }
        }

        Ok(())
    }

    pub async fn broadcast_event(&self, event: &Event) -> Result<()> {
        // Keep raw event fallback for generic signaling until local bus semantics
        // are narrowed further. Compact frames are used for root query/response.
        self.bridge
            .broadcast_message(event.as_json().into_bytes())
            .await
    }

    pub async fn query_root(
        &self,
        owner_pubkey: &str,
        tree_name: &str,
        timeout: Duration,
    ) -> Option<PeerRootEvent> {
        let request_id = rand::random::<u64>();
        let owner_bytes = owner_pubkey_bytes(owner_pubkey)?;
        let request = encode_query_root(request_id, owner_bytes, tree_name)?;
        let (tx, mut rx) = mpsc::unbounded_channel();
        self.pending_queries.lock().await.insert(request_id, tx);

        if self.bridge.broadcast_message(request).await.is_err() {
            self.pending_queries.lock().await.remove(&request_id);
            return None;
        }

        let mut roots = Vec::new();
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);

        loop {
            tokio::select! {
                _ = &mut deadline => break,
                maybe_msg = rx.recv() => {
                    let Some(msg) = maybe_msg else {
                        break;
                    };
                    match msg {
                        PendingQueryMessage::Root(root) => roots.push(root),
                        PendingQueryMessage::Done => break,
                    }
                }
            }
        }

        self.pending_queries.lock().await.remove(&request_id);
        pick_latest_root_event(&roots)
    }

    async fn handle_message(
        &self,
        peer_id: &str,
        payload: &[u8],
        signaling_tx: &mpsc::Sender<(String, Event)>,
    ) {
        if let Some(frame) = decode_frame(payload) {
            match frame {
                WifiAwareFrame::QueryRoot {
                    request_id,
                    owner_pubkey_hex,
                    tree_name,
                } => {
                    self.respond_to_root_query(request_id, &owner_pubkey_hex, &tree_name)
                        .await;
                }
                WifiAwareFrame::RootResponse { request_id, root } => {
                    let tx = self.pending_queries.lock().await.get(&request_id).cloned();
                    if let Some(tx) = tx {
                        let _ = tx.send(PendingQueryMessage::Root(root));
                    }
                }
                WifiAwareFrame::QueryDone { request_id } => {
                    let tx = self.pending_queries.lock().await.get(&request_id).cloned();
                    if let Some(tx) = tx {
                        let _ = tx.send(PendingQueryMessage::Done);
                    }
                }
            }
            return;
        }

        let Ok(text) = std::str::from_utf8(payload) else {
            debug!(
                "ignoring non-utf8 wifi aware payload from {} ({} bytes)",
                peer_id,
                payload.len()
            );
            return;
        };

        if let Ok(event) = Event::from_json(text) {
            if event.pubkey == self.keys.public_key() {
                return;
            }

            if event.kind.is_ephemeral() {
                let _ = signaling_tx
                    .send((self.source_name().to_string(), event))
                    .await;
                return;
            }

            if event.kind == nostr::Kind::Custom(HASHTREE_KIND)
                && is_hashtree_labeled_event(&event)
                && event.verify().is_ok()
            {
                let _ = self.relay.ingest_trusted_event(event).await;
            }
            return;
        }

        debug!("ignoring wifi aware payload from {}: {}", peer_id, text);
    }

    async fn respond_to_root_query(&self, request_id: u64, owner_pubkey: &str, tree_name: &str) {
        let Some(filter) = build_root_filter(owner_pubkey, tree_name) else {
            let _ = self
                .bridge
                .broadcast_message(encode_query_done(request_id))
                .await;
            return;
        };

        for event in self.relay.query_events(&filter, 50).await {
            let Some(root) = root_event_from_peer(&event, self.source_name(), tree_name) else {
                continue;
            };
            let Some(encoded) = encode_root_response(request_id, &root) else {
                warn!(
                    "Skipping wifi aware root response for {} due to unsupported root fields",
                    tree_name
                );
                continue;
            };
            if let Err(err) = self.bridge.broadcast_message(encoded).await {
                warn!("wifi aware root response broadcast failed: {}", err);
            }
        }

        if let Err(err) = self
            .bridge
            .broadcast_message(encode_query_done(request_id))
            .await
        {
            warn!("wifi aware query-done broadcast failed: {}", err);
        }
    }

    async fn broadcast_known_root_updates(&self) -> Result<()> {
        let filter = Filter::new()
            .kind(nostr::Kind::Custom(HASHTREE_KIND))
            .author(self.keys.public_key())
            .custom_tag(
                nostr::SingleLetterTag::lowercase(nostr::Alphabet::L),
                vec![super::root_events::HASHTREE_LABEL.to_string()],
            )
            .limit(256);
        let events = self.relay.query_events(&filter, 256).await;
        let mut announced = self.announced_event_ids.lock().await;
        for event in events {
            let event_id = event.id.to_hex();
            if announced.insert(event_id) {
                self.broadcast_event(&event).await?;
            }
        }
        Ok(())
    }
}

fn owner_pubkey_bytes(owner_pubkey: &str) -> Option<[u8; 32]> {
    let pubkey = PublicKey::from_hex(owner_pubkey)
        .or_else(|_| PublicKey::from_bech32(owner_pubkey))
        .ok()?;
    Some(pubkey.to_bytes())
}

fn hex_bytes_32(value: &str) -> Option<[u8; 32]> {
    let decoded = hex::decode(value).ok()?;
    decoded.try_into().ok()
}

fn push_u16(buf: &mut Vec<u8>, value: usize) -> Option<()> {
    let value: u16 = value.try_into().ok()?;
    buf.extend_from_slice(&value.to_be_bytes());
    Some(())
}

fn read_u16(payload: &[u8], cursor: &mut usize) -> Option<usize> {
    if payload.len() < *cursor + 2 {
        return None;
    }
    let value = u16::from_be_bytes([payload[*cursor], payload[*cursor + 1]]) as usize;
    *cursor += 2;
    Some(value)
}

fn read_u64(payload: &[u8], cursor: &mut usize) -> Option<u64> {
    if payload.len() < *cursor + 8 {
        return None;
    }
    let bytes: [u8; 8] = payload[*cursor..*cursor + 8].try_into().ok()?;
    *cursor += 8;
    Some(u64::from_be_bytes(bytes))
}

fn read_exact<const N: usize>(payload: &[u8], cursor: &mut usize) -> Option<[u8; N]> {
    if payload.len() < *cursor + N {
        return None;
    }
    let bytes: [u8; N] = payload[*cursor..*cursor + N].try_into().ok()?;
    *cursor += N;
    Some(bytes)
}

fn read_tree_name(payload: &[u8], cursor: &mut usize) -> Option<String> {
    let len = read_u16(payload, cursor)?;
    if payload.len() < *cursor + len {
        return None;
    }
    let value = std::str::from_utf8(&payload[*cursor..*cursor + len])
        .ok()?
        .to_string();
    *cursor += len;
    Some(value)
}

fn encode_query_root(request_id: u64, owner_pubkey: [u8; 32], tree_name: &str) -> Option<Vec<u8>> {
    let tree_bytes = tree_name.as_bytes();
    let mut payload = Vec::with_capacity(2 + 8 + 32 + 2 + tree_bytes.len());
    payload.push(FRAME_VERSION);
    payload.push(FRAME_KIND_QUERY_ROOT);
    payload.extend_from_slice(&request_id.to_be_bytes());
    payload.extend_from_slice(&owner_pubkey);
    push_u16(&mut payload, tree_bytes.len())?;
    payload.extend_from_slice(tree_bytes);
    Some(payload)
}

fn encode_query_done(request_id: u64) -> Vec<u8> {
    let mut payload = Vec::with_capacity(10);
    payload.push(FRAME_VERSION);
    payload.push(FRAME_KIND_QUERY_DONE);
    payload.extend_from_slice(&request_id.to_be_bytes());
    payload
}

fn encode_root_response(request_id: u64, root: &PeerRootEvent) -> Option<Vec<u8>> {
    let event_id = hex_bytes_32(&root.event_id)?;
    let hash = hex_bytes_32(&root.hash)?;
    let key = match root.key.as_deref() {
        Some(value) => Some(hex_bytes_32(value)?),
        None => None,
    };
    let encrypted_key = match root.encrypted_key.as_deref() {
        Some(value) => Some(hex_bytes_32(value)?),
        None => None,
    };
    let self_encrypted_key = match root.self_encrypted_key.as_deref() {
        Some(value) => Some(hex_bytes_32(value)?),
        None => None,
    };

    let mut flags = 0u8;
    if key.is_some() {
        flags |= ROOT_FLAG_KEY;
    }
    if encrypted_key.is_some() {
        flags |= ROOT_FLAG_ENCRYPTED_KEY;
    }
    if self_encrypted_key.is_some() {
        flags |= ROOT_FLAG_SELF_ENCRYPTED_KEY;
    }

    let mut payload = Vec::with_capacity(2 + 8 + 8 + 32 + 32 + 1 + 96);
    payload.push(FRAME_VERSION);
    payload.push(FRAME_KIND_ROOT_RESPONSE);
    payload.extend_from_slice(&request_id.to_be_bytes());
    payload.extend_from_slice(&root.created_at.to_be_bytes());
    payload.extend_from_slice(&event_id);
    payload.extend_from_slice(&hash);
    payload.push(flags);
    if let Some(value) = key {
        payload.extend_from_slice(&value);
    }
    if let Some(value) = encrypted_key {
        payload.extend_from_slice(&value);
    }
    if let Some(value) = self_encrypted_key {
        payload.extend_from_slice(&value);
    }
    Some(payload)
}

fn decode_frame(payload: &[u8]) -> Option<WifiAwareFrame> {
    if payload.len() < 2 || payload[0] != FRAME_VERSION {
        return None;
    }

    let mut cursor = 2;
    match payload[1] {
        FRAME_KIND_QUERY_ROOT => {
            let request_id = read_u64(payload, &mut cursor)?;
            let owner_pubkey = read_exact::<32>(payload, &mut cursor)?;
            let tree_name = read_tree_name(payload, &mut cursor)?;
            if cursor != payload.len() {
                return None;
            }
            Some(WifiAwareFrame::QueryRoot {
                request_id,
                owner_pubkey_hex: hex::encode(owner_pubkey),
                tree_name,
            })
        }
        FRAME_KIND_ROOT_RESPONSE => {
            let request_id = read_u64(payload, &mut cursor)?;
            let created_at = read_u64(payload, &mut cursor)?;
            let event_id = hex::encode(read_exact::<32>(payload, &mut cursor)?);
            let hash = hex::encode(read_exact::<32>(payload, &mut cursor)?);
            let flags = *payload.get(cursor)?;
            cursor += 1;
            let key = if flags & ROOT_FLAG_KEY != 0 {
                Some(hex::encode(read_exact::<32>(payload, &mut cursor)?))
            } else {
                None
            };
            let encrypted_key = if flags & ROOT_FLAG_ENCRYPTED_KEY != 0 {
                Some(hex::encode(read_exact::<32>(payload, &mut cursor)?))
            } else {
                None
            };
            let self_encrypted_key = if flags & ROOT_FLAG_SELF_ENCRYPTED_KEY != 0 {
                Some(hex::encode(read_exact::<32>(payload, &mut cursor)?))
            } else {
                None
            };
            if cursor != payload.len() {
                return None;
            }
            Some(WifiAwareFrame::RootResponse {
                request_id,
                root: PeerRootEvent {
                    hash,
                    key,
                    encrypted_key,
                    self_encrypted_key,
                    event_id,
                    created_at,
                    peer_id: WIFI_AWARE_SOURCE.to_string(),
                },
            })
        }
        FRAME_KIND_QUERY_DONE => {
            let request_id = read_u64(payload, &mut cursor)?;
            if cursor != payload.len() {
                return None;
            }
            Some(WifiAwareFrame::QueryDone { request_id })
        }
        _ => None,
    }
}

fn pick_latest_root_event(events: &[PeerRootEvent]) -> Option<PeerRootEvent> {
    events
        .iter()
        .max_by(|a, b| {
            let ordering = a.created_at.cmp(&b.created_at);
            if ordering == std::cmp::Ordering::Equal {
                a.event_id.cmp(&b.event_id)
            } else {
                ordering
            }
        })
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nostr_relay::NostrRelayConfig;
    use crate::socialgraph::{self, SocialGraphAccessControl, SocialGraphBackend};
    use nostr::{Alphabet, EventBuilder, Kind, SingleLetterTag, Tag, TagKind};
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::Mutex as AsyncMutex;

    struct MockWifiAwareBridge {
        sent_payloads: AsyncMutex<Vec<Vec<u8>>>,
        response_events: AsyncMutex<Vec<Event>>,
        event_tx: AsyncMutex<Option<mpsc::Sender<WifiAwareEvent>>>,
    }

    impl MockWifiAwareBridge {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                sent_payloads: AsyncMutex::new(Vec::new()),
                response_events: AsyncMutex::new(Vec::new()),
                event_tx: AsyncMutex::new(None),
            })
        }

        async fn queue_response_event(&self, event: Event) {
            self.response_events.lock().await.push(event);
        }

        async fn sent_payloads(&self) -> Vec<Vec<u8>> {
            self.sent_payloads.lock().await.clone()
        }

        async fn wait_until_started(&self) {
            for _ in 0..100 {
                if self.event_tx.lock().await.is_some() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("mock wifi aware bridge did not start in time");
        }
    }

    #[async_trait]
    impl MobileWifiAwareBridge for MockWifiAwareBridge {
        async fn start(&self, _local_peer_id: String) -> Result<mpsc::Receiver<WifiAwareEvent>> {
            let (tx, rx) = mpsc::channel(32);
            *self.event_tx.lock().await = Some(tx);
            Ok(rx)
        }

        async fn broadcast_message(&self, payload: Vec<u8>) -> Result<()> {
            self.sent_payloads.lock().await.push(payload.clone());
            let Some(tx) = self.event_tx.lock().await.clone() else {
                return Ok(());
            };

            if let Some(WifiAwareFrame::QueryRoot {
                request_id,
                owner_pubkey_hex,
                tree_name,
            }) = decode_frame(&payload)
            {
                let response_events = self.response_events.lock().await.clone();
                for event in response_events
                    .iter()
                    .filter(|event| event.pubkey.to_hex() == owner_pubkey_hex)
                {
                    let Some(root) = root_event_from_peer(event, WIFI_AWARE_SOURCE, &tree_name)
                    else {
                        continue;
                    };
                    let encoded = encode_root_response(request_id, &root)
                        .expect("expected compact root response encoding");
                    tx.send(WifiAwareEvent::Message {
                        peer_id: "peer-b".to_string(),
                        payload: encoded,
                    })
                    .await
                    .map_err(|err| anyhow!("mock wifi aware event send failed: {}", err))?;
                }
                tx.send(WifiAwareEvent::Message {
                    peer_id: "peer-b".to_string(),
                    payload: encode_query_done(request_id),
                })
                .await
                .map_err(|err| anyhow!("mock wifi aware query-done send failed: {}", err))?;
            }
            Ok(())
        }
    }

    fn test_relay(keys: &Keys, tmp: &TempDir) -> Result<Arc<NostrRelay>> {
        let _guard = socialgraph::test_lock();
        let graph_store =
            socialgraph::open_social_graph_store_with_mapsize(tmp.path(), Some(128 * 1024 * 1024))?;
        let backend: Arc<dyn SocialGraphBackend> = graph_store.clone();
        let access = Arc::new(SocialGraphAccessControl::new(
            Arc::clone(&backend),
            0,
            HashSet::from([keys.public_key().to_hex()]),
        ));
        Ok(Arc::new(NostrRelay::new(
            Arc::clone(&backend),
            tmp.path().to_path_buf(),
            HashSet::from([keys.public_key().to_hex()]),
            Some(access),
            NostrRelayConfig {
                spambox_db_max_bytes: 0,
                ..Default::default()
            },
        )?))
    }

    #[test]
    fn compact_query_root_frame_round_trips() {
        let owner =
            owner_pubkey_bytes("79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798")
                .expect("owner pubkey");
        let frame = encode_query_root(42, owner, "video").expect("query frame");

        match decode_frame(&frame).expect("decoded frame") {
            WifiAwareFrame::QueryRoot {
                request_id,
                owner_pubkey_hex,
                tree_name,
            } => {
                assert_eq!(request_id, 42);
                assert_eq!(owner_pubkey_hex, hex::encode(owner));
                assert_eq!(tree_name, "video");
            }
            _ => panic!("expected query-root frame"),
        }
    }

    #[tokio::test]
    async fn wifi_aware_bus_broadcast_event_forwards_json_bytes() -> Result<()> {
        let bridge = MockWifiAwareBridge::new();
        let bus_keys = Keys::generate();
        let tmp = TempDir::new()?;
        let relay = test_relay(&bus_keys, &tmp)?;
        let bus = WifiAwareNostrBus::new(
            WifiAwareConfig::default(),
            bus_keys.clone(),
            relay,
            bridge.clone(),
        );
        let event =
            EventBuilder::new(Kind::TextNote, "hello wifi aware", []).to_event(&bus_keys)?;

        bus.broadcast_event(&event).await?;

        let sent = bridge.sent_payloads().await;
        assert_eq!(sent, vec![event.as_json().into_bytes()]);
        Ok(())
    }

    #[tokio::test]
    async fn wifi_aware_bus_query_root_returns_matching_event_and_sends_compact_query() -> Result<()>
    {
        let bridge = MockWifiAwareBridge::new();
        let bus_keys = Keys::generate();
        let author_keys = Keys::generate();
        let tmp = TempDir::new()?;
        let relay = test_relay(&bus_keys, &tmp)?;
        let bus = WifiAwareNostrBus::new(
            WifiAwareConfig {
                enabled: true,
                max_peers: 2,
                announce_interval_ms: 60_000,
            },
            bus_keys,
            relay,
            bridge.clone(),
        );
        let root_event = EventBuilder::new(
            Kind::Custom(HASHTREE_KIND),
            "",
            [
                Tag::identifier("video".to_string()),
                Tag::custom(
                    TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::L)),
                    vec!["hashtree".to_string()],
                ),
                Tag::custom(TagKind::Custom("hash".into()), vec!["ab".repeat(32)]),
            ],
        )
        .to_event(&author_keys)?;
        bridge.queue_response_event(root_event).await;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let bus_task = {
            let bus = bus.clone();
            tokio::spawn(async move {
                let (signaling_tx, _signaling_rx) = mpsc::channel(8);
                bus.run("local-peer".to_string(), shutdown_rx, signaling_tx)
                    .await
            })
        };
        bridge.wait_until_started().await;

        let resolved = bus
            .query_root(
                &author_keys.public_key().to_hex(),
                "video",
                Duration::from_secs(1),
            )
            .await
            .expect("expected wifi aware root");

        assert_eq!(resolved.hash, "ab".repeat(32));
        assert_eq!(resolved.peer_id, WIFI_AWARE_SOURCE);

        let sent = bridge.sent_payloads().await;
        let query_frame = sent.first().expect("expected compact query");
        match decode_frame(query_frame).expect("expected decoded query") {
            WifiAwareFrame::QueryRoot {
                owner_pubkey_hex,
                tree_name,
                ..
            } => {
                assert_eq!(owner_pubkey_hex, author_keys.public_key().to_hex());
                assert_eq!(tree_name, "video");
            }
            _ => panic!("expected first payload to be a query-root frame"),
        }

        shutdown_tx.send(true)?;
        bus_task.await??;
        Ok(())
    }
}
