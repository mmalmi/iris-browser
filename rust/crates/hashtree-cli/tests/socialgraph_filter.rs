use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use nostr::{EventBuilder, JsonUtil, Kind, Tag, ToBech32};
use tempfile::TempDir;

#[test]
fn socialgraph_filter_drops_unknown_and_overmuted() {
    let temp = TempDir::new().unwrap();
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();

    let root_keys = nostr::Keys::generate();
    let root_pk = root_keys.public_key().to_bytes();
    let root_npub = root_keys.public_key().to_bech32().unwrap();

    let config = format!(
        "[nostr]\n\
socialgraph_root = \"{}\"\n\
max_write_distance = 2\n",
        root_npub
    );
    fs::write(config_dir.join("config.toml"), config).unwrap();

    let graph_store = hashtree_cli::socialgraph::open_social_graph_store(&data_dir).unwrap();
    hashtree_cli::socialgraph::set_social_graph_root(&graph_store, &root_pk);
    thread::sleep(Duration::from_millis(100));

    let alice_keys = nostr::Keys::generate();
    let alice_pk = alice_keys.public_key().to_bytes();
    let alice_tag = Tag::public_key(alice_keys.public_key());
    let root_follows_alice = EventBuilder::new(Kind::ContactList, "", vec![alice_tag])
        .to_event(&root_keys)
        .unwrap();
    hashtree_cli::socialgraph::ingest_event(&graph_store, "sub1", &root_follows_alice.as_json());

    let charlie_keys = nostr::Keys::generate();
    let charlie_pk = charlie_keys.public_key().to_bytes();
    let charlie_tag = Tag::public_key(charlie_keys.public_key());
    let alice_follows_charlie = EventBuilder::new(Kind::ContactList, "", vec![charlie_tag.clone()])
        .to_event(&alice_keys)
        .unwrap();
    hashtree_cli::socialgraph::ingest_event(&graph_store, "sub2", &alice_follows_charlie.as_json());

    let root_mutes_charlie = EventBuilder::new(Kind::Custom(10000), "", vec![charlie_tag])
        .to_event(&root_keys)
        .unwrap();
    hashtree_cli::socialgraph::ingest_event(&graph_store, "sub3", &root_mutes_charlie.as_json());

    thread::sleep(Duration::from_millis(200));
    drop(graph_store);

    let alice_note = EventBuilder::new(Kind::TextNote, "hello from alice", vec![])
        .to_event(&alice_keys)
        .unwrap();
    let bob_keys = nostr::Keys::generate();
    let bob_note = EventBuilder::new(Kind::TextNote, "hello from bob", vec![])
        .to_event(&bob_keys)
        .unwrap();
    let charlie_note = EventBuilder::new(Kind::TextNote, "hello from charlie", vec![])
        .to_event(&charlie_keys)
        .unwrap();

    let input = format!(
        "{}\n{}\n{}\n",
        alice_note.as_json(),
        bob_note.as_json(),
        charlie_note.as_json()
    );

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_htree"));
    cmd.env("HTREE_CONFIG_DIR", &config_dir)
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("socialgraph")
        .arg("filter")
        .arg("--max-distance")
        .arg("2")
        .arg("--overmute-threshold")
        .arg("1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().unwrap();
    {
        let stdin = child.stdin.as_mut().unwrap();
        stdin.write_all(input.as_bytes()).unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    let mut lines = stdout.lines();
    let first = lines.next().expect("expected one event");
    assert!(lines.next().is_none(), "expected only one event in output");

    let value: serde_json::Value = serde_json::from_str(first).unwrap();
    let pubkey = value["pubkey"].as_str().unwrap();
    assert_eq!(pubkey, hex::encode(alice_pk));
    assert_ne!(pubkey, hex::encode(charlie_pk));
}
