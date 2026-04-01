# hashtree-nostr

Hashtree-native Nostr event indexes for Rust.

This crate stores signed Nostr events as deterministic content-addressed blobs
and maintains thin B-tree indexes for lookups like:

- `event_id -> event blob`
- author/time scans
- author/kind/time scans
- replaceable and parameterized-replaceable event winners

The layout is kept interoperable with the TypeScript `@hashtree/nostr`
implementation. The manifest exposes the event-id index under `by-id`.

Part of [hashtree](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree).
