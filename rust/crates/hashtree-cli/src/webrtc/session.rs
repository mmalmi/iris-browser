use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;

use nostr::{Event, Filter};

use super::bluetooth_peer::BluetoothPeer;
use super::peer::Peer;
use super::signaling::PeerTransport;
use super::types::{MeshNostrFrame, PeerHTLConfig};

#[derive(Clone)]
pub enum MeshPeer {
    WebRtc(Arc<Peer>),
    Bluetooth(Arc<BluetoothPeer>),
    #[cfg(test)]
    Mock(Arc<TestMeshPeer>),
}

impl MeshPeer {
    pub fn is_ready(&self) -> bool {
        match self {
            Self::WebRtc(peer) => peer.has_data_channel(),
            Self::Bluetooth(peer) => peer.is_connected(),
            #[cfg(test)]
            Self::Mock(peer) => peer.ready,
        }
    }

    pub fn is_connected(&self) -> bool {
        match self {
            Self::WebRtc(peer) => peer.is_connected(),
            Self::Bluetooth(peer) => peer.is_connected(),
            #[cfg(test)]
            Self::Mock(peer) => peer.connected,
        }
    }

    pub fn htl_config(&self) -> PeerHTLConfig {
        match self {
            Self::WebRtc(peer) => *peer.htl_config(),
            Self::Bluetooth(peer) => *peer.htl_config(),
            #[cfg(test)]
            Self::Mock(peer) => peer.htl_config,
        }
    }

    pub fn transport(&self) -> PeerTransport {
        match self {
            Self::WebRtc(_) => PeerTransport::WebRtc,
            Self::Bluetooth(_) => PeerTransport::Bluetooth,
            #[cfg(test)]
            Self::Mock(_) => PeerTransport::WebRtc,
        }
    }

    pub async fn request(&self, hash_hex: &str, timeout: Duration) -> Result<Option<Vec<u8>>> {
        match self {
            Self::WebRtc(peer) => peer.request_with_timeout(hash_hex, timeout).await,
            Self::Bluetooth(peer) => peer.request_with_timeout(hash_hex, timeout).await,
            #[cfg(test)]
            Self::Mock(peer) => peer.request(hash_hex, timeout).await,
        }
    }

    pub async fn query_nostr_events(
        &self,
        filters: Vec<Filter>,
        timeout: Duration,
    ) -> Result<Vec<Event>> {
        match self {
            Self::WebRtc(peer) => peer.query_nostr_events(filters, timeout).await,
            Self::Bluetooth(peer) => peer.query_nostr_events(filters, timeout).await,
            #[cfg(test)]
            Self::Mock(peer) => peer.query_nostr_events(filters, timeout).await,
        }
    }

    pub async fn send_mesh_frame_text(&self, frame: &MeshNostrFrame) -> Result<()> {
        match self {
            Self::WebRtc(peer) => peer.send_mesh_frame_text(frame).await,
            Self::Bluetooth(peer) => peer.send_mesh_frame_text(frame).await,
            #[cfg(test)]
            Self::Mock(peer) => peer.send_mesh_frame_text(frame).await,
        }
    }

    pub async fn close(&self) -> Result<()> {
        match self {
            Self::WebRtc(peer) => peer.close().await,
            Self::Bluetooth(peer) => peer.close().await,
            #[cfg(test)]
            Self::Mock(peer) => peer.close().await,
        }
    }

    pub fn as_webrtc(&self) -> Option<&Arc<Peer>> {
        match self {
            Self::WebRtc(peer) => Some(peer),
            Self::Bluetooth(_) => None,
            #[cfg(test)]
            Self::Mock(_) => None,
        }
    }
}

#[cfg(test)]
use anyhow::anyhow;

#[cfg(test)]
pub struct TestMeshPeer {
    pub ready: bool,
    pub connected: bool,
    pub htl_config: PeerHTLConfig,
    request_response: tokio::sync::Mutex<Option<Vec<u8>>>,
    response_delay: Duration,
    query_events: tokio::sync::Mutex<Vec<Event>>,
    query_delay: Duration,
    sent_frames: tokio::sync::Mutex<Vec<MeshNostrFrame>>,
    close_delay: Duration,
    closed: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl TestMeshPeer {
    pub fn with_response(response: Option<Vec<u8>>) -> Self {
        Self {
            ready: true,
            connected: true,
            htl_config: PeerHTLConfig::from_flags(false, false),
            request_response: tokio::sync::Mutex::new(response),
            response_delay: Duration::ZERO,
            query_events: tokio::sync::Mutex::new(Vec::new()),
            query_delay: Duration::ZERO,
            sent_frames: tokio::sync::Mutex::new(Vec::new()),
            close_delay: Duration::ZERO,
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn with_delayed_response(response: Option<Vec<u8>>, response_delay: Duration) -> Self {
        Self {
            ready: true,
            connected: true,
            htl_config: PeerHTLConfig::from_flags(false, false),
            request_response: tokio::sync::Mutex::new(response),
            response_delay,
            query_events: tokio::sync::Mutex::new(Vec::new()),
            query_delay: Duration::ZERO,
            sent_frames: tokio::sync::Mutex::new(Vec::new()),
            close_delay: Duration::ZERO,
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn with_events(events: Vec<Event>) -> Self {
        Self {
            ready: true,
            connected: true,
            htl_config: PeerHTLConfig::from_flags(false, false),
            request_response: tokio::sync::Mutex::new(None),
            response_delay: Duration::ZERO,
            query_events: tokio::sync::Mutex::new(events),
            query_delay: Duration::ZERO,
            sent_frames: tokio::sync::Mutex::new(Vec::new()),
            close_delay: Duration::ZERO,
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn with_delayed_events(events: Vec<Event>, query_delay: Duration) -> Self {
        Self {
            ready: true,
            connected: true,
            htl_config: PeerHTLConfig::from_flags(false, false),
            request_response: tokio::sync::Mutex::new(None),
            response_delay: Duration::ZERO,
            query_events: tokio::sync::Mutex::new(events),
            query_delay,
            sent_frames: tokio::sync::Mutex::new(Vec::new()),
            close_delay: Duration::ZERO,
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn with_delayed_close(close_delay: Duration) -> Self {
        Self {
            ready: true,
            connected: false,
            htl_config: PeerHTLConfig::from_flags(false, false),
            request_response: tokio::sync::Mutex::new(None),
            response_delay: Duration::ZERO,
            query_events: tokio::sync::Mutex::new(Vec::new()),
            query_delay: Duration::ZERO,
            sent_frames: tokio::sync::Mutex::new(Vec::new()),
            close_delay,
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub async fn request(&self, _hash_hex: &str, _timeout: Duration) -> Result<Option<Vec<u8>>> {
        if !self.response_delay.is_zero() {
            tokio::time::sleep(self.response_delay).await;
        }
        Ok(self.request_response.lock().await.clone())
    }

    pub async fn query_nostr_events(
        &self,
        _filters: Vec<Filter>,
        _timeout: Duration,
    ) -> Result<Vec<Event>> {
        if !self.query_delay.is_zero() {
            tokio::time::sleep(self.query_delay).await;
        }
        Ok(self.query_events.lock().await.clone())
    }

    pub async fn send_mesh_frame_text(&self, frame: &MeshNostrFrame) -> Result<()> {
        self.sent_frames.lock().await.push(frame.clone());
        Ok(())
    }

    pub async fn close(&self) -> Result<()> {
        if !self.close_delay.is_zero() {
            tokio::time::sleep(self.close_delay).await;
        }
        self.closed
            .store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    pub async fn sent_frame_count(&self) -> usize {
        self.sent_frames.lock().await.len()
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
impl MeshPeer {
    pub fn mock_for_tests(peer: TestMeshPeer) -> Self {
        Self::Mock(Arc::new(peer))
    }

    pub fn mock_ref(&self) -> Result<&Arc<TestMeshPeer>> {
        match self {
            Self::Mock(peer) => Ok(peer),
            _ => Err(anyhow!("mesh peer is not a mock")),
        }
    }
}
