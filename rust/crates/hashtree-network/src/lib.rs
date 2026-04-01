//! Mesh transport primitives for HashTree.
//!
//! This crate provides the reusable router, signaling, peer-link, and store
//! layers for hashtree mesh networking. The default production composition uses
//! Nostr websockets for signaling and WebRTC for direct links, but the same
//! abstractions support LAN buses, Bluetooth transports, and simulation.
//!
//! # Overview
//!
//! - **Storage Backend**: Any [`hashtree_core::Store`] implementation
//! - **Peer Discovery**: Any [`SignalingTransport`] implementation
//! - **Data Exchange**: Any [`PeerLink`] / [`PeerLinkFactory`] implementation
//! - **Protocol**: Request/response with hash-based addressing
//! - **Adaptive Selection**: Intelligent peer selection based on performance
//!
//! # Example
//!
//! ```rust,no_run
//! use hashtree_core::MemoryStore;
//! use hashtree_network::{MeshStore, MeshStoreConfig};
//! use nostr_sdk::prelude::*;
//! use std::sync::Arc;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let local_store = Arc::new(MemoryStore::new());
//!     let config = MeshStoreConfig::default();
//!
//!     let mut store = MeshStore::new(local_store, config);
//!
//!     // Generate or load Nostr keys
//!     let keys = Keys::generate();
//!
//!     // Start P2P network
//!     store.start(keys).await?;
//!
//!     // Now store.get() will try local first, then fetch from peers
//!
//!     Ok(())
//! }
//! ```

pub mod channel;
pub mod mesh_store_core;
pub mod mock;
pub mod nostr;
pub mod peer_selector;
pub mod protocol;
pub mod real_factory;
pub mod signaling;
pub mod store;
pub mod transport;
pub mod types;

pub use channel::{ChannelError, LatencyChannel, MockChannel, PeerChannel};
pub use mesh_store_core::{
    build_hedged_wave_plan, normalize_dispatch_config, run_hedged_waves, sync_selector_peers,
    DataPumpStats, HedgedWaveAction, MeshRoutingConfig, MeshStoreCore, ProductionMeshStore,
    RequestDispatchConfig, ResponseBehaviorConfig, SimMeshStore,
};
pub use mock::{
    clear_channel_registry, MockConnectionFactory, MockDataChannel, MockLatencyMode, MockRelay,
    MockRelayTransport,
};
pub use nostr::{decode_signaling_event, encode_signaling_event, NostrRelayTransport};
pub use peer_selector::{
    peer_principal, PeerMetadataSnapshot, PeerSelector, PeerStats, PersistedPeerMetadata,
    SelectionStrategy, SelectorSummary, PEER_METADATA_SNAPSHOT_VERSION,
};
pub use protocol::{
    bytes_to_hash, create_fragment_response, create_quote_request, create_quote_response_available,
    create_quote_response_unavailable, create_request, create_request_with_quote, create_response,
    encode_quote_request, encode_quote_response, encode_request, encode_response, hash_to_bytes,
    hash_to_key, is_fragmented, parse_message, DataMessage, DataQuoteRequest, DataQuoteResponse,
    DataRequest, DataResponse, FRAGMENT_SIZE, MSG_TYPE_QUOTE_REQUEST, MSG_TYPE_QUOTE_RESPONSE,
    MSG_TYPE_REQUEST, MSG_TYPE_RESPONSE,
};
pub use real_factory::WebRtcPeerLinkFactory;
pub use signaling::{MeshRouter, PeerEntry};
pub use store::{MeshStore, MeshStoreError};
pub use transport::{PeerLink, PeerLinkFactory, SignalingTransport, TransportError};
pub use types::{
    classifier_channel, decrement_htl_with_policy, is_polite_peer, should_forward,
    should_forward_htl, validate_mesh_frame, ClassifierRx, ClassifierTx, ClassifyRequest, HtlMode,
    HtlPolicy, IceCandidate, MeshNostrFrame, MeshNostrPayload, MeshStats, MeshStoreConfig,
    PeerHTLConfig, PeerId, PeerPool, PeerState, PoolConfig, PoolSettings, SignalingMessage,
    TimedSeenSet, WebRTCStats, BLOB_REQUEST_POLICY, DATA_CHANNEL_LABEL, DECREMENT_AT_MAX_PROB,
    DECREMENT_AT_MIN_PROB, MAX_HTL, MESH_DEFAULT_HTL, MESH_EVENT_POLICY, MESH_MAX_HTL,
    MESH_PROTOCOL, MESH_PROTOCOL_VERSION, MESH_SIGNALING_EVENT_KIND, NOSTR_KIND_HASHTREE,
};
