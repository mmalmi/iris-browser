//! Integration test for `htree pr create`
//!
//! Verifies that the CLI publishes a NIP-34 pull request event (kind 1618)
//! with the expected tags and content using a local test relay.
//!
//! Run with: cargo test --package hashtree-cli --test pr_create -- --nocapture

use nostr::{Keys, ToBech32};
use serde_json::Value;
use std::fs;
use std::io;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;

mod test_relay {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use tokio::net::TcpStream;
    use tokio::sync::broadcast;
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    #[derive(Clone)]
    enum PublishAckBehavior {
        Accept,
        Reject(String),
    }

    pub struct TestRelay {
        port: u16,
        events: Arc<Mutex<Vec<Value>>>,
        shutdown: broadcast::Sender<()>,
    }

    impl TestRelay {
        pub fn new() -> Self {
            Self::with_behavior(PublishAckBehavior::Accept)
        }

        pub fn new_rejecting(message: &str) -> Self {
            Self::with_behavior(PublishAckBehavior::Reject(message.to_string()))
        }

        fn with_behavior(ack_behavior: PublishAckBehavior) -> Self {
            let events = Arc::new(Mutex::new(Vec::new()));
            let (shutdown, _) = broadcast::channel(1);

            let std_listener = TcpListener::bind("127.0.0.1:0").expect("bind relay listener");
            let port = std_listener.local_addr().expect("relay local addr").port();
            std_listener.set_nonblocking(true).expect("set nonblocking");

            let events_for_thread = Arc::clone(&events);
            let shutdown_for_thread = shutdown.clone();
            let ack_behavior_for_thread = ack_behavior.clone();

            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("build tokio runtime");

                rt.block_on(async move {
                    let listener =
                        tokio::net::TcpListener::from_std(std_listener).expect("tokio listener");
                    let mut shutdown_rx = shutdown_for_thread.subscribe();

                    loop {
                        tokio::select! {
                            _ = shutdown_rx.recv() => break,
                            accept = listener.accept() => {
                                if let Ok((stream, _)) = accept {
                                    let events = Arc::clone(&events_for_thread);
                                    let ack_behavior = ack_behavior_for_thread.clone();
                                    tokio::spawn(async move {
                                        handle_connection(stream, events, ack_behavior).await;
                                    });
                                }
                            }
                        }
                    }
                });
            });

            std::thread::sleep(Duration::from_millis(100));

            Self {
                port,
                events,
                shutdown,
            }
        }

        pub fn url(&self) -> String {
            format!("ws://127.0.0.1:{}", self.port)
        }

        pub fn wait_for_kind(&self, kind: u64, timeout: Duration) -> Option<Value> {
            let start = Instant::now();
            loop {
                if let Some(event) = self
                    .events
                    .lock()
                    .expect("relay events lock")
                    .iter()
                    .find(|event| event.get("kind").and_then(Value::as_u64) == Some(kind))
                    .cloned()
                {
                    return Some(event);
                }

                if start.elapsed() >= timeout {
                    return None;
                }

                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }

    impl Drop for TestRelay {
        fn drop(&mut self) {
            let _ = self.shutdown.send(());
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    async fn handle_connection(
        stream: TcpStream,
        events: Arc<Mutex<Vec<Value>>>,
        ack_behavior: PublishAckBehavior,
    ) {
        let ws_stream = match accept_async(stream).await {
            Ok(ws) => ws,
            Err(_) => return,
        };

        let (mut write, mut read) = ws_stream.split();

        while let Some(msg) = read.next().await {
            let msg = match msg {
                Ok(Message::Text(text)) => text,
                Ok(Message::Ping(data)) => {
                    let _ = write.send(Message::Pong(data)).await;
                    continue;
                }
                Ok(Message::Close(_)) => break,
                _ => continue,
            };

            let parsed: Vec<Value> = match serde_json::from_str(&msg) {
                Ok(value) => value,
                Err(_) => continue,
            };

            let Some(msg_type) = parsed.first().and_then(Value::as_str) else {
                continue;
            };

            match msg_type {
                "EVENT" => {
                    let Some(event) = parsed.get(1).cloned() else {
                        continue;
                    };
                    let Some(id) = event.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    let id = id.to_string();

                    let ok = match &ack_behavior {
                        PublishAckBehavior::Accept => {
                            events.lock().expect("relay events lock").push(event);
                            serde_json::json!(["OK", id, true, ""])
                        }
                        PublishAckBehavior::Reject(message) => {
                            serde_json::json!(["OK", id, false, message])
                        }
                    };
                    let _ = write.send(Message::Text(ok.to_string())).await;
                }
                "REQ" => {
                    // Minimal support: replay all stored events and send EOSE.
                    let Some(sub_id) = parsed.get(1).and_then(Value::as_str) else {
                        continue;
                    };

                    let snapshot = events.lock().expect("relay events lock").clone();
                    for event in snapshot {
                        let msg = serde_json::json!(["EVENT", sub_id, event]);
                        let _ = write.send(Message::Text(msg.to_string())).await;
                    }
                    let eose = serde_json::json!(["EOSE", sub_id]);
                    let _ = write.send(Message::Text(eose.to_string())).await;
                }
                "CLOSE" => {}
                _ => {}
            }
        }
    }
}

fn htree_bin() -> String {
    std::env::var("CARGO_BIN_EXE_htree").unwrap_or_else(|_| {
        if let Ok(target_dir) = std::env::var("CARGO_TARGET_DIR") {
            return Path::new(&target_dir)
                .join("debug/htree")
                .to_string_lossy()
                .to_string();
        }

        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        manifest_dir
            .parent()
            .and_then(Path::parent)
            .map(|rust_root| rust_root.join("target/debug/htree"))
            .expect("rust workspace root")
            .to_string_lossy()
            .to_string()
    })
}

fn run_git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to run git {:?}: {}", args, e));
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to run git {:?}: {}", args, e));
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn write_keys_file(config_dir: &Path, nsec: &str) -> io::Result<()> {
    fs::create_dir_all(config_dir)?;
    fs::write(config_dir.join("keys"), format!("{nsec} self\n"))
}

fn find_tag<'a>(event: &'a Value, name: &str) -> Option<&'a [Value]> {
    event.get("tags")?.as_array()?.iter().find_map(|tag| {
        let arr = tag.as_array()?;
        if arr.first()?.as_str()? == name {
            Some(arr.as_slice())
        } else {
            None
        }
    })
}

fn tag_value<'a>(event: &'a Value, name: &str) -> Option<&'a str> {
    find_tag(event, name)?.get(1)?.as_str()
}

struct PrCreateFixture {
    relay: test_relay::TestRelay,
    _tmp: TempDir,
    config_dir: PathBuf,
    data_dir: PathBuf,
    repo_dir: PathBuf,
    self_npub: String,
    self_pubkey_hex: String,
    target_npub: String,
    target_pubkey_hex: String,
    target_repo_name: String,
}

impl PrCreateFixture {
    fn target_repo_url(&self) -> String {
        format!("htree://{}/{}", self.target_npub, self.target_repo_name)
    }

    fn run_htree_in(&self, dir: &Path, args: &[&str]) -> Output {
        Command::new(htree_bin())
            .current_dir(dir)
            .env("HOME", self._tmp.path())
            .env("HTREE_CONFIG_DIR", &self.config_dir)
            .env("HTREE_DATA_DIR", &self.data_dir)
            .env("NOSTR_RELAYS", self.relay.url())
            .env("HTREE_PREFER_LOCAL_RELAY", "0")
            .args(args)
            .output()
            .expect("run htree")
    }

    fn run_htree(&self, args: &[&str]) -> Output {
        self.run_htree_in(&self.repo_dir, args)
    }
}

fn setup_pr_create_fixture() -> PrCreateFixture {
    setup_pr_create_fixture_with_relay(test_relay::TestRelay::new())
}

fn setup_pr_create_fixture_with_relay(relay: test_relay::TestRelay) -> PrCreateFixture {
    let tmp = TempDir::new().expect("temp dir");
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("create data dir");

    let self_keys = Keys::generate();
    let self_npub = self_keys.public_key().to_bech32().expect("self npub");
    let self_pubkey_hex = hex::encode(self_keys.public_key().to_bytes());
    let self_nsec = self_keys
        .secret_key()
        .to_bech32()
        .expect("self nsec bech32");
    write_keys_file(&config_dir, &self_nsec).expect("write keys file");

    let target_keys = Keys::generate();
    let target_npub = target_keys.public_key().to_bech32().expect("target npub");
    let target_pubkey_hex = hex::encode(target_keys.public_key().to_bytes());

    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).expect("create repo dir");

    run_git(&repo_dir, &["init", "-b", "master"]);
    run_git(&repo_dir, &["config", "user.email", "test@example.com"]);
    run_git(&repo_dir, &["config", "user.name", "Test User"]);

    fs::write(repo_dir.join("README.md"), "# pr-create-test\n").expect("write readme");
    run_git(&repo_dir, &["add", "README.md"]);
    run_git(&repo_dir, &["commit", "-m", "initial"]);

    run_git(&repo_dir, &["checkout", "-b", "feature"]);
    fs::write(repo_dir.join("feature.txt"), "hello from feature\n").expect("write feature");
    run_git(&repo_dir, &["add", "feature.txt"]);
    run_git(&repo_dir, &["commit", "-m", "feature change"]);

    PrCreateFixture {
        relay,
        _tmp: tmp,
        config_dir,
        data_dir,
        repo_dir,
        self_npub,
        self_pubkey_hex,
        target_npub,
        target_pubkey_hex,
        target_repo_name: "target-repo".to_string(),
    }
}

#[test]
fn test_pr_create_publishes_kind_1618_event() {
    let fixture = setup_pr_create_fixture();
    let commit_tip = git_stdout(&fixture.repo_dir, &["rev-parse", "HEAD"]);

    let title = "CLI PR create test";
    let description = "Verifies kind 1618 event tags";
    let target_repo = fixture.target_repo_url();
    let output = fixture.run_htree(&[
        "pr",
        "create",
        &target_repo,
        "--title",
        title,
        "--description",
        description,
        "--target-branch",
        "master",
    ]);

    assert!(
        output.status.success(),
        "htree pr create failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("PR created:"),
        "stdout missing PR created: {stdout}"
    );
    assert!(
        stdout.contains("Event: nevent"),
        "stdout missing nevent: {stdout}"
    );
    assert!(
        stdout.contains("View: https://git.iris.to/"),
        "stdout missing view link: {stdout}"
    );
    assert!(
        stderr.contains("has no upstream tracking branch"),
        "stderr missing upstream warning.\nstderr:\n{}",
        stderr
    );

    let event = fixture
        .relay
        .wait_for_kind(1618, Duration::from_secs(5))
        .expect("expected kind 1618 PR event on relay");

    assert_eq!(event.get("kind").and_then(Value::as_u64), Some(1618));
    assert_eq!(
        event.get("content").and_then(Value::as_str),
        Some(description)
    );
    assert_eq!(
        event.get("pubkey").and_then(Value::as_str),
        Some(fixture.self_pubkey_hex.as_str())
    );

    let expected_a = format!(
        "30617:{}:{}",
        fixture.target_pubkey_hex, fixture.target_repo_name
    );
    let expected_clone = format!("htree://{}/{}", fixture.self_npub, fixture.target_repo_name);

    assert_eq!(tag_value(&event, "a"), Some(expected_a.as_str()));
    assert_eq!(
        tag_value(&event, "p"),
        Some(fixture.target_pubkey_hex.as_str())
    );
    assert_eq!(tag_value(&event, "subject"), Some(title));
    assert_eq!(tag_value(&event, "branch"), Some("feature"));
    assert_eq!(tag_value(&event, "target-branch"), Some("master"));
    assert_eq!(tag_value(&event, "c"), Some(commit_tip.as_str()));
    assert_eq!(tag_value(&event, "clone"), Some(expected_clone.as_str()));
    assert_eq!(tag_value(&event, "description"), Some(description));
}

#[test]
fn test_pr_create_accepts_git_remote_alias() {
    let fixture = setup_pr_create_fixture();
    let commit_tip = git_stdout(&fixture.repo_dir, &["rev-parse", "HEAD"]);
    run_git(
        &fixture.repo_dir,
        &["remote", "add", "htree", &fixture.target_repo_url()],
    );

    let output = fixture.run_htree(&[
        "pr",
        "create",
        "htree",
        "--title",
        "Alias target PR",
        "--target-branch",
        "master",
    ]);

    assert!(
        output.status.success(),
        "htree pr create via alias failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let event = fixture
        .relay
        .wait_for_kind(1618, Duration::from_secs(5))
        .expect("expected kind 1618 PR event on relay");
    let expected_a = format!(
        "30617:{}:{}",
        fixture.target_pubkey_hex, fixture.target_repo_name
    );
    assert_eq!(tag_value(&event, "a"), Some(expected_a.as_str()));
    assert_eq!(tag_value(&event, "branch"), Some("feature"));
    assert_eq!(tag_value(&event, "c"), Some(commit_tip.as_str()));
}

#[test]
fn test_pr_create_accepts_git_remote_alias_with_slash() {
    let fixture = setup_pr_create_fixture();
    let commit_tip = git_stdout(&fixture.repo_dir, &["rev-parse", "HEAD"]);
    run_git(
        &fixture.repo_dir,
        &["remote", "add", "team/htree", &fixture.target_repo_url()],
    );

    let output = fixture.run_htree(&[
        "pr",
        "create",
        "team/htree",
        "--title",
        "Slash alias target PR",
        "--target-branch",
        "master",
    ]);

    assert!(
        output.status.success(),
        "htree pr create via slash alias failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let event = fixture
        .relay
        .wait_for_kind(1618, Duration::from_secs(5))
        .expect("expected kind 1618 PR event on relay");
    let expected_a = format!(
        "30617:{}:{}",
        fixture.target_pubkey_hex, fixture.target_repo_name
    );
    assert_eq!(tag_value(&event, "a"), Some(expected_a.as_str()));
    assert_eq!(tag_value(&event, "branch"), Some("feature"));
    assert_eq!(tag_value(&event, "c"), Some(commit_tip.as_str()));
}

#[test]
fn test_pr_create_infers_single_htree_remote_when_repo_omitted() {
    let fixture = setup_pr_create_fixture();
    run_git(
        &fixture.repo_dir,
        &["remote", "add", "htree", &fixture.target_repo_url()],
    );

    let output = fixture.run_htree(&[
        "pr",
        "create",
        "--title",
        "Inferred target PR",
        "--target-branch",
        "master",
    ]);

    assert!(
        output.status.success(),
        "htree pr create without repo failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let event = fixture
        .relay
        .wait_for_kind(1618, Duration::from_secs(5))
        .expect("expected kind 1618 PR event on relay");
    let expected_a = format!(
        "30617:{}:{}",
        fixture.target_pubkey_hex, fixture.target_repo_name
    );
    assert_eq!(tag_value(&event, "a"), Some(expected_a.as_str()));
}

#[test]
fn test_pr_create_sanitizes_fragment_from_repo_and_clone_tags() {
    let fixture = setup_pr_create_fixture();
    let target_repo_with_fragment = format!("{}#k=super-secret", fixture.target_repo_url());

    let output = fixture.run_htree(&[
        "pr",
        "create",
        &target_repo_with_fragment,
        "--title",
        "Fragment sanitize test",
        "--target-branch",
        "master",
    ]);

    assert!(
        output.status.success(),
        "htree pr create with fragment URL failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let event = fixture
        .relay
        .wait_for_kind(1618, Duration::from_secs(5))
        .expect("expected kind 1618 PR event on relay");
    let expected_a = format!(
        "30617:{}:{}",
        fixture.target_pubkey_hex, fixture.target_repo_name
    );
    let expected_clone = format!("htree://{}/{}", fixture.self_npub, fixture.target_repo_name);
    assert_eq!(tag_value(&event, "a"), Some(expected_a.as_str()));
    assert_eq!(tag_value(&event, "clone"), Some(expected_clone.as_str()));
    assert!(
        !event.to_string().contains("super-secret"),
        "secret fragment leaked into event: {}",
        event
    );
}

#[test]
fn test_pr_create_fails_when_no_relay_confirms_event() {
    let fixture = setup_pr_create_fixture_with_relay(test_relay::TestRelay::new_rejecting(
        "blocked for test",
    ));

    let output = fixture.run_htree(&[
        "pr",
        "create",
        &fixture.target_repo_url(),
        "--title",
        "Unconfirmed PR should fail",
        "--target-branch",
        "master",
    ]);

    assert!(
        !output.status.success(),
        "expected pr create to fail when no relay confirms event.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stdout.contains("PR created:"),
        "stdout incorrectly reported success: {stdout}"
    );
    assert!(
        stderr.contains("not confirmed by any relay")
            || stderr.contains("Failed to publish PR event"),
        "stderr missing publish failure/zero-confirmation error.\nstderr:\n{}",
        stderr
    );

    assert!(
        fixture
            .relay
            .wait_for_kind(1618, Duration::from_millis(250))
            .is_none(),
        "rejected relay should not store the PR event"
    );
}

#[test]
fn test_pr_create_rejects_detached_head_without_branch_override() {
    let fixture = setup_pr_create_fixture();
    let target_repo = fixture.target_repo_url();
    run_git(&fixture.repo_dir, &["checkout", "--detach", "HEAD"]);

    let output = fixture.run_htree(&[
        "pr",
        "create",
        &target_repo,
        "--title",
        "Detached head should fail",
        "--target-branch",
        "master",
    ]);

    assert!(
        !output.status.success(),
        "expected detached HEAD to fail.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Detached HEAD") && stderr.contains("--branch"),
        "stderr missing detached HEAD guidance.\nstderr:\n{}",
        stderr
    );
}

#[test]
fn test_pr_create_fails_outside_git_repo() {
    let fixture = setup_pr_create_fixture();
    let non_repo_dir = fixture._tmp.path().join("not-a-repo");
    fs::create_dir_all(&non_repo_dir).expect("create non-repo dir");

    let output = fixture.run_htree_in(
        &non_repo_dir,
        &[
            "pr",
            "create",
            &fixture.target_repo_url(),
            "--title",
            "Outside repo should fail",
            "--target-branch",
            "master",
        ],
    );

    assert!(
        !output.status.success(),
        "expected pr create outside git repo to fail.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not a git repository") || stderr.contains("git work tree"),
        "stderr missing explicit non-repo message.\nstderr:\n{}",
        stderr
    );

    assert!(
        fixture
            .relay
            .wait_for_kind(1618, Duration::from_millis(250))
            .is_none(),
        "should fail before publishing any PR event"
    );
}
