use anyhow::{Context, Result};
use hashtree_cli::nostr_relay::{NostrRelay, NostrRelayConfig};
use hashtree_cli::socialgraph;
use hashtree_cli::webrtc::{MulticastConfig, MulticastNostrBus};
use nostr::{Alphabet, EventBuilder, Filter, Keys, Kind, SingleLetterTag, Tag, TagKind};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tempfile::TempDir;
use tokio::sync::{mpsc, watch};

const HASHTREE_KIND: u16 = 30078;
const HASHTREE_LABEL: &str = "hashtree";

fn unique_multicast_port() -> u16 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    38000 + (nanos % 2000) as u16
}

fn build_root_event(keys: &Keys, tree_name: &str, hash_hex: &str) -> nostr::Event {
    EventBuilder::new(
        Kind::Custom(HASHTREE_KIND),
        "",
        [
            Tag::identifier(tree_name.to_string()),
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::L)),
                vec![HASHTREE_LABEL.to_string()],
            ),
            Tag::custom(TagKind::Custom("hash".into()), vec![hash_hex.to_string()]),
        ],
    )
    .to_event(keys)
    .expect("root event")
}

async fn make_relay(dir: &TempDir, allowed_pubkey: String) -> Result<Arc<NostrRelay>> {
    let graph_store =
        socialgraph::open_social_graph_store_with_mapsize(dir.path(), Some(128 * 1024 * 1024))?;
    let backend: Arc<dyn socialgraph::SocialGraphBackend> = graph_store.clone();
    let mut allowed = HashSet::new();
    allowed.insert(allowed_pubkey.clone());
    let access = Arc::new(socialgraph::SocialGraphAccessControl::new(
        Arc::clone(&backend),
        0,
        allowed,
    ));

    Ok(Arc::new(NostrRelay::new(
        backend,
        dir.path().to_path_buf(),
        HashSet::from([allowed_pubkey]),
        Some(access),
        NostrRelayConfig {
            spambox_db_max_bytes: 0,
            ..Default::default()
        },
    )?))
}

#[tokio::test]
async fn multicast_bus_announces_root_events_to_other_peers() -> Result<()> {
    let keys_a = Keys::generate();
    let keys_b = Keys::generate();
    let dir_a = TempDir::new()?;
    let dir_b = TempDir::new()?;
    let relay_a = make_relay(&dir_a, keys_a.public_key().to_hex()).await?;
    let relay_b = make_relay(&dir_b, keys_b.public_key().to_hex()).await?;

    let port = unique_multicast_port();
    let multicast = MulticastConfig {
        enabled: true,
        group: "239.255.42.98".to_string(),
        port,
        max_peers: 8,
        announce_interval_ms: 200,
    };

    let bus_a = MulticastNostrBus::bind(multicast.clone(), keys_a.clone(), relay_a.clone()).await?;
    let bus_b = MulticastNostrBus::bind(multicast.clone(), keys_b.clone(), relay_b.clone()).await?;

    let (shutdown_tx_a, shutdown_rx_a) = watch::channel(false);
    let (shutdown_tx_b, shutdown_rx_b) = watch::channel(false);
    let (signal_tx_a, _signal_rx_a) = mpsc::channel(8);
    let (signal_tx_b, _signal_rx_b) = mpsc::channel(8);

    let task_a = tokio::spawn(bus_a.clone().run(shutdown_rx_a, signal_tx_a));
    let task_b = tokio::spawn(bus_b.clone().run(shutdown_rx_b, signal_tx_b));

    let tree_name = "offline-repo";
    let hash_hex = "ab".repeat(32);
    let event = build_root_event(&keys_a, tree_name, &hash_hex);
    relay_a.ingest_trusted_event(event.clone()).await?;

    let filter = Filter::new()
        .kind(Kind::Custom(HASHTREE_KIND))
        .author(keys_a.public_key())
        .custom_tag(
            SingleLetterTag::lowercase(Alphabet::D),
            vec![tree_name.to_string()],
        )
        .custom_tag(
            SingleLetterTag::lowercase(Alphabet::L),
            vec![HASHTREE_LABEL.to_string()],
        );

    let replicated = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let events = relay_b.query_events(&filter, 10).await;
            if events.iter().any(|candidate| candidate.id == event.id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;

    let _ = shutdown_tx_a.send(true);
    let _ = shutdown_tx_b.send(true);
    let _ = task_a.await;
    let _ = task_b.await;

    replicated.context("root update was not announced over multicast")?;
    Ok(())
}

#[tokio::test]
async fn multicast_bus_answers_req_queries_with_root_events() -> Result<()> {
    let keys_a = Keys::generate();
    let keys_b = Keys::generate();
    let dir_a = TempDir::new()?;
    let dir_b = TempDir::new()?;
    let relay_a = make_relay(&dir_a, keys_a.public_key().to_hex()).await?;
    let relay_b = make_relay(&dir_b, keys_b.public_key().to_hex()).await?;

    let port = unique_multicast_port() + 1;
    let multicast = MulticastConfig {
        enabled: true,
        group: "239.255.42.99".to_string(),
        port,
        max_peers: 8,
        announce_interval_ms: 1_000,
    };

    let bus_a = MulticastNostrBus::bind(multicast.clone(), keys_a.clone(), relay_a.clone()).await?;
    let bus_b = MulticastNostrBus::bind(multicast.clone(), keys_b.clone(), relay_b.clone()).await?;

    let (shutdown_tx_a, shutdown_rx_a) = watch::channel(false);
    let (shutdown_tx_b, shutdown_rx_b) = watch::channel(false);
    let (signal_tx_a, _signal_rx_a) = mpsc::channel(8);
    let (signal_tx_b, _signal_rx_b) = mpsc::channel(8);

    let task_a = tokio::spawn(bus_a.clone().run(shutdown_rx_a, signal_tx_a));
    let task_b = tokio::spawn(bus_b.clone().run(shutdown_rx_b, signal_tx_b));

    let tree_name = "lan-query";
    let hash_hex = "cd".repeat(32);
    let event = build_root_event(&keys_b, tree_name, &hash_hex);
    relay_b.ingest_trusted_event(event.clone()).await?;

    let resolved = bus_a
        .query_root(
            &keys_b.public_key().to_hex(),
            tree_name,
            Duration::from_secs(3),
        )
        .await
        .context("multicast root query returned no result")?;

    let _ = shutdown_tx_a.send(true);
    let _ = shutdown_tx_b.send(true);
    let _ = task_a.await;
    let _ = task_b.await;

    assert_eq!(resolved.hash, hash_hex);
    assert_eq!(resolved.event_id, event.id.to_hex());
    Ok(())
}
