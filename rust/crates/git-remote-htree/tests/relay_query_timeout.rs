mod common;

use common::test_relay::{TestRelay, TestRelayOptions};
use git_remote_htree::nostr_client::NostrClient;
use hashtree_config::Config;
use nostr::prelude::Keys;

#[test]
fn test_fetch_refs_uses_partial_relay_results_instead_of_not_found() {
    let good_relay = TestRelay::new(19630);
    let hanging_relay = TestRelay::with_options(
        19631,
        TestRelayOptions {
            // Simulate a relay that never answers kind 30078 REQ.
            ignore_req_kinds: vec![30078],
            ..Default::default()
        },
    );

    let keys = Keys::generate();
    let pubkey_hex = hex::encode(keys.public_key().to_bytes());
    let secret_hex = hex::encode(keys.secret_key().to_secret_bytes());

    let mut config = Config::default();
    config.nostr.relays = vec![good_relay.url(), hanging_relay.url()];
    // Force a deterministic failure *after* event discovery. If event discovery fails,
    // we'd get "Repository ... not found" instead.
    config.blossom.read_servers = vec!["http://127.0.0.1:9".to_string()];
    config.blossom.write_servers = config.blossom.read_servers.clone();

    let publisher = NostrClient::new(&pubkey_hex, Some(secret_hex), None, false, &config)
        .expect("publisher client");
    publisher
        .publish_repo(
            "relay-timeout-repro",
            "1111111111111111111111111111111111111111111111111111111111111111",
            None,
        )
        .expect("publish to relay");

    let mut reader =
        NostrClient::new(&pubkey_hex, None, None, false, &config).expect("reader client");
    let err = reader
        .fetch_refs("relay-timeout-repro")
        .expect_err("fetch should fail at blossom download stage")
        .to_string();

    assert!(
        !err.contains("Repository 'relay-timeout-repro' not found"),
        "should not report missing repo when one relay has the event; got: {}",
        err
    );
    assert!(
        err.contains("Failed to download root hash"),
        "should fail after resolving event and trying blossom download; got: {}",
        err
    );
}

#[test]
fn test_fetch_refs_retries_after_empty_repo_lookup_before_reporting_not_found() {
    let flaky_relay = TestRelay::with_options(
        19632,
        TestRelayOptions {
            // First repo lookup returns EOSE without the historical event, so the
            // client needs to retry discovery instead of surfacing a false "not found".
            respond_empty_req_kinds_once: vec![30078],
            ..Default::default()
        },
    );

    let keys = Keys::generate();
    let pubkey_hex = hex::encode(keys.public_key().to_bytes());
    let secret_hex = hex::encode(keys.secret_key().to_secret_bytes());

    let mut config = Config::default();
    config.nostr.relays = vec![flaky_relay.url()];
    config.blossom.read_servers = vec!["http://127.0.0.1:9".to_string()];
    config.blossom.write_servers = config.blossom.read_servers.clone();

    let publisher = NostrClient::new(&pubkey_hex, Some(secret_hex), None, false, &config)
        .expect("publisher client");
    publisher
        .publish_repo(
            "retry-after-empty-lookup",
            "2222222222222222222222222222222222222222222222222222222222222222",
            None,
        )
        .expect("publish to relay");

    let mut reader =
        NostrClient::new(&pubkey_hex, None, None, false, &config).expect("reader client");
    let err = reader
        .fetch_refs("retry-after-empty-lookup")
        .expect_err("fetch should fail at blossom download stage")
        .to_string();

    assert!(
        !err.contains("Repository 'retry-after-empty-lookup' not found"),
        "should retry repo discovery before reporting missing repo; got: {}",
        err
    );
    assert!(
        err.contains("Failed to download root hash"),
        "should fail after resolving the event and trying blossom download; got: {}",
        err
    );
}
