use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, warn};

use crate::nostr_relay::NostrRelay;

use super::peer::ContentStore;
use super::signaling::WebRTCState;
use super::types::{
    encode_request, encode_response, hash_to_hex, parse_message, DataMessage, DataRequest,
    DataResponse, MeshNostrFrame, PeerDirection, PeerHTLConfig, PeerId, TimedSeenSet,
    BLOB_REQUEST_POLICY,
};
use nostr::{
    ClientMessage as NostrClientMessage, Filter as NostrFilter, JsonUtil as NostrJsonUtil,
    RelayMessage as NostrRelayMessage, SubscriptionId as NostrSubscriptionId, Timestamp,
};

const BLUETOOTH_SEEN_EVENT_CAP: usize = 2048;
const BLUETOOTH_SEEN_EVENT_TTL: Duration = Duration::from_secs(600);

#[derive(Debug, Clone)]
pub enum BluetoothFrame {
    Text(String),
    Binary(Vec<u8>),
}

#[async_trait]
pub trait BluetoothLink: Send + Sync {
    async fn send(&self, frame: BluetoothFrame) -> Result<()>;
    async fn recv(&self) -> Option<BluetoothFrame>;
    fn is_open(&self) -> bool;
    async fn close(&self) -> Result<()>;
}

pub struct BluetoothPeer {
    pub peer_id: PeerId,
    pub direction: PeerDirection,
    pub created_at: std::time::Instant,
    pub connected_at: Option<std::time::Instant>,
    link: Arc<dyn BluetoothLink>,
    store: Option<Arc<dyn ContentStore>>,
    pending_requests: Arc<Mutex<HashMap<String, oneshot::Sender<Option<Vec<u8>>>>>>,
    pending_nostr_queries: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<NostrRelayMessage>>>>,
    nostr_relay: Option<Arc<NostrRelay>>,
    mesh_frame_tx: Option<mpsc::Sender<(PeerId, MeshNostrFrame)>>,
    traffic_state: Option<Arc<WebRTCState>>,
    seen_event_ids: Arc<Mutex<TimedSeenSet>>,
    htl_config: PeerHTLConfig,
}

impl BluetoothPeer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        peer_id: PeerId,
        direction: PeerDirection,
        link: Arc<dyn BluetoothLink>,
        store: Option<Arc<dyn ContentStore>>,
        nostr_relay: Option<Arc<NostrRelay>>,
        mesh_frame_tx: Option<mpsc::Sender<(PeerId, MeshNostrFrame)>>,
        traffic_state: Option<Arc<WebRTCState>>,
    ) -> Arc<Self> {
        let peer = Arc::new(Self {
            peer_id,
            direction,
            created_at: std::time::Instant::now(),
            connected_at: Some(std::time::Instant::now()),
            link,
            store,
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            pending_nostr_queries: Arc::new(Mutex::new(HashMap::new())),
            nostr_relay,
            mesh_frame_tx,
            traffic_state,
            seen_event_ids: Arc::new(Mutex::new(TimedSeenSet::new(
                BLUETOOTH_SEEN_EVENT_CAP,
                BLUETOOTH_SEEN_EVENT_TTL,
            ))),
            htl_config: PeerHTLConfig::random(),
        });
        Self::spawn_reader(peer.clone());
        peer
    }

    async fn mark_seen_event_id(&self, event_id: String) -> bool {
        self.seen_event_ids.lock().await.insert_if_new(event_id)
    }

    fn spawn_reader(peer: Arc<Self>) {
        tokio::spawn(async move {
            let mut nostr_forward_task = None;
            let mut nostr_client_id = None;

            if let Some(relay) = peer.nostr_relay.as_ref() {
                let client_id = relay.next_client_id();
                let (nostr_tx, mut nostr_rx) = mpsc::unbounded_channel::<String>();
                relay
                    .register_client(client_id, nostr_tx, Some(peer.peer_id.pubkey.clone()))
                    .await;
                nostr_client_id = Some(client_id);

                let live_subscription_id =
                    NostrSubscriptionId::new(format!("bluetooth-live-{}", rand::random::<u64>()));
                let _ = relay
                    .register_subscription_query(
                        client_id,
                        live_subscription_id.clone(),
                        vec![NostrFilter::new().since(Timestamp::now())],
                    )
                    .await;

                let peer_for_forward = peer.clone();
                nostr_forward_task = Some(tokio::spawn(async move {
                    while let Some(text) = nostr_rx.recv().await {
                        if let Ok(NostrRelayMessage::Event {
                            subscription_id,
                            event,
                        }) = NostrRelayMessage::from_json(&text)
                        {
                            if subscription_id == live_subscription_id {
                                if event.kind.is_ephemeral()
                                    || !peer_for_forward.mark_seen_event_id(event.id.to_hex()).await
                                {
                                    continue;
                                }
                                if peer_for_forward
                                    .send_frame(BluetoothFrame::Text(event.as_json()))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                                continue;
                            }
                        }
                        if peer_for_forward
                            .send_frame(BluetoothFrame::Text(text))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }));
            }

            while let Some(frame) = peer.link.recv().await {
                match frame {
                    BluetoothFrame::Binary(data) => {
                        if let Err(err) = peer.handle_binary_frame(data).await {
                            debug!(
                                "[BluetoothPeer {}] Ignoring invalid binary frame: {}",
                                peer.peer_id.short(),
                                err
                            );
                        }
                    }
                    BluetoothFrame::Text(text) => {
                        peer.handle_text_frame(text, nostr_client_id).await;
                    }
                }
            }

            if let (Some(relay), Some(client_id)) = (peer.nostr_relay.as_ref(), nostr_client_id) {
                relay.unregister_client(client_id).await;
            }

            if let Some(task) = nostr_forward_task {
                let _ = task.await;
            }
        });
    }

    async fn handle_binary_frame(&self, data: Vec<u8>) -> Result<()> {
        self.record_received(data.len() as u64).await;
        match parse_message(&data)? {
            DataMessage::Request(req) => {
                let hash_hex = hash_to_hex(&req.h);
                if let Some(store) = self.store.as_ref() {
                    if let Ok(Some(data)) = store.get(&hash_hex) {
                        let response = DataResponse {
                            h: req.h,
                            d: data,
                            i: None,
                            n: None,
                        };
                        let wire = encode_response(&response)?;
                        self.send_frame(BluetoothFrame::Binary(wire)).await?;
                    }
                }
            }
            DataMessage::Response(res) => {
                let hash_hex = hash_to_hex(&res.h);
                if let Some(sender) = self.pending_requests.lock().await.remove(&hash_hex) {
                    let _ = sender.send(Some(res.d));
                }
            }
            other => {
                debug!(
                    "[BluetoothPeer {}] Ignoring unsupported Bluetooth data frame {:?}",
                    self.peer_id.short(),
                    other
                );
            }
        }
        Ok(())
    }

    async fn handle_text_frame(&self, text: String, nostr_client_id: Option<u64>) {
        self.record_received(text.len() as u64).await;
        if let Ok(mesh_frame) = serde_json::from_str::<MeshNostrFrame>(&text) {
            if let Some(tx) = self.mesh_frame_tx.as_ref() {
                let _ = tx.send((self.peer_id.clone(), mesh_frame)).await;
                return;
            }
        }

        if let Ok(relay_msg) = NostrRelayMessage::from_json(&text) {
            if let Some(sub_id) = relay_subscription_id(&relay_msg) {
                let sender = {
                    let pending = self.pending_nostr_queries.lock().await;
                    pending.get(&sub_id).cloned()
                };
                if let Some(tx) = sender {
                    let _ = tx.send(relay_msg);
                    return;
                }
            }
        }

        if let Some(relay) = self.nostr_relay.as_ref() {
            if let Ok(event) = nostr::Event::from_json(&text) {
                if self.mark_seen_event_id(event.id.to_hex()).await {
                    let _ = relay
                        .ingest_trusted_event_from_bluetooth(event, Some(self.peer_id.to_string()))
                        .await;
                }
                return;
            }

            if let Ok(nostr_msg) = NostrClientMessage::from_json(&text) {
                if let Some(client_id) = nostr_client_id {
                    relay.handle_client_message(client_id, nostr_msg).await;
                }
            }
        }
    }

    pub fn is_connected(&self) -> bool {
        self.link.is_open()
    }

    pub fn htl_config(&self) -> &PeerHTLConfig {
        &self.htl_config
    }

    async fn record_sent(&self, bytes: u64) {
        if let Some(state) = self.traffic_state.as_ref() {
            state.record_sent(&self.peer_id.to_string(), bytes).await;
        }
    }

    async fn record_received(&self, bytes: u64) {
        if let Some(state) = self.traffic_state.as_ref() {
            state
                .record_received(&self.peer_id.to_string(), bytes)
                .await;
        }
    }

    async fn send_frame(&self, frame: BluetoothFrame) -> Result<()> {
        let bytes = match &frame {
            BluetoothFrame::Text(text) => text.len() as u64,
            BluetoothFrame::Binary(payload) => payload.len() as u64,
        };
        if let Err(err) = self.link.send(frame).await {
            warn!(
                "[BluetoothPeer {}] Failed to send frame over BLE: {}",
                self.peer_id.short(),
                err
            );
            let _ = self.link.close().await;
            return Err(err);
        }
        self.record_sent(bytes).await;
        Ok(())
    }

    pub async fn request_with_timeout(
        &self,
        hash_hex: &str,
        timeout: Duration,
    ) -> Result<Option<Vec<u8>>> {
        if !self.link.is_open() {
            return Ok(None);
        }

        let hash = hex::decode(hash_hex)?;
        let request = DataRequest {
            h: hash,
            htl: BLOB_REQUEST_POLICY.max_htl,
            q: None,
        };
        let wire = encode_request(&request)?;
        let (tx, rx) = oneshot::channel();
        self.pending_requests
            .lock()
            .await
            .insert(hash_hex.to_string(), tx);
        self.send_frame(BluetoothFrame::Binary(wire)).await?;

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(data)) => Ok(data),
            Ok(Err(_)) => Ok(None),
            Err(_) => {
                self.pending_requests.lock().await.remove(hash_hex);
                Ok(None)
            }
        }
    }

    pub async fn query_nostr_events(
        &self,
        filters: Vec<NostrFilter>,
        timeout: Duration,
    ) -> Result<Vec<nostr::Event>> {
        let subscription_id = NostrSubscriptionId::generate();
        let subscription_key = subscription_id.to_string();
        let (tx, mut rx) = mpsc::unbounded_channel::<NostrRelayMessage>();

        self.pending_nostr_queries
            .lock()
            .await
            .insert(subscription_key.clone(), tx);

        let req = NostrClientMessage::req(subscription_id.clone(), filters);
        self.send_frame(BluetoothFrame::Text(req.as_json())).await?;

        let mut events = Vec::new();
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            match tokio::time::timeout(deadline - now, rx.recv()).await {
                Ok(Some(NostrRelayMessage::Event {
                    subscription_id: sid,
                    event,
                })) if sid == subscription_id => events.push(*event),
                Ok(Some(NostrRelayMessage::EndOfStoredEvents(sid))) if sid == subscription_id => {
                    break;
                }
                Ok(Some(NostrRelayMessage::Closed {
                    subscription_id: sid,
                    ..
                })) if sid == subscription_id => break,
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }

        let close = NostrClientMessage::close(subscription_id.clone());
        let _ = self.send_frame(BluetoothFrame::Text(close.as_json())).await;
        self.pending_nostr_queries
            .lock()
            .await
            .remove(&subscription_key);
        Ok(events)
    }

    pub async fn send_mesh_frame_text(&self, frame: &MeshNostrFrame) -> Result<()> {
        let text = serde_json::to_string(frame)?;
        self.send_frame(BluetoothFrame::Text(text)).await
    }

    pub async fn close(&self) -> Result<()> {
        self.link.close().await
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

#[cfg(test)]
pub struct MockBluetoothLink {
    open: std::sync::atomic::AtomicBool,
    tx: mpsc::Sender<BluetoothFrame>,
    rx: Mutex<mpsc::Receiver<BluetoothFrame>>,
}

#[cfg(test)]
impl MockBluetoothLink {
    pub fn pair() -> (Arc<Self>, Arc<Self>) {
        let (tx_a, rx_a) = mpsc::channel(32);
        let (tx_b, rx_b) = mpsc::channel(32);
        (
            Arc::new(Self {
                open: std::sync::atomic::AtomicBool::new(true),
                tx: tx_a,
                rx: Mutex::new(rx_b),
            }),
            Arc::new(Self {
                open: std::sync::atomic::AtomicBool::new(true),
                tx: tx_b,
                rx: Mutex::new(rx_a),
            }),
        )
    }
}

#[cfg(test)]
#[async_trait]
impl BluetoothLink for MockBluetoothLink {
    async fn send(&self, frame: BluetoothFrame) -> Result<()> {
        use std::sync::atomic::Ordering;
        if !self.open.load(Ordering::Relaxed) {
            return Ok(());
        }
        self.tx.send(frame).await.map_err(Into::into)
    }

    async fn recv(&self) -> Option<BluetoothFrame> {
        self.rx.lock().await.recv().await
    }

    fn is_open(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.open.load(Ordering::Relaxed)
    }

    async fn close(&self) -> Result<()> {
        use std::sync::atomic::Ordering;
        self.open.store(false, Ordering::Relaxed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nostr_relay::{NostrRelay, NostrRelayConfig};
    use crate::webrtc::signaling::{ConnectionState, PeerEntry, PeerSignalPath, PeerTransport};
    use anyhow::anyhow;
    use nostr::{EventBuilder, Filter, Keys, Kind};
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Instant;
    use tempfile::TempDir;

    struct TestStore {
        blobs: HashMap<String, Vec<u8>>,
    }

    impl ContentStore for TestStore {
        fn get(&self, hash_hex: &str) -> Result<Option<Vec<u8>>> {
            Ok(self.blobs.get(hash_hex).cloned())
        }
    }

    struct FailingSendLink {
        open: AtomicBool,
    }

    #[async_trait]
    impl BluetoothLink for FailingSendLink {
        async fn send(&self, _frame: BluetoothFrame) -> Result<()> {
            Err(anyhow!("send failed"))
        }

        async fn recv(&self) -> Option<BluetoothFrame> {
            std::future::pending::<Option<BluetoothFrame>>().await
        }

        fn is_open(&self) -> bool {
            self.open.load(Ordering::Relaxed)
        }

        async fn close(&self) -> Result<()> {
            self.open.store(false, Ordering::Relaxed);
            Ok(())
        }
    }

    #[tokio::test]
    async fn bluetooth_peer_round_trips_hash_request_over_mock_link() {
        let (link_a, link_b) = MockBluetoothLink::pair();
        let data = b"bluetooth mesh payload".to_vec();
        let hash_hex = hex::encode(hashtree_core::sha256(&data));

        let requester = BluetoothPeer::new(
            PeerId::new("peer-a".to_string()),
            PeerDirection::Outbound,
            link_a,
            None,
            None,
            None,
            None,
        );

        let mut blobs = HashMap::new();
        blobs.insert(hash_hex.clone(), data.clone());
        let responder = BluetoothPeer::new(
            PeerId::new("peer-b".to_string()),
            PeerDirection::Inbound,
            link_b,
            Some(Arc::new(TestStore { blobs })),
            None,
            None,
            None,
        );

        let received = requester
            .request_with_timeout(&hash_hex, Duration::from_secs(1))
            .await
            .expect("request should succeed");

        assert_eq!(received, Some(data));
        responder.close().await.unwrap();
    }

    #[tokio::test]
    async fn bluetooth_peer_records_bidirectional_bytes_in_router_state() {
        let (link_a, link_b) = MockBluetoothLink::pair();
        let state = Arc::new(WebRTCState::new());
        let data = b"bluetooth stats payload".to_vec();
        let hash_hex = hex::encode(hashtree_core::sha256(&data));
        let requester_id = PeerId::new("peer-a".to_string());
        let responder_id = PeerId::new("peer-b".to_string());

        for peer_id in [&requester_id, &responder_id] {
            state.peers.write().await.insert(
                peer_id.to_string(),
                PeerEntry {
                    peer_id: peer_id.clone(),
                    direction: PeerDirection::Outbound,
                    state: ConnectionState::Connected,
                    last_seen: Instant::now(),
                    peer: None,
                    pool: super::super::types::PeerPool::Other,
                    transport: PeerTransport::Bluetooth,
                    signal_paths: std::collections::BTreeSet::from([PeerSignalPath::Bluetooth]),
                    bytes_sent: 0,
                    bytes_received: 0,
                },
            );
        }

        let requester = BluetoothPeer::new(
            requester_id.clone(),
            PeerDirection::Outbound,
            link_a,
            None,
            None,
            None,
            Some(state.clone()),
        );

        let mut blobs = HashMap::new();
        blobs.insert(hash_hex.clone(), data.clone());
        let responder = BluetoothPeer::new(
            responder_id.clone(),
            PeerDirection::Inbound,
            link_b,
            Some(Arc::new(TestStore { blobs })),
            None,
            None,
            Some(state.clone()),
        );

        let received = requester
            .request_with_timeout(&hash_hex, Duration::from_secs(1))
            .await
            .expect("request should succeed");

        assert_eq!(received, Some(data.clone()));
        let hash = hex::decode(&hash_hex).expect("valid hash hex");
        let expected_request_len = encode_request(&DataRequest {
            h: hash.clone(),
            htl: BLOB_REQUEST_POLICY.max_htl,
            q: None,
        })
        .expect("request encoding")
        .len() as u64;
        let expected_response_len = encode_response(&DataResponse {
            h: hash,
            d: data.clone(),
            i: None,
            n: None,
        })
        .expect("response encoding")
        .len() as u64;

        let peers = state.peers.read().await;
        let requester_stats = peers
            .get(&requester_id.to_string())
            .expect("requester stats");
        let responder_stats = peers
            .get(&responder_id.to_string())
            .expect("responder stats");
        assert_eq!(requester_stats.bytes_sent, expected_request_len);
        assert_eq!(requester_stats.bytes_received, expected_response_len);
        assert_eq!(responder_stats.bytes_received, expected_request_len);
        assert_eq!(responder_stats.bytes_sent, expected_response_len);
        drop(peers);

        responder.close().await.unwrap();
    }

    #[tokio::test]
    async fn bluetooth_peer_round_trips_nostr_queries_over_mock_link() -> Result<()> {
        let (link_a, link_b) = MockBluetoothLink::pair();
        let tmp = TempDir::new()?;
        let graph_store = {
            let _guard = crate::socialgraph::test_lock();
            crate::socialgraph::open_social_graph_store_with_mapsize(
                tmp.path(),
                Some(128 * 1024 * 1024),
            )?
        };
        let author_keys = Keys::generate();
        let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();
        let access = Arc::new(crate::socialgraph::SocialGraphAccessControl::new(
            Arc::clone(&backend),
            0,
            HashSet::from([author_keys.public_key().to_hex()]),
        ));
        let relay = Arc::new(NostrRelay::new(
            Arc::clone(&backend),
            tmp.path().to_path_buf(),
            HashSet::from([author_keys.public_key().to_hex()]),
            Some(access),
            NostrRelayConfig {
                spambox_db_max_bytes: 0,
                ..Default::default()
            },
        )?);

        let requester = BluetoothPeer::new(
            PeerId::new("peer-a".to_string()),
            PeerDirection::Outbound,
            link_a,
            None,
            None,
            None,
            None,
        );
        let responder = BluetoothPeer::new(
            PeerId::new("peer-b".to_string()),
            PeerDirection::Inbound,
            link_b,
            None,
            Some(relay.clone()),
            None,
            None,
        );

        let event = EventBuilder::new(Kind::TextNote, "bluetooth nostr relay", [])
            .to_event(&author_keys)?;
        relay.ingest_trusted_event(event.clone()).await?;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let events = requester
            .query_nostr_events(
                vec![Filter::new()
                    .authors(vec![event.pubkey])
                    .kinds(vec![event.kind])],
                Duration::from_secs(1),
            )
            .await?;

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, event.id);
        responder.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn bluetooth_peer_forwards_local_publishes_and_records_bluetooth_provenance() -> Result<()>
    {
        let (link_a, link_b) = MockBluetoothLink::pair();
        let tmp_a = TempDir::new()?;
        let tmp_b = TempDir::new()?;

        let graph_store_a = {
            let _guard = crate::socialgraph::test_lock();
            crate::socialgraph::open_social_graph_store_with_mapsize(
                tmp_a.path(),
                Some(128 * 1024 * 1024),
            )?
        };
        let graph_store_b = {
            let _guard = crate::socialgraph::test_lock();
            crate::socialgraph::open_social_graph_store_with_mapsize(
                tmp_b.path(),
                Some(128 * 1024 * 1024),
            )?
        };
        let author_keys = Keys::generate();

        let backend_a: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store_a.clone();
        let backend_b: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store_b.clone();
        let access_a = Arc::new(crate::socialgraph::SocialGraphAccessControl::new(
            Arc::clone(&backend_a),
            0,
            HashSet::from([author_keys.public_key().to_hex()]),
        ));
        let access_b = Arc::new(crate::socialgraph::SocialGraphAccessControl::new(
            Arc::clone(&backend_b),
            0,
            HashSet::from([author_keys.public_key().to_hex()]),
        ));

        let relay_a = Arc::new(NostrRelay::new(
            Arc::clone(&backend_a),
            tmp_a.path().to_path_buf(),
            HashSet::from([author_keys.public_key().to_hex()]),
            Some(access_a),
            NostrRelayConfig {
                spambox_db_max_bytes: 0,
                ..Default::default()
            },
        )?);
        let relay_b = Arc::new(NostrRelay::new(
            Arc::clone(&backend_b),
            tmp_b.path().to_path_buf(),
            HashSet::from([author_keys.public_key().to_hex()]),
            Some(access_b),
            NostrRelayConfig {
                spambox_db_max_bytes: 0,
                ..Default::default()
            },
        )?);

        let sender = BluetoothPeer::new(
            PeerId::new("peer-a".to_string()),
            PeerDirection::Outbound,
            link_a,
            None,
            Some(relay_a.clone()),
            None,
            None,
        );
        let receiver = BluetoothPeer::new(
            PeerId::new("peer-b".to_string()),
            PeerDirection::Inbound,
            link_b,
            None,
            Some(relay_b.clone()),
            None,
            None,
        );

        tokio::time::sleep(Duration::from_millis(50)).await;

        let cid = "ab".repeat(32);
        let event = EventBuilder::new(
            Kind::TextNote,
            "bluetooth publish sync",
            [nostr::Tag::parse(&["cid", &cid]).unwrap()],
        )
        .to_event(&author_keys)?;
        relay_a.ingest_trusted_event(event.clone()).await?;

        tokio::time::sleep(Duration::from_millis(150)).await;

        let received = relay_b
            .query_events(
                &Filter::new()
                    .authors(vec![event.pubkey])
                    .kinds(vec![event.kind]),
                10,
            )
            .await;
        assert_eq!(
            received
                .iter()
                .filter(|candidate| candidate.id == event.id)
                .count(),
            1
        );

        let bluetooth_events = relay_b.bluetooth_received_events(10).await;
        assert_eq!(bluetooth_events.len(), 1);
        assert_eq!(bluetooth_events[0].event_id, event.id.to_hex());
        assert_eq!(bluetooth_events[0].peer_id.as_deref(), Some("peer-b"));
        assert_eq!(bluetooth_events[0].cid_values, vec![cid]);

        receiver.close().await?;
        sender.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn bluetooth_peer_forwards_local_publishes_both_directions() -> Result<()> {
        let (link_a, link_b) = MockBluetoothLink::pair();
        let tmp_a = TempDir::new()?;
        let tmp_b = TempDir::new()?;

        let graph_store_a = {
            let _guard = crate::socialgraph::test_lock();
            crate::socialgraph::open_social_graph_store_with_mapsize(
                tmp_a.path(),
                Some(128 * 1024 * 1024),
            )?
        };
        let graph_store_b = {
            let _guard = crate::socialgraph::test_lock();
            crate::socialgraph::open_social_graph_store_with_mapsize(
                tmp_b.path(),
                Some(128 * 1024 * 1024),
            )?
        };
        let author_keys_a = Keys::generate();
        let author_keys_b = Keys::generate();

        let backend_a: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store_a.clone();
        let backend_b: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store_b.clone();
        let access_a = Arc::new(crate::socialgraph::SocialGraphAccessControl::new(
            Arc::clone(&backend_a),
            0,
            HashSet::from([
                author_keys_a.public_key().to_hex(),
                author_keys_b.public_key().to_hex(),
            ]),
        ));
        let access_b = Arc::new(crate::socialgraph::SocialGraphAccessControl::new(
            Arc::clone(&backend_b),
            0,
            HashSet::from([
                author_keys_a.public_key().to_hex(),
                author_keys_b.public_key().to_hex(),
            ]),
        ));

        let relay_a = Arc::new(NostrRelay::new(
            Arc::clone(&backend_a),
            tmp_a.path().to_path_buf(),
            HashSet::from([
                author_keys_a.public_key().to_hex(),
                author_keys_b.public_key().to_hex(),
            ]),
            Some(access_a),
            NostrRelayConfig {
                spambox_db_max_bytes: 0,
                ..Default::default()
            },
        )?);
        let relay_b = Arc::new(NostrRelay::new(
            Arc::clone(&backend_b),
            tmp_b.path().to_path_buf(),
            HashSet::from([
                author_keys_a.public_key().to_hex(),
                author_keys_b.public_key().to_hex(),
            ]),
            Some(access_b),
            NostrRelayConfig {
                spambox_db_max_bytes: 0,
                ..Default::default()
            },
        )?);

        let peer_a = BluetoothPeer::new(
            PeerId::new("peer-a".to_string()),
            PeerDirection::Outbound,
            link_a,
            None,
            Some(relay_a.clone()),
            None,
            None,
        );
        let peer_b = BluetoothPeer::new(
            PeerId::new("peer-b".to_string()),
            PeerDirection::Inbound,
            link_b,
            None,
            Some(relay_b.clone()),
            None,
            None,
        );

        tokio::time::sleep(Duration::from_millis(50)).await;

        let cid_a = "ab".repeat(32);
        let event_a = EventBuilder::new(
            Kind::TextNote,
            "bluetooth publish from a",
            [nostr::Tag::parse(&["cid", &cid_a]).unwrap()],
        )
        .to_event(&author_keys_a)?;
        relay_a.ingest_trusted_event(event_a.clone()).await?;

        let cid_b = "cd".repeat(32);
        let event_b = EventBuilder::new(
            Kind::TextNote,
            "bluetooth publish from b",
            [nostr::Tag::parse(&["cid", &cid_b]).unwrap()],
        )
        .to_event(&author_keys_b)?;
        relay_b.ingest_trusted_event(event_b.clone()).await?;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let received_on_b = relay_b
            .query_events(
                &Filter::new()
                    .authors(vec![event_a.pubkey])
                    .kinds(vec![event_a.kind]),
                10,
            )
            .await;
        assert_eq!(
            received_on_b
                .iter()
                .filter(|candidate| candidate.id == event_a.id)
                .count(),
            1
        );

        let received_on_a = relay_a
            .query_events(
                &Filter::new()
                    .authors(vec![event_b.pubkey])
                    .kinds(vec![event_b.kind]),
                10,
            )
            .await;
        assert_eq!(
            received_on_a
                .iter()
                .filter(|candidate| candidate.id == event_b.id)
                .count(),
            1
        );

        let bluetooth_events_a = relay_a.bluetooth_received_events(10).await;
        assert_eq!(bluetooth_events_a.len(), 1);
        assert_eq!(bluetooth_events_a[0].event_id, event_b.id.to_hex());
        assert_eq!(bluetooth_events_a[0].peer_id.as_deref(), Some("peer-a"));
        assert_eq!(bluetooth_events_a[0].cid_values, vec![cid_b]);

        let bluetooth_events_b = relay_b.bluetooth_received_events(10).await;
        assert_eq!(bluetooth_events_b.len(), 1);
        assert_eq!(bluetooth_events_b[0].event_id, event_a.id.to_hex());
        assert_eq!(bluetooth_events_b[0].peer_id.as_deref(), Some("peer-b"));
        assert_eq!(bluetooth_events_b[0].cid_values, vec![cid_a]);

        peer_b.close().await?;
        peer_a.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn bluetooth_peer_closes_after_local_publish_send_failure() -> Result<()> {
        let tmp = TempDir::new()?;
        let graph_store = {
            let _guard = crate::socialgraph::test_lock();
            crate::socialgraph::open_social_graph_store_with_mapsize(
                tmp.path(),
                Some(128 * 1024 * 1024),
            )?
        };
        let author_keys = Keys::generate();
        let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();
        let access = Arc::new(crate::socialgraph::SocialGraphAccessControl::new(
            Arc::clone(&backend),
            0,
            HashSet::from([author_keys.public_key().to_hex()]),
        ));
        let relay = Arc::new(NostrRelay::new(
            Arc::clone(&backend),
            tmp.path().to_path_buf(),
            HashSet::from([author_keys.public_key().to_hex()]),
            Some(access),
            NostrRelayConfig {
                spambox_db_max_bytes: 0,
                ..Default::default()
            },
        )?);

        let peer = BluetoothPeer::new(
            PeerId::new("peer-a".to_string()),
            PeerDirection::Outbound,
            Arc::new(FailingSendLink {
                open: AtomicBool::new(true),
            }),
            None,
            Some(relay.clone()),
            None,
            None,
        );

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(peer.is_connected());

        let event = EventBuilder::new(Kind::TextNote, "close stale bluetooth peer", [])
            .to_event(&author_keys)?;
        relay.ingest_trusted_event(event).await?;

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!peer.is_connected());
        Ok(())
    }
}
