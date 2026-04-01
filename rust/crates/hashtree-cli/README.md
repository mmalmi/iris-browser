# hashtree-cli

Hashtree daemon and CLI - content-addressed storage with P2P sync.

## Installation

```bash
# Full CLI install with FUSE mount support by default
cargo install hashtree-cli

# Without social graph
cargo install hashtree-cli --no-default-features --features p2p

# Minimal install without P2P or social graph
cargo install hashtree-cli --no-default-features

# Cashu wallet helper for `htree cashu ...`
cargo install hashtree-cashu-cli
```

## Commands

```bash
# Add content
htree add myfile.txt                    # Add file (CHK-encrypted, shareable)
htree add mydir/ --unencrypted          # Add directory as raw plaintext
htree add myfile.txt --publish mydata   # Add and publish to Nostr

# Push to Blossom servers
htree push <hash>                       # Push to configured servers

# Get/cat content
htree get <hash>                        # Download to file
htree cat <hash>                        # Print to stdout

# Pins
htree pins                              # List pinned content
htree pin <hash>                        # Pin content
htree unpin <hash>                      # Unpin content

# Nostr identity
htree user                              # Show npub
htree publish mydata <hash>             # Publish hash to npub.../mydata
htree follow npub1...                   # Follow user
htree following                         # List followed users

# Daemon
htree start                             # Start P2P daemon
htree start --daemon                    # Start in background
htree start --daemon --log-file /var/log/hashtree.log
htree stop                              # Stop background daemon
htree status                            # Check daemon status

# FUSE mount
htree mount htree://self/mytree          # mounts to ./mytree and errors if it already exists
htree mount htree://npub1.../mytree ~/mnt/mytree
htree mount htree://npub1.../mytree/docs ~/mnt/docs
```

## Social Graph

The daemon maintains a local social graph store. On startup it crawls follow lists (kind 3) from Nostr relays and uses follow distance to control write access to your Blossom server, without a manual allow-list for people in your social circle.

The social graph API is available at `/api/socialgraph/distance/:pubkey`.

## Configuration

Config file: `~/.hashtree/config.toml`

```toml
[blossom]
read_servers = ["https://cdn.iris.to", "https://hashtree.iris.to"]
write_servers = ["https://hashtree.iris.to"]

[nostr]
relays = ["wss://relay.damus.io", "wss://relay.snort.social"]
socialgraph_root = "npub1..."         # defaults to own key
social_graph_crawl_depth = 2          # BFS depth for social graph crawl
max_write_distance = 3                # max follow distance for write access
negentropy_only = false         # require NIP-77 relays for mirror history sync
history_sync_on_reconnect = true
```

Keys file: `~/.hashtree/keys`

```
nsec1abc123... default
nsec1xyz789... work
```

Part of [hashtree-rs](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree).
