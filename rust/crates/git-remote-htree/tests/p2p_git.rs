//! E2E test: Git push/pull between two peers via WebRTC
//!
//! Tests bidirectional git operations with multiple commits going back and forth.
//! Uses local TestRelay for WebRTC signaling - no external network needed.

mod common;

use common::create_test_repo;
use common::test_relay::TestRelay;
use futures::{SinkExt, StreamExt};
use nostr::{
    ClientMessage as NostrClientMessage, EventBuilder, JsonUtil, Keys, Kind, Tag, ToBech32,
};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Test peer with htree daemon
struct TestPeer {
    _data_dir: TempDir,
    _home_dir: TempDir,
    process: Option<Child>,
    port: u16,
    npub: String,
    home_path: PathBuf,
}

struct RootEventData {
    hash: String,
    key: Option<String>,
    encrypted_key: Option<String>,
    self_encrypted_key: Option<String>,
}

impl TestPeer {
    fn new(
        port: u16,
        htree_bin: &str,
        keys: &Keys,
        follow_pubkeys: &[String],
        relay_url: &str,
    ) -> Self {
        let data_dir = TempDir::new().expect("Failed to create data dir");
        let home_dir = TempDir::new().expect("Failed to create home dir");
        let home_path = home_dir.path().to_path_buf();

        let config_dir = home_path.join(".hashtree");
        std::fs::create_dir_all(&config_dir).expect("Failed to create config dir");

        let config_content = format!(
            r#"
[server]
bind_address = "127.0.0.1:{port}"
enable_auth = false
stun_port = 0
enable_webrtc = true
public_writes = true

[nostr]
relays = ["{relay_url}"]

[blossom]
read_servers = ["http://127.0.0.1:{port}"]
write_servers = ["http://127.0.0.1:{port}"]

[sync]
enabled = false
"#,
            relay_url = relay_url,
            port = port,
        );
        std::fs::write(config_dir.join("config.toml"), &config_content)
            .expect("Failed to write config");

        let nsec = keys
            .secret_key()
            .to_bech32()
            .expect("Failed to encode nsec");
        let npub = keys
            .public_key()
            .to_bech32()
            .expect("Failed to encode npub");
        std::fs::write(config_dir.join("keys"), format!("{} self\n", nsec))
            .expect("Failed to write keys");

        if !follow_pubkeys.is_empty() {
            let contacts_json =
                serde_json::to_string(&follow_pubkeys).expect("Failed to serialize contacts");
            std::fs::write(data_dir.path().join("contacts.json"), &contacts_json)
                .expect("Failed to write contacts");
        }

        let process = Command::new(htree_bin)
            .arg("--data-dir")
            .arg(data_dir.path())
            .arg("start")
            .arg("--addr")
            .arg(format!("127.0.0.1:{}", port))
            .env("HOME", &home_path)
            .env(
                "RUST_LOG",
                std::env::var("HTREE_TEST_RUST_LOG").unwrap_or_else(|_| "warn".to_string()),
            )
            .stdout(if std::env::var("HTREE_TEST_STDIO").is_ok() {
                Stdio::inherit()
            } else {
                Stdio::null()
            })
            .stderr(if std::env::var("HTREE_TEST_STDIO").is_ok() {
                Stdio::inherit()
            } else {
                Stdio::null()
            })
            .spawn()
            .expect("Failed to start htree daemon");

        TestPeer {
            _data_dir: data_dir,
            _home_dir: home_dir,
            process: Some(process),
            port,
            npub,
            home_path,
        }
    }

    fn api_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn git(&self, args: &[&str], cwd: &Path) -> std::process::Output {
        let bin_dir = find_bin_dir().expect("Binary dir not found");
        let mut cmd = Command::new("git");
        cmd.args(args)
            .current_dir(cwd)
            .env("HOME", &self.home_path)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    bin_dir.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            );
        if let Ok(log_filter) = std::env::var("HTREE_TEST_GIT_RUST_LOG") {
            cmd.env("RUST_LOG", log_filter);
        }
        cmd.output().expect("Failed to run git")
    }

    fn git_ok(&self, args: &[&str], cwd: &Path) {
        let out = self.git(args, cwd);
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

impl Drop for TestPeer {
    fn drop(&mut self) {
        if let Some(ref mut process) = self.process {
            let _ = process.kill();
            let _ = process.wait();
        }
    }
}

fn find_htree_binary() -> Option<PathBuf> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = PathBuf::from(manifest_dir)
        .parent()?
        .parent()?
        .to_path_buf();
    let debug_bin = workspace_root.join("target/debug/htree");
    let release_bin = workspace_root.join("target/release/htree");
    if debug_bin.exists() {
        Some(debug_bin)
    } else if release_bin.exists() {
        Some(release_bin)
    } else {
        None
    }
}

fn find_bin_dir() -> Option<PathBuf> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = PathBuf::from(manifest_dir)
        .parent()?
        .parent()?
        .to_path_buf();
    let debug_dir = workspace_root.join("target/debug");
    let release_dir = workspace_root.join("target/release");
    if debug_dir.join("git-remote-htree").exists() {
        Some(debug_dir)
    } else if release_dir.join("git-remote-htree").exists() {
        Some(release_dir)
    } else {
        None
    }
}

fn wait_for_server(url: &str) -> bool {
    for _ in 0..30 {
        if let Ok(resp) = reqwest::blocking::get(&format!("{}/health", url)) {
            if resp.status().is_success() {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

fn get_daemon_status(peer_url: &str) -> serde_json::Value {
    let url = format!("{}/api/status", peer_url);
    reqwest::blocking::get(&url)
        .expect("Failed to get status")
        .json()
        .expect("Failed to parse status JSON")
}

fn wait_for_p2p(peer_url: &str, target_pubkey: &str) -> bool {
    for attempt in 1..=30 {
        if let Ok(resp) = reqwest::blocking::get(&format!("{}/api/peers", peer_url)) {
            if let Ok(text) = resp.text() {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(peers) = json.get("peers").and_then(|p| p.as_array()) {
                        for peer in peers {
                            let matches =
                                peer.get("pubkey").and_then(|p| p.as_str()) == Some(target_pubkey);
                            let has_channel = peer
                                .get("has_data_channel")
                                .and_then(|d| d.as_bool())
                                .unwrap_or(false);
                            if matches && has_channel {
                                println!("  P2P connected after {}s", attempt * 2);
                                return true;
                            }
                        }
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    false
}

fn relay_has_repo_event(relay: &TestRelay, repo_name: &str) -> bool {
    relay.stored_events().iter().any(|event| {
        let is_hashtree_kind = event.get("kind").and_then(|k| k.as_u64()) == Some(30078);
        if !is_hashtree_kind {
            return false;
        }
        let Some(tags) = event.get("tags").and_then(|t| t.as_array()) else {
            return false;
        };
        let has_repo = tags.iter().any(|tag| {
            let Some(arr) = tag.as_array() else {
                return false;
            };
            arr.len() >= 2 && arr[0].as_str() == Some("d") && arr[1].as_str() == Some(repo_name)
        });
        let has_label = tags.iter().any(|tag| {
            let Some(arr) = tag.as_array() else {
                return false;
            };
            arr.len() >= 2 && arr[0].as_str() == Some("l") && arr[1].as_str() == Some("hashtree")
        });
        has_repo && has_label
    })
}

fn relay_root_event_data(relay: &TestRelay, repo_name: &str) -> RootEventData {
    let events = relay.stored_events();
    let latest = events
        .iter()
        .filter(|event| {
            let is_hashtree_kind = event.get("kind").and_then(|k| k.as_u64()) == Some(30078);
            if !is_hashtree_kind {
                return false;
            }
            let Some(tags) = event.get("tags").and_then(|t| t.as_array()) else {
                return false;
            };
            let has_repo = tags.iter().any(|tag| {
                let Some(arr) = tag.as_array() else {
                    return false;
                };
                arr.len() >= 2 && arr[0].as_str() == Some("d") && arr[1].as_str() == Some(repo_name)
            });
            let has_label = tags.iter().any(|tag| {
                let Some(arr) = tag.as_array() else {
                    return false;
                };
                arr.len() >= 2
                    && arr[0].as_str() == Some("l")
                    && arr[1].as_str() == Some("hashtree")
            });
            has_repo && has_label
        })
        .max_by(|a, b| {
            let a_created = a.get("created_at").and_then(|v| v.as_i64()).unwrap_or(0);
            let b_created = b.get("created_at").and_then(|v| v.as_i64()).unwrap_or(0);
            a_created.cmp(&b_created).then_with(|| {
                a.get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .cmp(b.get("id").and_then(|v| v.as_str()).unwrap_or(""))
            })
        });

    let Some(event) = latest else {
        panic!(
            "No hashtree root hash found on relay for repo {}",
            repo_name
        );
    };
    let tags = event
        .get("tags")
        .and_then(|t| t.as_array())
        .expect("hashtree event tags must be an array");
    let hash = tags
        .iter()
        .find_map(|tag| {
            let arr = tag.as_array()?;
            if arr.len() >= 2 && arr[0].as_str() == Some("hash") {
                arr[1].as_str().map(ToString::to_string)
            } else {
                None
            }
        })
        .or_else(|| {
            event
                .get("content")
                .and_then(|v| v.as_str())
                .filter(|v| !v.is_empty())
                .map(ToString::to_string)
        })
        .unwrap_or_else(|| {
            panic!(
                "No hash tag/content found on latest hashtree event for repo {}",
                repo_name
            )
        });

    let key = tags.iter().find_map(|tag| {
        let arr = tag.as_array()?;
        if arr.len() >= 2 && arr[0].as_str() == Some("key") {
            arr[1].as_str().map(ToString::to_string)
        } else {
            None
        }
    });
    let encrypted_key = tags.iter().find_map(|tag| {
        let arr = tag.as_array()?;
        if arr.len() >= 2 && arr[0].as_str() == Some("encryptedKey") {
            arr[1].as_str().map(ToString::to_string)
        } else {
            None
        }
    });
    let self_encrypted_key = tags.iter().find_map(|tag| {
        let arr = tag.as_array()?;
        if arr.len() >= 2 && arr[0].as_str() == Some("selfEncryptedKey") {
            arr[1].as_str().map(ToString::to_string)
        } else {
            None
        }
    });

    RootEventData {
        hash,
        key,
        encrypted_key,
        self_encrypted_key,
    }
}

fn publish_local_only_root_event(
    peer_url: &str,
    keys: &Keys,
    repo_name: &str,
    root: &RootEventData,
) {
    let mut tags = vec![
        Tag::parse(&["d", repo_name]).expect("valid d tag"),
        Tag::parse(&["l", "hashtree"]).expect("valid l tag"),
        Tag::parse(&["hash", &root.hash]).expect("valid hash tag"),
    ];
    if let Some(ref key) = root.key {
        tags.push(Tag::parse(&["key", key]).expect("valid key tag"));
    }
    if let Some(ref encrypted_key) = root.encrypted_key {
        tags.push(Tag::parse(&["encryptedKey", encrypted_key]).expect("valid encryptedKey tag"));
    }
    if let Some(ref self_encrypted_key) = root.self_encrypted_key {
        tags.push(
            Tag::parse(&["selfEncryptedKey", self_encrypted_key])
                .expect("valid selfEncryptedKey tag"),
        );
    }

    let event = EventBuilder::new(Kind::Custom(30078), "", tags)
        .to_event(keys)
        .expect("build hashtree root event");
    let msg = NostrClientMessage::event(event).as_json();
    let ws_url = format!(
        "{}/ws",
        peer_url
            .replacen("http://", "ws://", 1)
            .trim_end_matches('/')
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("create runtime");
    rt.block_on(async move {
        let (mut ws, _) = tokio_tungstenite::connect_async(&ws_url)
            .await
            .expect("connect /ws");
        ws.send(WsMessage::Text(msg.into()))
            .await
            .expect("send EVENT");
        let _ = tokio::time::timeout(Duration::from_secs(1), ws.next()).await;
        let _ = ws.close(None).await;
    });
}

fn local_ws_has_repo_event(peer_url: &str, pubkey_hex: &str, repo_name: &str) -> bool {
    let ws_url = format!(
        "{}/ws",
        peer_url
            .replacen("http://", "ws://", 1)
            .trim_end_matches('/')
    );
    let req = serde_json::json!([
        "REQ",
        "peer-only-check",
        {
            "kinds": [30078],
            "authors": [pubkey_hex],
            "#d": [repo_name],
            "#l": ["hashtree"],
            "limit": 5
        }
    ])
    .to_string();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("create runtime");
    rt.block_on(async move {
        let (mut ws, _) = tokio_tungstenite::connect_async(&ws_url)
            .await
            .expect("connect /ws");
        ws.send(WsMessage::Text(req.into()))
            .await
            .expect("send REQ");

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match tokio::time::timeout(remaining, ws.next()).await {
                Ok(Some(Ok(WsMessage::Text(text)))) => {
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
                        continue;
                    };
                    let Some(arr) = v.as_array() else { continue };
                    if arr.first().and_then(|x| x.as_str()) == Some("EVENT") {
                        return true;
                    }
                    if arr.first().and_then(|x| x.as_str()) == Some("EOSE") {
                        return false;
                    }
                }
                _ => break,
            }
        }
        false
    })
}

#[test]
fn test_p2p_git_roundtrip() {
    // Check prerequisites
    let htree_bin = match find_htree_binary() {
        Some(b) => b,
        None => {
            println!("SKIP: htree binary not found");
            return;
        }
    };
    if find_bin_dir().is_none() {
        println!("SKIP: git-remote-htree binary not found");
        return;
    }

    println!("=== P2P Git Roundtrip Test ===\n");

    // Start local relay
    let relay = TestRelay::new(19090);
    let relay_url = relay.url();
    println!("Relay: {}", relay_url);

    // Generate keys
    let keys_a = Keys::generate();
    let keys_b = Keys::generate();
    let pubkey_a = keys_a.public_key().to_hex();
    let pubkey_b = keys_b.public_key().to_hex();

    // Start peers
    println!("Starting peers...");
    let peer_a = TestPeer::new(
        19091,
        htree_bin.to_str().unwrap(),
        &keys_a,
        &[pubkey_b.clone()],
        &relay_url,
    );
    let peer_b = TestPeer::new(
        19092,
        htree_bin.to_str().unwrap(),
        &keys_b,
        &[pubkey_a.clone()],
        &relay_url,
    );
    assert!(wait_for_server(&peer_a.api_url()), "Peer A failed to start");
    assert!(wait_for_server(&peer_b.api_url()), "Peer B failed to start");
    println!("Peers ready\n");

    // === Peer A: Create and push initial repo ===
    println!("1. Peer A: Creating and pushing repo...");
    let repo_a = create_test_repo();
    std::fs::write(repo_a.path().join("count.txt"), "1").unwrap();
    peer_a.git_ok(&["add", "count.txt"], repo_a.path());
    peer_a.git_ok(&["commit", "-m", "Add count"], repo_a.path());
    peer_a.git_ok(
        &["remote", "add", "origin", "htree://self/shared-repo"],
        repo_a.path(),
    );
    let push = peer_a.git(&["push", "-u", "origin", "master"], repo_a.path());
    let stderr = String::from_utf8_lossy(&push.stderr);
    assert!(
        push.status.success() || stderr.contains("-> master"),
        "Initial push failed: {}",
        stderr
    );
    println!("   Pushed (count=1)\n");

    // Wait for P2P connection (both directions)
    println!("2. Waiting for P2P connection...");
    assert!(
        wait_for_p2p(&peer_a.api_url(), &pubkey_b),
        "P2P A->B connection failed"
    );
    assert!(
        wait_for_p2p(&peer_b.api_url(), &pubkey_a),
        "P2P B->A connection failed"
    );

    // === Verify status endpoint shows connection ===
    println!("   Verifying /api/status...");
    let status_a = get_daemon_status(&peer_a.api_url());
    let status_b = get_daemon_status(&peer_b.api_url());

    let webrtc_a = status_a.get("webrtc").expect("status should have webrtc");
    let webrtc_b = status_b.get("webrtc").expect("status should have webrtc");

    assert!(
        webrtc_a
            .get("enabled")
            .and_then(|e| e.as_bool())
            .unwrap_or(false),
        "WebRTC should be enabled"
    );
    assert!(
        webrtc_b
            .get("enabled")
            .and_then(|e| e.as_bool())
            .unwrap_or(false),
        "WebRTC should be enabled"
    );

    let connected_a = webrtc_a
        .get("with_data_channel")
        .and_then(|c| c.as_u64())
        .unwrap_or(0);
    let connected_b = webrtc_b
        .get("with_data_channel")
        .and_then(|c| c.as_u64())
        .unwrap_or(0);

    assert!(
        connected_a >= 1,
        "Peer A should have at least 1 connected peer with data channel, got {}",
        connected_a
    );
    assert!(
        connected_b >= 1,
        "Peer B should have at least 1 connected peer with data channel, got {}",
        connected_b
    );
    println!(
        "   Status verified: A has {} peers, B has {} peers",
        connected_a, connected_b
    );

    // === Peer B: Clone the repo ===
    println!("\n3. Peer B: Cloning repo...");
    let clone_dir_b = TempDir::new().unwrap();
    let repo_b_path = clone_dir_b.path().join("repo");
    peer_b.git_ok(
        &[
            "clone",
            &format!("htree://{}/shared-repo", peer_a.npub),
            "repo",
        ],
        clone_dir_b.path(),
    );

    // Verify clone content
    let count = std::fs::read_to_string(repo_b_path.join("count.txt")).unwrap();
    assert_eq!(count.trim(), "1", "Initial clone should have count=1");
    assert!(
        repo_b_path.join("README.md").exists(),
        "README.md should exist"
    );
    println!("   Cloned and verified (count=1)\n");

    // Configure git for cloned repo
    peer_b.git_ok(&["config", "user.email", "peerb@test.local"], &repo_b_path);
    peer_b.git_ok(&["config", "user.name", "Peer B"], &repo_b_path);

    // === Peer B: Make changes and push ===
    println!("4. Peer B: Updating and pushing...");
    std::fs::write(repo_b_path.join("count.txt"), "2").unwrap();
    std::fs::write(repo_b_path.join("from_b.txt"), "Added by Peer B").unwrap();
    peer_b.git_ok(&["add", "."], &repo_b_path);
    peer_b.git_ok(&["commit", "-m", "Peer B: count=2"], &repo_b_path);
    peer_b.git_ok(
        &["remote", "set-url", "origin", "htree://self/shared-repo"],
        &repo_b_path,
    );
    let push = peer_b.git(&["push", "-u", "origin", "master"], &repo_b_path);
    let stderr = String::from_utf8_lossy(&push.stderr);
    assert!(
        push.status.success() || stderr.contains("-> master"),
        "Peer B push failed: {}",
        stderr
    );
    println!("   Pushed (count=2)\n");

    // === Peer A: Pull changes ===
    println!("5. Peer A: Pulling changes...");
    // Need to set remote to peer B's npub to pull their version
    peer_a.git_ok(
        &[
            "remote",
            "set-url",
            "origin",
            &format!("htree://{}/shared-repo", peer_b.npub),
        ],
        repo_a.path(),
    );
    peer_a.git_ok(&["pull", "--rebase"], repo_a.path());

    let count = std::fs::read_to_string(repo_a.path().join("count.txt")).unwrap();
    assert_eq!(count.trim(), "2", "After pull, count should be 2");
    assert!(
        repo_a.path().join("from_b.txt").exists(),
        "from_b.txt should exist after pull"
    );
    println!("   Pulled and verified (count=2, from_b.txt exists)\n");

    // === Peer A: Make more changes and push ===
    println!("6. Peer A: Updating and pushing...");
    std::fs::write(repo_a.path().join("count.txt"), "3").unwrap();
    std::fs::write(repo_a.path().join("from_a.txt"), "Added by Peer A").unwrap();
    peer_a.git_ok(&["add", "."], repo_a.path());
    peer_a.git_ok(&["commit", "-m", "Peer A: count=3"], repo_a.path());
    peer_a.git_ok(
        &["remote", "set-url", "origin", "htree://self/shared-repo"],
        repo_a.path(),
    );
    let push = peer_a.git(&["push"], repo_a.path());
    let stderr = String::from_utf8_lossy(&push.stderr);
    assert!(
        push.status.success() || stderr.contains("-> master"),
        "Peer A second push failed: {}",
        stderr
    );
    println!("   Pushed (count=3)\n");

    // === Peer B: Pull final changes ===
    println!("7. Peer B: Pulling final changes...");
    peer_b.git_ok(
        &[
            "remote",
            "set-url",
            "origin",
            &format!("htree://{}/shared-repo", peer_a.npub),
        ],
        &repo_b_path,
    );
    peer_b.git_ok(&["pull", "--rebase"], &repo_b_path);

    let count = std::fs::read_to_string(repo_b_path.join("count.txt")).unwrap();
    assert_eq!(count.trim(), "3", "Final count should be 3");
    assert!(
        repo_b_path.join("from_a.txt").exists(),
        "from_a.txt should exist"
    );
    assert!(
        repo_b_path.join("from_b.txt").exists(),
        "from_b.txt should still exist"
    );
    println!("   Pulled and verified (count=3, both files exist)\n");

    // === Peer-assisted root discovery (repo event absent from relay) ===
    println!("8. Peer B: Cloning repo discovered via WebRTC peer root query...");
    let peer_only_repo = "peer-only-repo";
    assert!(
        !relay_has_repo_event(&relay, peer_only_repo),
        "Test relay unexpectedly already has {} event",
        peer_only_repo
    );

    let shared_root = relay_root_event_data(&relay, "shared-repo");
    publish_local_only_root_event(&peer_a.api_url(), &keys_a, peer_only_repo, &shared_root);
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !relay_has_repo_event(&relay, peer_only_repo),
        "peer-only event leaked to relay; test setup invalid"
    );
    assert!(
        local_ws_has_repo_event(&peer_a.api_url(), &pubkey_a, peer_only_repo),
        "Peer A local relay should serve peer-only event over /ws"
    );
    assert!(
        wait_for_p2p(&peer_b.api_url(), &pubkey_a),
        "Peer B must have an active data channel to Peer A for peer-only resolve"
    );

    let resolve_url = format!(
        "{}/api/nostr/resolve/{}/{}",
        peer_b.api_url(),
        pubkey_a,
        peer_only_repo
    );
    let resolve_json: serde_json::Value = reqwest::blocking::get(&resolve_url)
        .expect("peer-b resolve request failed")
        .json()
        .expect("peer-b resolve response parse failed");
    assert_eq!(
        resolve_json.get("hash").and_then(|v| v.as_str()),
        Some(shared_root.hash.as_str()),
        "peer-b daemon should resolve peer-only root via WebRTC: {}",
        resolve_json
    );

    let clone_dir_peer_only = TempDir::new().unwrap();
    let clone_peer_only = peer_b.git(
        &[
            "clone",
            &format!("htree://{}/{}", peer_a.npub, peer_only_repo),
            "repo",
        ],
        clone_dir_peer_only.path(),
    );
    assert!(
        clone_peer_only.status.success(),
        "peer-only clone failed: {}",
        String::from_utf8_lossy(&clone_peer_only.stderr)
    );

    let peer_only_repo_path = clone_dir_peer_only.path().join("repo");
    let count_path = peer_only_repo_path.join("count.txt");
    assert!(
        count_path.exists(),
        "peer-only clone missing count.txt; clone stdout={} stderr={} entries={:?}",
        String::from_utf8_lossy(&clone_peer_only.stdout),
        String::from_utf8_lossy(&clone_peer_only.stderr),
        std::fs::read_dir(&peer_only_repo_path)
            .map(|iter| {
                iter.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    );
    let count = std::fs::read_to_string(count_path).unwrap();
    assert_eq!(
        count.trim(),
        "3",
        "Peer-only clone should resolve latest root"
    );
    assert!(
        peer_only_repo_path.join("from_a.txt").exists(),
        "Peer-only clone should include latest files"
    );
    println!("   Peer-only clone succeeded via local daemon + WebRTC peers\n");

    println!("=== SUCCESS: P2P Git roundtrip complete! ===");
}
