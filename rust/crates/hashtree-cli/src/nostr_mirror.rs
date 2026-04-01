use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use anyhow::{Context, Result};
use hashtree_nostr::{
    CrawlConfig, CrawlReport, ListEventsOptions, NostrBridge, NostrEventStore, RelayFetchMode,
};
use nostr::{
    Alphabet, Event, EventBuilder, Filter, Kind, PublicKey, SingleLetterTag, Tag, TagKind,
    Timestamp,
};
use nostr_sdk::{
    pool::RelayLimits, prelude::RelayPoolNotification, Client, Keys, Options, RelayStatus,
};
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::socialgraph::crawler::SOCIALGRAPH_RELAY_EVENT_MAX_SIZE;
use crate::socialgraph::{self, SocialGraphBackend, SocialGraphStore};
use crate::HashtreeStore;

#[cfg(not(test))]
const MIRROR_STARTUP_DELAY: Duration = Duration::from_secs(8);
#[cfg(test)]
const MIRROR_STARTUP_DELAY: Duration = Duration::from_millis(50);

#[cfg(not(test))]
const MIRROR_CONNECT_SETTLE_DELAY: Duration = Duration::from_secs(1);
#[cfg(test)]
const MIRROR_CONNECT_SETTLE_DELAY: Duration = Duration::from_millis(250);

#[cfg(not(test))]
const MIRROR_AUTHOR_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
#[cfg(test)]
const MIRROR_AUTHOR_REFRESH_INTERVAL: Duration = Duration::from_millis(100);

#[cfg(not(test))]
const MIRROR_RECONNECT_HISTORY_SYNC_COOLDOWN: Duration = Duration::from_secs(30);
#[cfg(test)]
const MIRROR_RECONNECT_HISTORY_SYNC_COOLDOWN: Duration = Duration::from_millis(100);

const DEFAULT_HISTORY_KINDS: [u16; 2] = [0, 3];
const DEFAULT_PROFILE_SEARCH_TREE_NAME: &str = "profile-search";

#[cfg(not(test))]
const MIRROR_MISSING_PROFILE_BACKFILL_INTERVAL: Duration = Duration::from_secs(300);
#[cfg(test)]
const MIRROR_MISSING_PROFILE_BACKFILL_INTERVAL: Duration = Duration::from_millis(100);

#[cfg(not(test))]
const MIRROR_ROOT_PUBLISH_DEBOUNCE: Duration = Duration::from_secs(5);
#[cfg(test)]
const MIRROR_ROOT_PUBLISH_DEBOUNCE: Duration = Duration::from_millis(20);

#[cfg(not(test))]
const MIRROR_ROOT_PUBLISH_MAX_STALENESS: Duration = Duration::from_secs(30);
#[cfg(test)]
const MIRROR_ROOT_PUBLISH_MAX_STALENESS: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct NostrMirrorConfig {
    pub relays: Vec<String>,
    pub publish_relays: Vec<String>,
    pub max_follow_distance: u32,
    pub overmute_threshold: f64,
    pub author_batch_size: usize,
    pub history_sync_author_chunk_size: usize,
    pub missing_profile_backfill_batch_size: usize,
    pub fetch_timeout: Duration,
    pub relay_event_max_size: Option<u32>,
    pub require_negentropy: bool,
    pub kinds: Vec<u16>,
    pub history_sync_on_start: bool,
    pub history_sync_on_reconnect: bool,
    pub published_profile_search_tree_name: Option<String>,
}

impl Default for NostrMirrorConfig {
    fn default() -> Self {
        Self {
            relays: Vec::new(),
            publish_relays: Vec::new(),
            max_follow_distance: 2,
            overmute_threshold: 1.0,
            author_batch_size: 256,
            history_sync_author_chunk_size: 5_000,
            missing_profile_backfill_batch_size: 5_000,
            fetch_timeout: Duration::from_secs(15),
            relay_event_max_size: Some(SOCIALGRAPH_RELAY_EVENT_MAX_SIZE),
            require_negentropy: false,
            kinds: DEFAULT_HISTORY_KINDS.to_vec(),
            history_sync_on_start: true,
            history_sync_on_reconnect: true,
            published_profile_search_tree_name: Some(DEFAULT_PROFILE_SEARCH_TREE_NAME.to_string()),
        }
    }
}

#[derive(Debug, Default)]
struct RootPublishState {
    pending_root: Option<hashtree_core::Cid>,
    last_changed_at: Option<Instant>,
    dirty_since: Option<Instant>,
    last_published_root: Option<hashtree_core::Cid>,
    last_published_at: Option<Instant>,
}

pub struct BackgroundNostrMirror {
    config: NostrMirrorConfig,
    store: Arc<HashtreeStore>,
    graph_store: Arc<SocialGraphStore>,
    client: Client,
    publish_client: Option<Client>,
    profile_search_publish_state: Mutex<RootPublishState>,
    missing_profile_cursor: Mutex<usize>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
}

impl BackgroundNostrMirror {
    pub async fn new(
        config: NostrMirrorConfig,
        store: Arc<HashtreeStore>,
        graph_store: Arc<SocialGraphStore>,
        publish_keys: Option<Keys>,
    ) -> Result<Self> {
        let client = if let Some(max_size) = config.relay_event_max_size {
            let mut limits = RelayLimits::default();
            limits.events.max_size = Some(max_size);
            Client::with_opts(Keys::generate(), Options::new().relay_limits(limits))
        } else {
            Client::new(Keys::generate())
        };
        for relay in &config.relays {
            client
                .add_relay(relay)
                .await
                .with_context(|| format!("add mirror relay {relay}"))?;
        }
        client.connect().await;

        let publish_client = if let Some(keys) = publish_keys {
            if config.publish_relays.is_empty() {
                None
            } else {
                let client = Client::new(keys);
                for relay in &config.publish_relays {
                    client
                        .add_relay(relay)
                        .await
                        .with_context(|| format!("add mirror publish relay {relay}"))?;
                }
                client.connect().await;
                Some(client)
            }
        } else {
            None
        };

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Ok(Self {
            config,
            store,
            graph_store,
            client,
            publish_client,
            profile_search_publish_state: Mutex::new(RootPublishState::default()),
            missing_profile_cursor: Mutex::new(0),
            shutdown_tx,
            shutdown_rx,
        })
    }

    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    pub async fn run(&self) -> Result<()> {
        if self.config.relays.is_empty() || self.config.max_follow_distance == 0 {
            return Ok(());
        }

        info!(
            "Nostr mirror starting: relays={} max_follow_distance={} negentropy_only={} kinds={:?} history_sync_author_chunk_size={} history_sync_on_start={} history_sync_on_reconnect={}",
            self.config.relays.len(),
            self.config.max_follow_distance,
            self.config.require_negentropy,
            self.config.kinds,
            self.config.history_sync_author_chunk_size.max(1),
            self.config.history_sync_on_start,
            self.config.history_sync_on_reconnect
        );

        tokio::time::sleep(MIRROR_STARTUP_DELAY).await;
        tokio::time::sleep(MIRROR_CONNECT_SETTLE_DELAY).await;
        let live_since = Timestamp::now();
        self.note_profile_search_root_change()?;

        let initial_authors = self.collect_authors()?;
        if initial_authors.is_empty() {
            info!("Nostr mirror: no social-graph authors to mirror yet");
        } else if self.config.history_sync_on_start {
            if self.should_backfill_missing_profiles(None) {
                let missing_profile_authors = self.collect_missing_profile_authors(
                    self.config.missing_profile_backfill_batch_size,
                )?;
                if !missing_profile_authors.is_empty() {
                    info!(
                        "Nostr mirror missing-profile backfill starting: authors={}",
                        missing_profile_authors.len()
                    );
                    self.history_sync_authors_with_kinds(
                        missing_profile_authors,
                        &[Kind::Metadata.as_u16()],
                    )
                    .await?;
                }
            }
            self.history_sync_authors(initial_authors.clone()).await?;
        }

        let mut subscribed_authors = HashSet::new();
        self.subscribe_authors_since(&initial_authors, live_since, &mut subscribed_authors)
            .await?;

        let mut relay_statuses = self.capture_relay_statuses().await;
        let mut last_reconnect_history_sync_at: Option<Instant> = None;
        let mut last_missing_profile_backfill_at: Option<Instant> = None;
        let mut notifications = self.client.notifications();
        let mut shutdown_rx = self.shutdown_rx.clone();
        let mut refresh_interval = tokio::time::interval(MIRROR_AUTHOR_REFRESH_INTERVAL);
        let mut publish_interval = tokio::time::interval(MIRROR_ROOT_PUBLISH_DEBOUNCE);

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
                _ = refresh_interval.tick() => {
                    let authors = self.collect_authors()?;
                    let new_authors = authors
                        .into_iter()
                        .filter(|author| !subscribed_authors.contains(author))
                        .collect::<Vec<_>>();
                    if !new_authors.is_empty() {
                        debug!(
                            "Nostr mirror discovered {} newly reachable author(s)",
                            new_authors.len()
                        );
                        self.history_sync_authors(new_authors.clone()).await?;
                        self.subscribe_authors_since(
                            &new_authors,
                            Timestamp::now(),
                            &mut subscribed_authors,
                        )
                        .await?;
                    }
                    if self.should_backfill_missing_profiles(last_missing_profile_backfill_at) {
                        let missing_profile_authors = self.collect_missing_profile_authors(
                            self.config.missing_profile_backfill_batch_size,
                        )?;
                        if !missing_profile_authors.is_empty() {
                            info!(
                                "Nostr mirror missing-profile backfill starting: authors={}",
                                missing_profile_authors.len()
                            );
                            self.history_sync_authors_with_kinds(
                                missing_profile_authors,
                                &[Kind::Metadata.as_u16()],
                            )
                            .await?;
                            last_missing_profile_backfill_at = Some(Instant::now());
                        }
                    }
                }
                _ = publish_interval.tick() => {
                    if let Err(err) = self.maybe_publish_profile_search_root(false).await {
                        warn!("Nostr mirror profile-search publish failed: {:#}", err);
                    }
                }
                notification = notifications.recv() => {
                    match notification {
                        Ok(RelayPoolNotification::Event { event, .. }) => {
                            self.ingest_live_event(&event)?;
                        }
                        Ok(RelayPoolNotification::RelayStatus { relay_url, status }) => {
                            let relay_url = relay_url.to_string();
                            let previous = relay_statuses.insert(relay_url.clone(), status);
                            if Self::should_history_sync_on_reconnect(
                                self.config.history_sync_on_reconnect,
                                previous,
                                status,
                            ) && Self::should_run_reconnect_history_sync(
                                    last_reconnect_history_sync_at.as_ref(),
                                )
                            {
                                let authors = self.collect_authors()?;
                                if !authors.is_empty() {
                                    info!(
                                        "Nostr mirror relay reconnected; running catch-up history sync: relay={} authors={} negentropy_only={}",
                                        relay_url,
                                        authors.len(),
                                        self.config.require_negentropy
                                    );
                                    self.history_sync_authors(authors).await?;
                                    last_reconnect_history_sync_at = Some(Instant::now());
                                }
                            }
                        }
                        Ok(RelayPoolNotification::Shutdown) => break,
                        Ok(_) => {}
                        Err(err) => {
                            warn!("Nostr mirror notification error: {}", err);
                            break;
                        }
                    }
                }
            }
        }

        let _ = self.client.disconnect().await;
        if let Some(client) = self.publish_client.as_ref() {
            let _ = client.disconnect().await;
        }
        Ok(())
    }

    async fn capture_relay_statuses(&self) -> HashMap<String, RelayStatus> {
        let mut statuses = HashMap::new();
        for (relay_url, relay) in self.client.relays().await {
            statuses.insert(relay_url.to_string(), relay.status().await);
        }
        statuses
    }

    async fn has_connected_publish_relay(&self) -> bool {
        let Some(client) = self.publish_client.as_ref() else {
            return false;
        };
        Self::client_has_connected_relay(client).await
    }

    async fn client_has_connected_relay(client: &Client) -> bool {
        for (_relay_url, relay) in client.relays().await {
            if relay.status().await == RelayStatus::Connected {
                return true;
            }
        }
        false
    }

    fn collect_authors(&self) -> Result<Vec<String>> {
        let mut authors = Vec::new();
        let mut seen = HashSet::new();
        for distance in 0..=self.config.max_follow_distance {
            for pubkey in socialgraph::SocialGraphBackend::users_by_follow_distance(
                self.graph_store.as_ref(),
                distance,
            )
            .with_context(|| format!("load social-graph distance {distance}"))?
            {
                if self
                    .graph_store
                    .is_overmuted_user(&pubkey, self.config.overmute_threshold)?
                {
                    continue;
                }
                let hex = hex::encode(pubkey);
                if seen.insert(hex.clone()) {
                    authors.push(hex);
                }
            }
        }
        Ok(authors)
    }

    fn collect_missing_profile_authors(&self, limit: usize) -> Result<Vec<String>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let authors = self.collect_authors()?;
        if authors.is_empty() {
            return Ok(Vec::new());
        }

        let mut cursor = self
            .missing_profile_cursor
            .lock()
            .expect("missing profile cursor");
        let mut index = (*cursor).min(authors.len());
        let mut scanned = 0usize;
        let mut missing = Vec::new();

        while scanned < authors.len() && missing.len() < limit {
            let author = &authors[index];
            if self.graph_store.latest_profile_event(author)?.is_none() {
                missing.push(author.clone());
            }
            index += 1;
            if index == authors.len() {
                index = 0;
            }
            scanned += 1;
        }

        *cursor = index;
        Ok(missing)
    }

    fn should_backfill_missing_profiles(&self, last_run: Option<Instant>) -> bool {
        if self.config.missing_profile_backfill_batch_size == 0
            || !self.config.kinds.contains(&Kind::Metadata.as_u16())
        {
            return false;
        }
        match last_run {
            Some(last_run) => last_run.elapsed() >= MIRROR_MISSING_PROFILE_BACKFILL_INTERVAL,
            None => true,
        }
    }

    fn should_history_sync_on_reconnect(
        history_sync_on_reconnect: bool,
        previous: Option<RelayStatus>,
        status: RelayStatus,
    ) -> bool {
        history_sync_on_reconnect
            && status == RelayStatus::Connected
            && matches!(
                previous,
                Some(
                    RelayStatus::Initialized
                        | RelayStatus::Pending
                        | RelayStatus::Connecting
                        | RelayStatus::Disconnected
                        | RelayStatus::Terminated
                )
            )
    }

    fn should_run_reconnect_history_sync(last_run: Option<&Instant>) -> bool {
        match last_run {
            None => true,
            Some(last_run) => last_run.elapsed() >= MIRROR_RECONNECT_HISTORY_SYNC_COOLDOWN,
        }
    }

    async fn history_sync_authors(&self, authors: Vec<String>) -> Result<()> {
        self.history_sync_authors_with_kinds(authors, &self.config.kinds)
            .await
    }

    async fn history_sync_authors_with_kinds(
        &self,
        authors: Vec<String>,
        kinds: &[u16],
    ) -> Result<()> {
        self.history_sync_authors_chunked(authors, |current_root, author_chunk| async move {
            self.history_sync_author_chunk(current_root, author_chunk, kinds)
                .await
        })
        .await
    }

    async fn history_sync_authors_chunked<F, Fut>(
        &self,
        authors: Vec<String>,
        mut run_chunk: F,
    ) -> Result<()>
    where
        F: FnMut(Option<hashtree_core::Cid>, Vec<String>) -> Fut,
        Fut: std::future::Future<Output = Result<CrawlReport>>,
    {
        if authors.is_empty() {
            return Ok(());
        }

        info!(
            "Nostr mirror history sync starting: authors={} relays={} negentropy_only={}",
            authors.len(),
            self.config.relays.len(),
            self.config.require_negentropy
        );

        let mut current_root = self.graph_store.public_events_root()?;
        let mut last_error = None;
        let mut applied_chunks = 0usize;
        let mut failed_chunks = 0usize;
        let chunk_size = self.config.history_sync_author_chunk_size.max(1);
        let total_chunks = authors.len().div_ceil(chunk_size);

        for (chunk_index, author_chunk) in authors.chunks(chunk_size).enumerate() {
            let author_chunk = author_chunk.to_vec();
            let author_count = author_chunk.len();
            info!(
                "Nostr mirror history sync chunk starting: chunk={}/{} authors={}",
                chunk_index + 1,
                total_chunks,
                author_count
            );
            let report = match run_chunk(current_root.clone(), author_chunk).await {
                Ok(report) => report,
                Err(err) => {
                    failed_chunks = failed_chunks.saturating_add(1);
                    warn!(
                        "Nostr mirror history sync chunk failed: chunk={}/{} authors={} error={:#}",
                        chunk_index + 1,
                        total_chunks,
                        author_count,
                        err
                    );
                    last_error = Some(err);
                    continue;
                }
            };

            if report.root != current_root {
                self.apply_history_root(report.root.as_ref()).await?;
                current_root = report.root.clone();
                info!(
                    "Nostr mirror history sync updated trusted root: chunk={}/{} authors_processed={} events_selected={} events_seen={}",
                    chunk_index + 1,
                    total_chunks,
                    report.authors_processed,
                    report.events_selected,
                    report.events_seen
                );
            }
            applied_chunks = applied_chunks.saturating_add(1);
        }

        if applied_chunks == 0 {
            return Err(last_error
                .unwrap_or_else(|| anyhow::anyhow!("mirror history sync made no progress"))
                .context("run mirror history sync"));
        }
        if failed_chunks > 0 {
            warn!(
                "Nostr mirror history sync completed with skipped chunks: applied_chunks={} failed_chunks={}",
                applied_chunks,
                failed_chunks
            );
        }
        Ok(())
    }

    async fn history_sync_author_chunk(
        &self,
        current_root: Option<hashtree_core::Cid>,
        authors: Vec<String>,
        kinds: &[u16],
    ) -> Result<CrawlReport> {
        let mut last_error = None;
        let mut report = None;
        for attempt in 0..3 {
            let mut last_logged_authors = 0usize;
            let bridge = NostrBridge::new(
                self.store.store_arc(),
                CrawlConfig {
                    relays: self.config.relays.clone(),
                    author_allowlist: Some(authors.clone()),
                    max_live_bytes: None,
                    max_events_seen: None,
                    max_authors: None,
                    max_follow_distance: None,
                    author_batch_size: self.config.author_batch_size.max(1),
                    per_author_event_limit: kinds.len().max(1),
                    per_author_live_bytes: None,
                    fetch_timeout: self.config.fetch_timeout,
                    kinds: Some(kinds.to_vec()),
                    relay_fetch_mode: RelayFetchMode::AuthorBatches,
                    require_negentropy: self.config.require_negentropy,
                    relay_event_max_size: self.config.relay_event_max_size,
                    relay_page_size: 1_000,
                    max_relay_pages: 10,
                },
            );

            match bridge
                .crawl_with_progress(self.graph_store.as_ref(), current_root.as_ref(), |progress| {
                    let log_interval = self.config.author_batch_size.saturating_mul(8).max(2_048);
                    let should_log = progress.authors_processed == progress.authors_considered
                        || progress.authors_processed == 0
                        || progress
                            .authors_processed
                            .saturating_sub(last_logged_authors)
                            >= log_interval;
                    if should_log {
                        last_logged_authors = progress.authors_processed;
                        info!(
                            "Nostr mirror history sync progress: authors_processed={}/{} events_selected={} events_seen={}",
                            progress.authors_processed,
                            progress.authors_considered,
                            progress.events_selected,
                            progress.events_seen
                        );
                    }
                })
                .await
            {
                Ok(next_report) => {
                    report = Some(next_report);
                    break;
                }
                Err(err) => {
                    last_error = Some(err);
                    if attempt < 2 {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        }
        report
            .ok_or_else(|| last_error.expect("history sync retry captured error"))
            .context("run mirror history sync")
    }

    async fn apply_history_root(&self, root: Option<&hashtree_core::Cid>) -> Result<()> {
        self.graph_store.write_public_events_root(root)?;
        let Some(root) = root else {
            return Ok(());
        };

        let event_store = NostrEventStore::new(self.store.store_arc());
        let events = event_store
            .list_recent_lossy(Some(root), ListEventsOptions::default())
            .await
            .context("list trusted mirrored events")?
            .into_iter()
            .map(socialgraph::stored_event_to_nostr_event)
            .collect::<Result<Vec<_>>>()?;

        self.graph_store
            .rebuild_profile_index_for_events(&events)
            .context("rebuild mirrored profile search index")?;
        socialgraph::ingest_graph_parsed_events(self.graph_store.as_ref(), &events)
            .context("sync mirrored social graph state")?;
        self.note_profile_search_root_change()?;
        if let Err(err) = self.maybe_publish_profile_search_root(false).await {
            warn!(
                "Nostr mirror profile-search publish failed after root update: {:#}",
                err
            );
        }
        Ok(())
    }

    async fn subscribe_authors_since(
        &self,
        authors: &[String],
        since: Timestamp,
        subscribed_authors: &mut HashSet<String>,
    ) -> Result<()> {
        let new_authors = authors
            .iter()
            .filter(|author| !subscribed_authors.contains(*author))
            .cloned()
            .collect::<Vec<_>>();
        if new_authors.is_empty() {
            return Ok(());
        }

        for chunk in new_authors.chunks(self.config.author_batch_size.max(1)) {
            let pubkeys = chunk
                .iter()
                .filter_map(|author| PublicKey::from_hex(author).ok())
                .collect::<Vec<_>>();
            if pubkeys.is_empty() {
                continue;
            }

            let filter = Filter::new()
                .authors(pubkeys)
                .kinds(self.config.kinds.iter().copied().map(Kind::from))
                .since(since);

            self.client
                .subscribe(vec![filter], None)
                .await
                .context("subscribe mirror author batch")?;
        }

        subscribed_authors.extend(new_authors);
        Ok(())
    }

    fn ingest_live_event(&self, event: &Event) -> Result<()> {
        socialgraph::ingest_parsed_event(self.graph_store.as_ref(), event)
            .context("ingest live mirrored event")?;
        if event.kind == Kind::Metadata {
            self.note_profile_search_root_change()?;
        }
        Ok(())
    }

    fn note_profile_search_root_change(&self) -> Result<()> {
        let Some(_tree_name) = self.config.published_profile_search_tree_name.as_deref() else {
            return Ok(());
        };

        let root = self.graph_store.profile_search_root()?;
        let mut state = self
            .profile_search_publish_state
            .lock()
            .expect("profile search publish state");
        let now = Instant::now();

        if state.pending_root == root {
            return Ok(());
        }

        state.pending_root = root;
        state.last_changed_at = Some(now);
        if state.dirty_since.is_none() {
            state.dirty_since = Some(now);
        }
        Ok(())
    }

    async fn maybe_publish_profile_search_root(&self, force: bool) -> Result<()> {
        let Some(tree_name) = self.config.published_profile_search_tree_name.as_deref() else {
            return Ok(());
        };
        let Some(publish_client) = self.publish_client.as_ref() else {
            return Ok(());
        };
        if !self.has_connected_publish_relay().await {
            return Ok(());
        }

        let pending_root = {
            let state = self
                .profile_search_publish_state
                .lock()
                .expect("profile search publish state");
            let Some(pending_root) = state.pending_root.clone() else {
                return Ok(());
            };
            if state.last_published_root.as_ref() == Some(&pending_root) {
                return Ok(());
            }

            let now = Instant::now();
            let debounce_ready = state.last_changed_at.is_some_and(|changed_at| {
                now.duration_since(changed_at) >= MIRROR_ROOT_PUBLISH_DEBOUNCE
            });
            let stale_ready = state.dirty_since.is_some_and(|dirty_since| {
                now.duration_since(dirty_since) >= MIRROR_ROOT_PUBLISH_MAX_STALENESS
            });
            if !force && !debounce_ready && !stale_ready {
                return Ok(());
            }

            pending_root
        };

        let event = Self::build_public_root_event(tree_name, &pending_root);
        let output = publish_client
            .send_event_builder(event)
            .await
            .context("publish profile search root event")?;
        if output.failed.is_empty() && output.success.is_empty() {
            return Ok(());
        }

        {
            let mut state = self
                .profile_search_publish_state
                .lock()
                .expect("profile search publish state");
            if state.pending_root.as_ref() == Some(&pending_root) {
                state.last_published_root = Some(pending_root.clone());
                state.last_published_at = Some(Instant::now());
                state.dirty_since = None;
            }
        }

        info!(
            "Nostr mirror published profile search root: tree={} hash={}",
            tree_name,
            hex::encode(pending_root.hash)
        );
        Ok(())
    }

    fn build_public_root_event(tree_name: &str, cid: &hashtree_core::Cid) -> EventBuilder {
        let mut tags = vec![
            Tag::identifier(tree_name.to_string()),
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::L)),
                vec!["hashtree"],
            ),
            Tag::custom(TagKind::Custom("hash".into()), vec![hex::encode(cid.hash)]),
        ];
        if let Some(key) = cid.key {
            tags.push(Tag::custom(
                TagKind::Custom("key".into()),
                vec![hex::encode(key)],
            ));
        }

        EventBuilder::new(Kind::Custom(30078), "", tags)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use hashtree_resolver::RootResolver;
    use nostr::{EventBuilder, JsonUtil, Tag};
    use nostr_sdk::ToBech32;
    use std::collections::HashMap;
    use std::net::TcpListener;
    use std::sync::Mutex;
    use tempfile::TempDir;
    use tokio::net::TcpStream;
    use tokio::sync::broadcast;
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    use crate::socialgraph::{open_social_graph_store_with_storage, set_social_graph_root};

    struct TestRelay {
        url: String,
        shutdown: broadcast::Sender<()>,
        events: Arc<Mutex<Vec<Event>>>,
        broadcaster: broadcast::Sender<Event>,
        request_count: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl TestRelay {
        fn new(events: Vec<Event>) -> Self {
            let events = Arc::new(Mutex::new(events));
            let (shutdown, _) = broadcast::channel(1);
            let (broadcaster, _) = broadcast::channel(32);
            let request_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

            let std_listener = TcpListener::bind("127.0.0.1:0").expect("bind relay");
            let port = std_listener.local_addr().expect("relay addr").port();
            std_listener
                .set_nonblocking(true)
                .expect("listener nonblocking");

            let events_for_thread = Arc::clone(&events);
            let shutdown_for_thread = shutdown.clone();
            let broadcaster_for_thread = broadcaster.clone();
            let request_count_for_thread = Arc::clone(&request_count);
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("runtime");
                runtime.block_on(async move {
                    let listener =
                        tokio::net::TcpListener::from_std(std_listener).expect("tokio listener");
                    let mut shutdown_rx = shutdown_for_thread.subscribe();

                    loop {
                        tokio::select! {
                            _ = shutdown_rx.recv() => break,
                            accept = listener.accept() => {
                                if let Ok((stream, _)) = accept {
                                    let events = Arc::clone(&events_for_thread);
                                    let broadcaster = broadcaster_for_thread.clone();
                                    let request_count = Arc::clone(&request_count_for_thread);
                                    tokio::spawn(async move {
                                        handle_connection(stream, events, broadcaster, request_count)
                                            .await;
                                    });
                                }
                            }
                        }
                    }
                });
            });

            std::thread::sleep(Duration::from_millis(100));

            Self {
                url: format!("ws://127.0.0.1:{port}"),
                shutdown,
                events,
                broadcaster,
                request_count,
            }
        }

        fn url(&self) -> String {
            self.url.clone()
        }

        fn publish(&self, event: Event) {
            self.events
                .lock()
                .expect("relay events")
                .push(event.clone());
            let _ = self.broadcaster.send(event);
        }

        fn request_count(&self) -> usize {
            self.request_count
                .load(std::sync::atomic::Ordering::Relaxed)
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
        events: Arc<Mutex<Vec<Event>>>,
        broadcaster: broadcast::Sender<Event>,
        request_count: Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let ws = match accept_async(stream).await {
            Ok(ws) => ws,
            Err(_) => return,
        };
        let (mut write, mut read) = ws.split();
        let mut subscriptions = HashMap::<String, Vec<nostr::Filter>>::new();
        let mut event_rx = broadcaster.subscribe();
        loop {
            tokio::select! {
                maybe_message = read.next() => {
                    let Some(message) = maybe_message else {
                        break;
                    };
                    let text = match message {
                        Ok(Message::Text(text)) => text,
                        Ok(Message::Ping(data)) => {
                            let _ = write.send(Message::Pong(data)).await;
                            continue;
                        }
                        Ok(Message::Close(_)) => break,
                        _ => continue,
                    };

                    let parsed = match nostr::ClientMessage::from_json(text.as_bytes()) {
                        Ok(message) => message,
                        Err(_) => continue,
                    };

                    match parsed {
                        nostr::ClientMessage::Req {
                            subscription_id,
                            filters,
                        } => {
                            request_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            subscriptions.insert(subscription_id.to_string(), filters.clone());
                            let current = events.lock().expect("relay events").clone();
                            for event in current {
                                if filters.iter().any(|filter| filter.match_event(&event)) {
                                    let _ = write
                                        .send(Message::Text(
                                            nostr::RelayMessage::event(subscription_id.clone(), event)
                                                .as_json(),
                                        ))
                                        .await;
                                }
                            }
                            let _ = write
                                .send(Message::Text(
                                    nostr::RelayMessage::eose(subscription_id).as_json(),
                                ))
                                .await;
                        }
                        nostr::ClientMessage::Close(subscription_id) => {
                            subscriptions.remove(&subscription_id.to_string());
                            let _ = write
                                .send(Message::Text(
                                    nostr::RelayMessage::closed(subscription_id, "").as_json(),
                                ))
                                .await;
                        }
                        nostr::ClientMessage::Event(event) => {
                            let event = *event;
                            events.lock().expect("relay events").push(event.clone());
                            let _ = broadcaster.send(event.clone());
                            let _ = write
                                .send(Message::Text(
                                    nostr::RelayMessage::ok(event.id, true, "").as_json(),
                                ))
                                .await;
                        }
                        _ => {}
                    }
                }
                Ok(event) = event_rx.recv() => {
                    for (subscription_id, filters) in &subscriptions {
                        if filters.iter().any(|filter| filter.match_event(&event)) {
                            let _ = write
                                .send(Message::Text(
                                    nostr::RelayMessage::event(
                                        nostr::SubscriptionId::new(subscription_id.clone()),
                                        event.clone(),
                                    )
                                    .as_json(),
                                ))
                                .await;
                        }
                    }
                }
            }
        }
    }

    async fn wait_until<F>(label: &str, timeout: Duration, mut condition: F)
    where
        F: FnMut() -> bool,
    {
        let started = std::time::Instant::now();
        while started.elapsed() < timeout {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("{label}: condition not met within {:?}", timeout);
    }

    #[tokio::test]
    async fn apply_history_root_updates_profile_index() -> Result<()> {
        let _guard = crate::socialgraph::test_lock();
        let tmp = TempDir::new().expect("tempdir");
        let store = Arc::new(HashtreeStore::new(tmp.path())?);
        let graph_store = open_social_graph_store_with_storage(
            tmp.path(),
            store.store_arc(),
            Some(64 * 1024 * 1024),
        )?;

        let root_keys = nostr::Keys::generate();
        let root_pubkey = root_keys.public_key().to_bytes();
        set_social_graph_root(&graph_store, &root_pubkey);

        let alice_keys = nostr::Keys::generate();
        let root_contacts = EventBuilder::new(
            Kind::ContactList,
            "",
            vec![Tag::public_key(alice_keys.public_key())],
        )
        .custom_created_at(Timestamp::from(10))
        .to_event(&root_keys)
        .expect("root contacts");
        socialgraph::ingest_parsed_event(graph_store.as_ref(), &root_contacts)?;

        let alice_profile = EventBuilder::new(Kind::Metadata, r#"{"name":"Alice Mirror"}"#, [])
            .custom_created_at(Timestamp::from(11))
            .to_event(&alice_keys)
            .expect("alice profile");
        let stored = hashtree_nostr::StoredNostrEvent {
            id: alice_profile.id.to_hex(),
            pubkey: alice_profile.pubkey.to_hex(),
            created_at: alice_profile.created_at.as_u64(),
            kind: alice_profile.kind.as_u16() as u32,
            tags: alice_profile
                .tags
                .iter()
                .map(|tag| tag.as_slice().to_vec())
                .collect(),
            content: alice_profile.content.clone(),
            sig: alice_profile.sig.to_string(),
        };
        let event_store = NostrEventStore::new(store.store_arc());
        let root = event_store.build(None, vec![stored]).await?;
        let mirror = BackgroundNostrMirror::new(
            NostrMirrorConfig::default(),
            store,
            graph_store.clone(),
            None,
        )
        .await?;
        mirror.apply_history_root(root.as_ref()).await?;

        let alice_hex = alice_keys.public_key().to_hex();
        assert!(graph_store.latest_profile_event(&alice_hex)?.is_some());
        assert!(graph_store.profile_search_root()?.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn apply_history_root_publishes_profile_search_tree() -> Result<()> {
        let _guard = crate::socialgraph::test_lock();
        let tmp = TempDir::new().expect("tempdir");
        let store = Arc::new(HashtreeStore::new(tmp.path())?);
        let graph_store = open_social_graph_store_with_storage(
            tmp.path(),
            store.store_arc(),
            Some(64 * 1024 * 1024),
        )?;

        let root_keys = nostr::Keys::generate();
        let root_pubkey = root_keys.public_key().to_bytes();
        set_social_graph_root(&graph_store, &root_pubkey);

        let alice_keys = nostr::Keys::generate();
        let alice_profile =
            EventBuilder::new(Kind::Metadata, r#"{"name":"Alice Published Search"}"#, [])
                .custom_created_at(Timestamp::from(11))
                .to_event(&alice_keys)
                .expect("alice profile");
        let stored = hashtree_nostr::StoredNostrEvent {
            id: alice_profile.id.to_hex(),
            pubkey: alice_profile.pubkey.to_hex(),
            created_at: alice_profile.created_at.as_u64(),
            kind: alice_profile.kind.as_u16() as u32,
            tags: alice_profile
                .tags
                .iter()
                .map(|tag| tag.as_slice().to_vec())
                .collect(),
            content: alice_profile.content.clone(),
            sig: alice_profile.sig.to_string(),
        };
        let event_store = NostrEventStore::new(store.store_arc());
        let root = event_store.build(None, vec![stored]).await?;

        let relay = TestRelay::new(Vec::new());
        let publish_keys = nostr_sdk::Keys::parse(&root_keys.secret_key().to_bech32()?)
            .context("parse mirror publish keys")?;
        let mirror = BackgroundNostrMirror::new(
            NostrMirrorConfig {
                relays: vec![relay.url()],
                publish_relays: vec![relay.url()],
                history_sync_on_start: false,
                published_profile_search_tree_name: Some("profile-search".to_string()),
                ..NostrMirrorConfig::default()
            },
            store,
            graph_store.clone(),
            Some(publish_keys),
        )
        .await?;

        let connected_started = std::time::Instant::now();
        while connected_started.elapsed() < Duration::from_secs(5) {
            if mirror.has_connected_publish_relay().await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            mirror.has_connected_publish_relay().await,
            "publisher relay should connect"
        );
        mirror.apply_history_root(root.as_ref()).await?;
        tokio::time::sleep(MIRROR_ROOT_PUBLISH_DEBOUNCE + Duration::from_millis(20)).await;
        mirror.maybe_publish_profile_search_root(false).await?;

        let resolver = crate::NostrRootResolver::new(crate::NostrResolverConfig {
            relays: vec![relay.url()],
            resolve_timeout: Duration::from_secs(2),
            secret_key: None,
        })
        .await?;
        let npub = root_keys.public_key().to_bech32()?;
        let resolved = resolver
            .resolve(&format!("{npub}/profile-search"))
            .await?
            .expect("published profile-search root");

        assert_eq!(
            resolved,
            graph_store.profile_search_root()?.expect("search root")
        );
        resolver.stop().await?;
        Ok(())
    }

    #[tokio::test]
    async fn startup_publish_sends_existing_profile_search_root() -> Result<()> {
        let _guard = crate::socialgraph::test_lock();
        let tmp = TempDir::new().expect("tempdir");
        let store = Arc::new(HashtreeStore::new(tmp.path())?);
        let graph_store = open_social_graph_store_with_storage(
            tmp.path(),
            store.store_arc(),
            Some(64 * 1024 * 1024),
        )?;

        let root_keys = nostr::Keys::generate();
        let root_pubkey = root_keys.public_key().to_bytes();
        set_social_graph_root(&graph_store, &root_pubkey);

        let alice_keys = nostr::Keys::generate();
        let alice_profile =
            EventBuilder::new(Kind::Metadata, r#"{"name":"Alice Existing Search"}"#, [])
                .custom_created_at(Timestamp::from(11))
                .to_event(&alice_keys)
                .expect("alice profile");
        let stored = hashtree_nostr::StoredNostrEvent {
            id: alice_profile.id.to_hex(),
            pubkey: alice_profile.pubkey.to_hex(),
            created_at: alice_profile.created_at.as_u64(),
            kind: alice_profile.kind.as_u16() as u32,
            tags: alice_profile
                .tags
                .iter()
                .map(|tag| tag.as_slice().to_vec())
                .collect(),
            content: alice_profile.content.clone(),
            sig: alice_profile.sig.to_string(),
        };
        let event_store = NostrEventStore::new(store.store_arc());
        let root = event_store.build(None, vec![stored]).await?;
        graph_store.write_public_events_root(root.as_ref())?;
        graph_store.rebuild_profile_index_for_events(&[alice_profile.clone()])?;

        let relay = TestRelay::new(Vec::new());
        let publish_keys = nostr_sdk::Keys::parse(&root_keys.secret_key().to_bech32()?)
            .context("parse mirror publish keys")?;
        let mirror = BackgroundNostrMirror::new(
            NostrMirrorConfig {
                relays: vec![relay.url()],
                publish_relays: vec![relay.url()],
                history_sync_on_start: false,
                published_profile_search_tree_name: Some("profile-search".to_string()),
                ..NostrMirrorConfig::default()
            },
            store,
            graph_store.clone(),
            Some(publish_keys),
        )
        .await?;

        let connected_started = std::time::Instant::now();
        while connected_started.elapsed() < Duration::from_secs(5) {
            if mirror.has_connected_publish_relay().await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            mirror.has_connected_publish_relay().await,
            "publisher relay should connect"
        );

        mirror.note_profile_search_root_change()?;
        tokio::time::sleep(MIRROR_ROOT_PUBLISH_DEBOUNCE + Duration::from_millis(20)).await;
        mirror.maybe_publish_profile_search_root(false).await?;

        let resolver = crate::NostrRootResolver::new(crate::NostrResolverConfig {
            relays: vec![relay.url()],
            resolve_timeout: Duration::from_secs(2),
            secret_key: None,
        })
        .await?;
        let npub = root_keys.public_key().to_bech32()?;
        let resolved = resolver
            .resolve(&format!("{npub}/profile-search"))
            .await?
            .expect("published profile-search root");

        assert_eq!(
            resolved,
            graph_store.profile_search_root()?.expect("search root")
        );
        resolver.stop().await?;
        Ok(())
    }

    #[tokio::test]
    async fn history_sync_checkpoints_root_before_later_chunk_failure() -> Result<()> {
        let _guard = crate::socialgraph::test_lock();
        let tmp = TempDir::new().expect("tempdir");
        let store = Arc::new(HashtreeStore::new(tmp.path())?);
        let graph_store = open_social_graph_store_with_storage(
            tmp.path(),
            store.store_arc(),
            Some(64 * 1024 * 1024),
        )?;

        let root_keys = nostr::Keys::generate();
        let root_pubkey = root_keys.public_key().to_bytes();
        set_social_graph_root(&graph_store, &root_pubkey);

        let alice_keys = nostr::Keys::generate();
        let alice_profile = EventBuilder::new(Kind::Metadata, r#"{"name":"Alice Checkpoint"}"#, [])
            .custom_created_at(Timestamp::from(11))
            .to_event(&alice_keys)
            .expect("alice profile");
        let alice_stored = hashtree_nostr::StoredNostrEvent {
            id: alice_profile.id.to_hex(),
            pubkey: alice_profile.pubkey.to_hex(),
            created_at: alice_profile.created_at.as_u64(),
            kind: alice_profile.kind.as_u16() as u32,
            tags: alice_profile
                .tags
                .iter()
                .map(|tag| tag.as_slice().to_vec())
                .collect(),
            content: alice_profile.content.clone(),
            sig: alice_profile.sig.to_string(),
        };
        let event_store = NostrEventStore::new(store.store_arc());
        let root = event_store.build(None, vec![alice_stored]).await?;
        let mirror = BackgroundNostrMirror::new(
            NostrMirrorConfig::default(),
            store,
            graph_store.clone(),
            None,
        )
        .await?;
        let call_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        mirror
            .history_sync_authors_chunked(vec!["author-a".to_string(), "author-b".to_string()], {
                let call_index = Arc::clone(&call_index);
                let root = root.clone();
                move |_current_root, author_chunk| {
                    let call_index = Arc::clone(&call_index);
                    let root = root.clone();
                    std::future::ready(
                        match call_index.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                            0 => Ok(CrawlReport {
                                root: root.clone(),
                                authors_considered: 2,
                                authors_processed: author_chunk.len(),
                                events_seen: 1,
                                events_selected: 1,
                                live_bytes_selected: 0,
                            }),
                            _ => Err(anyhow::anyhow!("boom")),
                        },
                    )
                }
            })
            .await?;

        let alice_hex = alice_keys.public_key().to_hex();
        assert!(graph_store.latest_profile_event(&alice_hex)?.is_some());
        assert_eq!(
            graph_store.public_events_root()?,
            root,
            "expected first successful chunk to checkpoint trusted root"
        );
        Ok(())
    }

    #[tokio::test]
    async fn mirror_history_sync_accepts_large_contact_list_events() -> Result<()> {
        let _guard = crate::socialgraph::test_lock();
        let tmp = TempDir::new().expect("tempdir");
        let store = Arc::new(HashtreeStore::new(tmp.path())?);
        let graph_store = open_social_graph_store_with_storage(
            tmp.path(),
            store.store_arc(),
            Some(64 * 1024 * 1024),
        )?;

        let root_keys = nostr::Keys::generate();
        let root_pubkey = root_keys.public_key().to_bytes();
        set_social_graph_root(&graph_store, &root_pubkey);

        let followed_keys = (0..1_600)
            .map(|_| nostr::Keys::generate())
            .collect::<Vec<_>>();
        let tags = followed_keys
            .iter()
            .map(|keys| Tag::public_key(keys.public_key()))
            .collect::<Vec<_>>();
        let root_contacts = EventBuilder::new(Kind::ContactList, "", tags)
            .custom_created_at(Timestamp::from(10))
            .to_event(&root_keys)
            .expect("root contacts");
        assert!(
            root_contacts.as_json().len() > 70_000,
            "test event should exceed nostr-sdk default size limit"
        );

        let relay = TestRelay::new(vec![root_contacts]);
        let mirror = BackgroundNostrMirror::new(
            NostrMirrorConfig {
                relays: vec![relay.url()],
                max_follow_distance: 1,
                kinds: vec![3],
                history_sync_on_start: false,
                ..NostrMirrorConfig::default()
            },
            store,
            graph_store.clone(),
            None,
        )
        .await?;

        mirror
            .history_sync_authors(vec![root_keys.public_key().to_hex()])
            .await?;

        let follows = crate::socialgraph::get_follows(&graph_store, &root_pubkey);
        let first_pk = followed_keys
            .first()
            .expect("first followed key")
            .public_key()
            .to_bytes();
        let last_pk = followed_keys
            .last()
            .expect("last followed key")
            .public_key()
            .to_bytes();
        assert!(
            follows.contains(&first_pk) && follows.contains(&last_pk),
            "expected history sync to ingest oversized contact list event"
        );
        Ok(())
    }

    #[tokio::test]
    async fn mirror_collect_authors_skips_overmuted_users() -> Result<()> {
        let _guard = crate::socialgraph::test_lock();
        let tmp = TempDir::new().expect("tempdir");
        let store = Arc::new(HashtreeStore::new(tmp.path())?);
        let graph_store = open_social_graph_store_with_storage(
            tmp.path(),
            store.store_arc(),
            Some(64 * 1024 * 1024),
        )?;

        let root_keys = nostr::Keys::generate();
        let target_keys = nostr::Keys::generate();
        set_social_graph_root(&graph_store, &root_keys.public_key().to_bytes());

        let follow = EventBuilder::new(
            Kind::ContactList,
            "",
            vec![Tag::public_key(target_keys.public_key())],
        )
        .custom_created_at(Timestamp::from_secs(10))
        .to_event(&root_keys)
        .expect("follow");
        crate::socialgraph::ingest_parsed_event(&graph_store, &follow)?;

        let mute = EventBuilder::new(
            Kind::MuteList,
            "",
            vec![Tag::public_key(target_keys.public_key())],
        )
        .custom_created_at(Timestamp::from_secs(11))
        .to_event(&root_keys)
        .expect("mute");
        crate::socialgraph::ingest_parsed_event(&graph_store, &mute)?;

        let mirror = BackgroundNostrMirror::new(
            NostrMirrorConfig {
                max_follow_distance: 1,
                overmute_threshold: 1.0,
                ..NostrMirrorConfig::default()
            },
            store,
            graph_store.clone(),
            None,
        )
        .await?;

        let authors = mirror.collect_authors()?;
        assert!(authors.contains(&root_keys.public_key().to_hex()));
        assert!(!authors.contains(&target_keys.public_key().to_hex()));
        Ok(())
    }

    #[tokio::test]
    async fn mirror_collect_missing_profile_authors_skips_existing_profiles() -> Result<()> {
        let _guard = crate::socialgraph::test_lock();
        let tmp = TempDir::new().expect("tempdir");
        let store = Arc::new(HashtreeStore::new(tmp.path())?);
        let graph_store = open_social_graph_store_with_storage(
            tmp.path(),
            store.store_arc(),
            Some(64 * 1024 * 1024),
        )?;

        let root_keys = nostr::Keys::generate();
        let existing_keys = nostr::Keys::generate();
        let missing_keys = nostr::Keys::generate();
        set_social_graph_root(&graph_store, &root_keys.public_key().to_bytes());

        let root_profile = EventBuilder::new(Kind::Metadata, r#"{"name":"root"}"#, [])
            .custom_created_at(Timestamp::from_secs(5))
            .to_event(&root_keys)
            .expect("root profile");
        crate::socialgraph::ingest_parsed_event(&graph_store, &root_profile)?;

        let follow = EventBuilder::new(
            Kind::ContactList,
            "",
            vec![
                Tag::public_key(existing_keys.public_key()),
                Tag::public_key(missing_keys.public_key()),
            ],
        )
        .custom_created_at(Timestamp::from_secs(10))
        .to_event(&root_keys)
        .expect("follow");
        crate::socialgraph::ingest_parsed_event(&graph_store, &follow)?;

        let existing_profile = EventBuilder::new(Kind::Metadata, r#"{"name":"existing"}"#, [])
            .custom_created_at(Timestamp::from_secs(11))
            .to_event(&existing_keys)
            .expect("existing profile");
        crate::socialgraph::ingest_parsed_event(&graph_store, &existing_profile)?;

        let mirror = BackgroundNostrMirror::new(
            NostrMirrorConfig {
                max_follow_distance: 1,
                kinds: vec![0, 3],
                history_sync_on_start: false,
                ..NostrMirrorConfig::default()
            },
            store,
            graph_store.clone(),
            None,
        )
        .await?;

        let authors = mirror.collect_missing_profile_authors(10)?;
        assert!(authors.contains(&missing_keys.public_key().to_hex()));
        assert!(!authors.contains(&existing_keys.public_key().to_hex()));
        assert!(!authors.contains(&root_keys.public_key().to_hex()));
        Ok(())
    }

    #[tokio::test]
    async fn mirror_live_ingest_updates_profile_index() -> Result<()> {
        let _guard = crate::socialgraph::test_lock();
        let tmp = TempDir::new().expect("tempdir");
        let store = Arc::new(HashtreeStore::new(tmp.path())?);
        let graph_store = open_social_graph_store_with_storage(
            tmp.path(),
            store.store_arc(),
            Some(64 * 1024 * 1024),
        )?;

        let root_keys = nostr::Keys::generate();
        let root_pubkey = root_keys.public_key().to_bytes();
        set_social_graph_root(&graph_store, &root_pubkey);

        let alice_keys = nostr::Keys::generate();
        let root_contacts = EventBuilder::new(
            Kind::ContactList,
            "",
            vec![Tag::public_key(alice_keys.public_key())],
        )
        .custom_created_at(Timestamp::from(10))
        .to_event(&root_keys)
        .expect("root contacts");
        socialgraph::ingest_parsed_event(graph_store.as_ref(), &root_contacts)?;

        let relay = TestRelay::new(Vec::new());
        let mirror = Arc::new(
            BackgroundNostrMirror::new(
                NostrMirrorConfig {
                    relays: vec![relay.url()],
                    max_follow_distance: 1,
                    author_batch_size: 32,
                    history_sync_on_start: false,
                    missing_profile_backfill_batch_size: 0,
                    ..NostrMirrorConfig::default()
                },
                store,
                graph_store.clone(),
                None,
            )
            .await?,
        );

        let mirror_task = {
            let mirror = Arc::clone(&mirror);
            tokio::task::spawn_blocking(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build test mirror runtime");
                runtime.block_on(async { mirror.run().await })
            })
        };

        wait_until("subscription", Duration::from_secs(5), || {
            relay.request_count() > 0
        })
        .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let updated_profile =
            EventBuilder::new(Kind::Metadata, r#"{"name":"Alice Mirror Updated"}"#, [])
                .to_event(&alice_keys)
                .expect("updated profile");
        relay.publish(updated_profile);

        let alice_hex = alice_keys.public_key().to_hex();
        wait_until("live profile update", Duration::from_secs(5), || {
            graph_store
                .latest_profile_event(&alice_hex)
                .ok()
                .flatten()
                .is_some_and(|event| event.content.contains("Updated"))
        })
        .await;

        mirror.shutdown();
        mirror_task.await.expect("mirror join")?;
        Ok(())
    }

    #[test]
    fn relay_connected_after_disconnect_triggers_reconnect_history_sync() {
        assert!(BackgroundNostrMirror::should_history_sync_on_reconnect(
            true,
            Some(RelayStatus::Disconnected),
            RelayStatus::Connected
        ));
        assert!(!BackgroundNostrMirror::should_history_sync_on_reconnect(
            true,
            Some(RelayStatus::Connected),
            RelayStatus::Connected
        ));
        assert!(!BackgroundNostrMirror::should_history_sync_on_reconnect(
            true,
            None,
            RelayStatus::Connected
        ));
        assert!(!BackgroundNostrMirror::should_history_sync_on_reconnect(
            false,
            Some(RelayStatus::Disconnected),
            RelayStatus::Connected
        ));
    }

    #[test]
    fn reconnect_history_sync_respects_cooldown() {
        let now = Instant::now();
        assert!(BackgroundNostrMirror::should_run_reconnect_history_sync(
            None
        ));
        assert!(!BackgroundNostrMirror::should_run_reconnect_history_sync(
            Some(&now)
        ));
    }
}
