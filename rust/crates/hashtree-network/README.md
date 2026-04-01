# hashtree-network

Shared mesh networking primitives for hashtree: routing, signaling, negotiated
peer links, and routed store logic.

The core is transport-generic:

- `SignalingTransport` abstracts discovery and negotiation message delivery
- `PeerLink` / `PeerLinkFactory` abstract the direct byte path between peers
- `MeshRouter` coordinates peer discovery plus negotiated link setup
- `MeshStoreCore` runs routed retrieval over any local `Store`

The default production composition uses Nostr relays for signaling and WebRTC
data channels for direct links, but the same router/store core also powers
simulation and can support nearby transports such as Bluetooth.

## Architecture

1. Discovery and negotiation flow through a `SignalingTransport`
2. Direct peer links are created by a `PeerLinkFactory`
3. `MeshStoreCore` routes hash-addressed requests over the connected peers

## Usage

`hashtree-cli` and simulation both build on this crate. The production
`MeshStore` wrapper is intentionally thin and just wires the default Nostr +
WebRTC transport pair into the shared core.
