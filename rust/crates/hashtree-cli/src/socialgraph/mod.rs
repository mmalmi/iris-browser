pub mod access;
pub mod crawler;
pub mod local_lists;
pub mod snapshot;

pub use access::SocialGraphAccessControl;
pub use crawler::SocialGraphCrawler;
pub use local_lists::{
    read_local_list_file_state, sync_local_list_files_force, sync_local_list_files_if_changed,
    LocalListFileState, LocalListSyncOutcome,
};

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{Context, Result};
use bytes::Bytes;
use futures::executor::block_on;
use hashtree_core::{nhash_encode_full, Cid, HashTree, HashTreeConfig, NHashData};
use hashtree_index::BTree;
use hashtree_nostr::{
    is_parameterized_replaceable_kind, is_replaceable_kind, ListEventsOptions, NostrEventStore,
    NostrEventStoreError, ProfileGuard as NostrProfileGuard, StoredNostrEvent,
};
#[cfg(test)]
use hashtree_nostr::{
    reset_profile as reset_nostr_profile, set_profile_enabled as set_nostr_profile_enabled,
    take_profile as take_nostr_profile,
};
use nostr::{Event, Filter, JsonUtil, Kind, SingleLetterTag};
use nostr_social_graph::{
    BinaryBudget, GraphStats, NostrEvent as GraphEvent, SocialGraph,
    SocialGraphBackend as NostrSocialGraphBackend,
};
use nostr_social_graph_heed::HeedSocialGraph;

use crate::storage::{LocalStore, StorageRouter};

#[cfg(test)]
use std::sync::{Mutex, MutexGuard, OnceLock};
#[cfg(test)]
use std::time::Instant;

pub type UserSet = BTreeSet<[u8; 32]>;

const DEFAULT_ROOT_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000000";
const EVENTS_ROOT_FILE: &str = "events-root.msgpack";
const AMBIENT_EVENTS_ROOT_FILE: &str = "events-root-ambient.msgpack";
const AMBIENT_EVENTS_BLOB_DIR: &str = "ambient-blobs";
const PROFILE_SEARCH_ROOT_FILE: &str = "profile-search-root.msgpack";
const PROFILES_BY_PUBKEY_ROOT_FILE: &str = "profiles-by-pubkey-root.msgpack";
const UNKNOWN_FOLLOW_DISTANCE: u32 = 1000;
const DEFAULT_SOCIALGRAPH_MAP_SIZE_BYTES: u64 = 64 * 1024 * 1024;
const SOCIALGRAPH_MAX_DBS: u32 = 16;
const PROFILE_SEARCH_INDEX_ORDER: usize = 64;
const PROFILE_SEARCH_PREFIX: &str = "p:";
const PROFILE_NAME_MAX_LENGTH: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventStorageClass {
    Public,
    Ambient,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventQueryScope {
    PublicOnly,
    AmbientOnly,
    All,
}

struct EventIndexBucket {
    event_store: NostrEventStore<StorageRouter>,
    root_path: PathBuf,
}

struct ProfileIndexBucket {
    tree: HashTree<StorageRouter>,
    index: BTree<StorageRouter>,
    by_pubkey_root_path: PathBuf,
    search_root_path: PathBuf,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StoredCid {
    hash: [u8; 32],
    key: Option<[u8; 32]>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredProfileSearchEntry {
    pub pubkey: String,
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub nip05: Option<String>,
    pub created_at: u64,
    pub event_nhash: String,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SocialGraphStats {
    pub total_users: usize,
    pub root: Option<String>,
    pub total_follows: usize,
    pub max_depth: u32,
    pub size_by_distance: BTreeMap<u32, usize>,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
struct DistanceCache {
    stats: SocialGraphStats,
    users_by_distance: BTreeMap<u32, Vec<[u8; 32]>>,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct UpstreamGraphBackendError(String);

pub struct SocialGraphStore {
    graph: StdMutex<HeedSocialGraph>,
    distance_cache: StdMutex<Option<DistanceCache>>,
    public_events: EventIndexBucket,
    ambient_events: EventIndexBucket,
    profile_index: ProfileIndexBucket,
    profile_index_overmute_threshold: StdMutex<f64>,
}

pub trait SocialGraphBackend: Send + Sync {
    fn stats(&self) -> Result<SocialGraphStats>;
    fn users_by_follow_distance(&self, distance: u32) -> Result<Vec<[u8; 32]>>;
    fn follow_distance(&self, pk_bytes: &[u8; 32]) -> Result<Option<u32>>;
    fn follow_list_created_at(&self, owner: &[u8; 32]) -> Result<Option<u64>>;
    fn followed_targets(&self, owner: &[u8; 32]) -> Result<UserSet>;
    fn is_overmuted_user(&self, user_pk: &[u8; 32], threshold: f64) -> Result<bool>;
    fn profile_search_root(&self) -> Result<Option<Cid>> {
        Ok(None)
    }
    fn snapshot_chunks(&self, root: &[u8; 32], options: &BinaryBudget) -> Result<Vec<Bytes>>;
    fn ingest_event(&self, event: &Event) -> Result<()>;
    fn ingest_event_with_storage_class(
        &self,
        event: &Event,
        storage_class: EventStorageClass,
    ) -> Result<()> {
        let _ = storage_class;
        self.ingest_event(event)
    }
    fn ingest_events(&self, events: &[Event]) -> Result<()> {
        for event in events {
            self.ingest_event(event)?;
        }
        Ok(())
    }
    fn ingest_events_with_storage_class(
        &self,
        events: &[Event],
        storage_class: EventStorageClass,
    ) -> Result<()> {
        for event in events {
            self.ingest_event_with_storage_class(event, storage_class)?;
        }
        Ok(())
    }
    fn ingest_graph_events(&self, events: &[Event]) -> Result<()> {
        self.ingest_events(events)
    }
    fn query_events(&self, filter: &Filter, limit: usize) -> Result<Vec<Event>>;
}

#[cfg(test)]
pub type TestLockGuard = MutexGuard<'static, ()>;

#[cfg(test)]
static NDB_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[cfg(test)]
pub fn test_lock() -> TestLockGuard {
    NDB_TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
}

pub fn open_social_graph_store(data_dir: &Path) -> Result<Arc<SocialGraphStore>> {
    open_social_graph_store_with_mapsize(data_dir, None)
}

pub fn open_social_graph_store_with_mapsize(
    data_dir: &Path,
    mapsize_bytes: Option<u64>,
) -> Result<Arc<SocialGraphStore>> {
    let db_dir = data_dir.join("socialgraph");
    open_social_graph_store_at_path(&db_dir, mapsize_bytes)
}

pub fn open_social_graph_store_with_storage(
    data_dir: &Path,
    store: Arc<StorageRouter>,
    mapsize_bytes: Option<u64>,
) -> Result<Arc<SocialGraphStore>> {
    let db_dir = data_dir.join("socialgraph");
    open_social_graph_store_at_path_with_storage(&db_dir, store, mapsize_bytes)
}

pub fn open_social_graph_store_at_path(
    db_dir: &Path,
    mapsize_bytes: Option<u64>,
) -> Result<Arc<SocialGraphStore>> {
    let config = hashtree_config::Config::load_or_default();
    let backend = &config.storage.backend;
    let local_store = Arc::new(
        LocalStore::new_with_lmdb_map_size(db_dir.join("blobs"), backend, mapsize_bytes)
            .map_err(|err| anyhow::anyhow!("Failed to create social graph blob store: {err}"))?,
    );
    let store = Arc::new(StorageRouter::new(local_store));
    open_social_graph_store_at_path_with_storage(db_dir, store, mapsize_bytes)
}

pub fn open_social_graph_store_at_path_with_storage(
    db_dir: &Path,
    store: Arc<StorageRouter>,
    mapsize_bytes: Option<u64>,
) -> Result<Arc<SocialGraphStore>> {
    let ambient_backend = store.local_store().backend();
    let ambient_local = Arc::new(
        LocalStore::new_with_lmdb_map_size(
            db_dir.join(AMBIENT_EVENTS_BLOB_DIR),
            &ambient_backend,
            mapsize_bytes,
        )
        .map_err(|err| {
            anyhow::anyhow!("Failed to create social graph ambient blob store: {err}")
        })?,
    );
    let ambient_store = Arc::new(StorageRouter::new(ambient_local));
    open_social_graph_store_at_path_with_storage_split(db_dir, store, ambient_store, mapsize_bytes)
}

pub fn open_social_graph_store_at_path_with_storage_split(
    db_dir: &Path,
    public_store: Arc<StorageRouter>,
    ambient_store: Arc<StorageRouter>,
    mapsize_bytes: Option<u64>,
) -> Result<Arc<SocialGraphStore>> {
    std::fs::create_dir_all(db_dir)?;
    if let Some(size) = mapsize_bytes {
        ensure_social_graph_mapsize(db_dir, size)?;
    }
    let graph = HeedSocialGraph::open(db_dir, DEFAULT_ROOT_HEX)
        .context("open nostr-social-graph heed backend")?;

    Ok(Arc::new(SocialGraphStore {
        graph: StdMutex::new(graph),
        distance_cache: StdMutex::new(None),
        public_events: EventIndexBucket {
            event_store: NostrEventStore::new(Arc::clone(&public_store)),
            root_path: db_dir.join(EVENTS_ROOT_FILE),
        },
        ambient_events: EventIndexBucket {
            event_store: NostrEventStore::new(ambient_store),
            root_path: db_dir.join(AMBIENT_EVENTS_ROOT_FILE),
        },
        profile_index: ProfileIndexBucket {
            tree: HashTree::new(HashTreeConfig::new(Arc::clone(&public_store))),
            index: BTree::new(
                public_store,
                hashtree_index::BTreeOptions {
                    order: Some(PROFILE_SEARCH_INDEX_ORDER),
                },
            ),
            by_pubkey_root_path: db_dir.join(PROFILES_BY_PUBKEY_ROOT_FILE),
            search_root_path: db_dir.join(PROFILE_SEARCH_ROOT_FILE),
        },
        profile_index_overmute_threshold: StdMutex::new(1.0),
    }))
}

pub fn set_social_graph_root(store: &SocialGraphStore, pk_bytes: &[u8; 32]) {
    if let Err(err) = store.set_root(pk_bytes) {
        tracing::warn!("Failed to set social graph root: {err}");
    }
}

pub fn get_follow_distance(
    backend: &(impl SocialGraphBackend + ?Sized),
    pk_bytes: &[u8; 32],
) -> Option<u32> {
    backend.follow_distance(pk_bytes).ok().flatten()
}

pub fn get_follows(
    backend: &(impl SocialGraphBackend + ?Sized),
    pk_bytes: &[u8; 32],
) -> Vec<[u8; 32]> {
    match backend.followed_targets(pk_bytes) {
        Ok(set) => set.into_iter().collect(),
        Err(_) => Vec::new(),
    }
}

pub fn is_overmuted(
    backend: &(impl SocialGraphBackend + ?Sized),
    _root_pk: &[u8; 32],
    user_pk: &[u8; 32],
    threshold: f64,
) -> bool {
    backend
        .is_overmuted_user(user_pk, threshold)
        .unwrap_or(false)
}

pub fn ingest_event(backend: &(impl SocialGraphBackend + ?Sized), _sub_id: &str, event_json: &str) {
    let event = match Event::from_json(event_json) {
        Ok(event) => event,
        Err(_) => return,
    };

    if let Err(err) = backend.ingest_event(&event) {
        tracing::warn!("Failed to ingest social graph event: {err}");
    }
}

pub fn ingest_parsed_event(
    backend: &(impl SocialGraphBackend + ?Sized),
    event: &Event,
) -> Result<()> {
    backend.ingest_event(event)
}

pub fn ingest_parsed_event_with_storage_class(
    backend: &(impl SocialGraphBackend + ?Sized),
    event: &Event,
    storage_class: EventStorageClass,
) -> Result<()> {
    backend.ingest_event_with_storage_class(event, storage_class)
}

pub fn ingest_parsed_events(
    backend: &(impl SocialGraphBackend + ?Sized),
    events: &[Event],
) -> Result<()> {
    backend.ingest_events(events)
}

pub fn ingest_parsed_events_with_storage_class(
    backend: &(impl SocialGraphBackend + ?Sized),
    events: &[Event],
    storage_class: EventStorageClass,
) -> Result<()> {
    backend.ingest_events_with_storage_class(events, storage_class)
}

pub fn ingest_graph_parsed_events(
    backend: &(impl SocialGraphBackend + ?Sized),
    events: &[Event],
) -> Result<()> {
    backend.ingest_graph_events(events)
}

pub fn query_events(
    backend: &(impl SocialGraphBackend + ?Sized),
    filter: &Filter,
    limit: usize,
) -> Vec<Event> {
    backend.query_events(filter, limit).unwrap_or_default()
}

impl SocialGraphStore {
    pub fn set_profile_index_overmute_threshold(&self, threshold: f64) {
        *self
            .profile_index_overmute_threshold
            .lock()
            .expect("profile index overmute threshold") = threshold;
    }

    fn profile_index_overmute_threshold(&self) -> f64 {
        *self
            .profile_index_overmute_threshold
            .lock()
            .expect("profile index overmute threshold")
    }

    fn invalidate_distance_cache(&self) {
        *self.distance_cache.lock().unwrap() = None;
    }

    fn build_distance_cache(state: nostr_social_graph::SocialGraphState) -> Result<DistanceCache> {
        let unique_ids = state
            .unique_ids
            .into_iter()
            .map(|(pubkey, id)| decode_pubkey(&pubkey).map(|decoded| (id, decoded)))
            .collect::<Result<HashMap<_, _>>>()?;

        let mut users_by_distance = BTreeMap::new();
        let mut size_by_distance = BTreeMap::new();
        for (distance, users) in state.users_by_follow_distance {
            let decoded = users
                .into_iter()
                .filter_map(|id| unique_ids.get(&id).copied())
                .collect::<Vec<_>>();
            size_by_distance.insert(distance, decoded.len());
            users_by_distance.insert(distance, decoded);
        }

        let total_follows = state
            .followed_by_user
            .iter()
            .map(|(_, targets)| targets.len())
            .sum::<usize>();
        let total_users = size_by_distance.values().copied().sum();
        let max_depth = size_by_distance.keys().copied().max().unwrap_or_default();

        Ok(DistanceCache {
            stats: SocialGraphStats {
                total_users,
                root: Some(state.root),
                total_follows,
                max_depth,
                size_by_distance,
                enabled: true,
            },
            users_by_distance,
        })
    }

    fn load_distance_cache(&self) -> Result<DistanceCache> {
        if let Some(cache) = self.distance_cache.lock().unwrap().clone() {
            return Ok(cache);
        }

        let state = {
            let graph = self.graph.lock().unwrap();
            graph.export_state().context("export social graph state")?
        };
        let cache = Self::build_distance_cache(state)?;
        *self.distance_cache.lock().unwrap() = Some(cache.clone());
        Ok(cache)
    }

    fn set_root(&self, root: &[u8; 32]) -> Result<()> {
        let root_hex = hex::encode(root);
        {
            let mut graph = self.graph.lock().unwrap();
            if should_replace_placeholder_root(&graph)? {
                let fresh = SocialGraph::new(&root_hex);
                graph
                    .replace_state(&fresh.export_state())
                    .context("replace placeholder social graph root")?;
            } else {
                graph
                    .set_root(&root_hex)
                    .context("set nostr-social-graph root")?;
            }
        }
        self.invalidate_distance_cache();
        Ok(())
    }

    fn stats(&self) -> Result<SocialGraphStats> {
        Ok(self.load_distance_cache()?.stats)
    }

    fn follow_distance(&self, pk_bytes: &[u8; 32]) -> Result<Option<u32>> {
        let graph = self.graph.lock().unwrap();
        let distance = graph
            .get_follow_distance(&hex::encode(pk_bytes))
            .context("read social graph follow distance")?;
        Ok((distance != UNKNOWN_FOLLOW_DISTANCE).then_some(distance))
    }

    fn users_by_follow_distance(&self, distance: u32) -> Result<Vec<[u8; 32]>> {
        Ok(self
            .load_distance_cache()?
            .users_by_distance
            .get(&distance)
            .cloned()
            .unwrap_or_default())
    }

    fn follow_list_created_at(&self, owner: &[u8; 32]) -> Result<Option<u64>> {
        let graph = self.graph.lock().unwrap();
        graph
            .get_follow_list_created_at(&hex::encode(owner))
            .context("read social graph follow list timestamp")
    }

    fn followed_targets(&self, owner: &[u8; 32]) -> Result<UserSet> {
        let graph = self.graph.lock().unwrap();
        decode_pubkey_set(
            graph
                .get_followed_by_user(&hex::encode(owner))
                .context("read followed targets")?,
        )
    }

    fn is_overmuted_user(&self, user_pk: &[u8; 32], threshold: f64) -> Result<bool> {
        if threshold <= 0.0 {
            return Ok(false);
        }
        let graph = self.graph.lock().unwrap();
        graph
            .is_overmuted(&hex::encode(user_pk), threshold)
            .context("check social graph overmute")
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn profile_search_root(&self) -> Result<Option<Cid>> {
        self.profile_index.search_root()
    }

    pub(crate) fn public_events_root(&self) -> Result<Option<Cid>> {
        self.public_events.events_root()
    }

    pub(crate) fn write_public_events_root(&self, root: Option<&Cid>) -> Result<()> {
        self.public_events.write_events_root(root)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn latest_profile_event(&self, pubkey_hex: &str) -> Result<Option<Event>> {
        self.profile_index.profile_event_for_pubkey(pubkey_hex)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn profile_search_entries_for_prefix(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, StoredProfileSearchEntry)>> {
        self.profile_index.search_entries_for_prefix(prefix)
    }

    pub fn sync_profile_index_for_events(&self, events: &[Event]) -> Result<()> {
        self.update_profile_index_for_events(events)
    }

    pub(crate) fn rebuild_profile_index_for_events(&self, events: &[Event]) -> Result<()> {
        let latest_by_pubkey = self.filtered_latest_metadata_events_by_pubkey(events)?;
        let (by_pubkey_root, search_root) = self
            .profile_index
            .rebuild_profile_events(latest_by_pubkey.into_values())?;
        self.profile_index
            .write_by_pubkey_root(by_pubkey_root.as_ref())?;
        self.profile_index.write_search_root(search_root.as_ref())?;
        Ok(())
    }

    pub fn rebuild_profile_index_from_stored_events(&self) -> Result<usize> {
        let public_events_root = self.public_events.events_root()?;
        let ambient_events_root = self.ambient_events.events_root()?;
        if public_events_root.is_none() && ambient_events_root.is_none() {
            self.profile_index.write_by_pubkey_root(None)?;
            self.profile_index.write_search_root(None)?;
            return Ok(0);
        }

        let mut events = Vec::new();
        for (bucket, root) in [
            (&self.public_events, public_events_root),
            (&self.ambient_events, ambient_events_root),
        ] {
            let Some(root) = root else {
                continue;
            };
            let stored = block_on(bucket.event_store.list_by_kind_lossy(
                Some(&root),
                Kind::Metadata.as_u16() as u32,
                ListEventsOptions::default(),
            ))
            .map_err(map_event_store_error)?;
            events.extend(
                stored
                    .into_iter()
                    .map(nostr_event_from_stored)
                    .collect::<Result<Vec<_>>>()?,
            );
        }

        let latest_count = self
            .filtered_latest_metadata_events_by_pubkey(&events)?
            .len();
        self.rebuild_profile_index_for_events(&events)?;
        Ok(latest_count)
    }

    fn update_profile_index_for_events(&self, events: &[Event]) -> Result<()> {
        let latest_by_pubkey = latest_metadata_events_by_pubkey(events);
        let threshold = self.profile_index_overmute_threshold();

        if latest_by_pubkey.is_empty() {
            return Ok(());
        }

        let mut by_pubkey_root = self.profile_index.by_pubkey_root()?;
        let mut search_root = self.profile_index.search_root()?;
        let mut changed = false;

        for event in latest_by_pubkey.into_values() {
            let overmuted = self.is_overmuted_user(&event.pubkey.to_bytes(), threshold)?;
            let (next_by_pubkey_root, next_search_root, updated) = if overmuted {
                self.profile_index.remove_profile_event(
                    by_pubkey_root.as_ref(),
                    search_root.as_ref(),
                    &event.pubkey.to_hex(),
                )?
            } else {
                self.profile_index.update_profile_event(
                    by_pubkey_root.as_ref(),
                    search_root.as_ref(),
                    event,
                )?
            };
            if updated {
                by_pubkey_root = next_by_pubkey_root;
                search_root = next_search_root;
                changed = true;
            }
        }

        if changed {
            self.profile_index
                .write_by_pubkey_root(by_pubkey_root.as_ref())?;
            self.profile_index.write_search_root(search_root.as_ref())?;
        }

        Ok(())
    }

    fn filtered_latest_metadata_events_by_pubkey<'a>(
        &self,
        events: &'a [Event],
    ) -> Result<BTreeMap<String, &'a Event>> {
        let threshold = self.profile_index_overmute_threshold();
        let mut latest_by_pubkey = BTreeMap::<String, &Event>::new();
        for event in events.iter().filter(|event| event.kind == Kind::Metadata) {
            if self.is_overmuted_user(&event.pubkey.to_bytes(), threshold)? {
                continue;
            }
            let pubkey = event.pubkey.to_hex();
            match latest_by_pubkey.get(&pubkey) {
                Some(current) if compare_nostr_events(event, current).is_le() => {}
                _ => {
                    latest_by_pubkey.insert(pubkey, event);
                }
            }
        }
        Ok(latest_by_pubkey)
    }

    fn snapshot_chunks(&self, root: &[u8; 32], options: &BinaryBudget) -> Result<Vec<Bytes>> {
        let state = {
            let graph = self.graph.lock().unwrap();
            graph.export_state().context("export social graph state")?
        };
        let mut graph = SocialGraph::from_state(state).context("rebuild social graph state")?;
        let root_hex = hex::encode(root);
        if graph.get_root() != root_hex {
            graph
                .set_root(&root_hex)
                .context("set snapshot social graph root")?;
        }
        let chunks = graph
            .to_binary_chunks_with_budget(*options)
            .context("encode social graph snapshot")?;
        Ok(chunks.into_iter().map(Bytes::from).collect())
    }

    fn ingest_event(&self, event: &Event) -> Result<()> {
        self.ingest_event_with_storage_class(event, self.default_storage_class_for(event)?)
    }

    fn ingest_events(&self, events: &[Event]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }

        let mut public = Vec::new();
        let mut ambient = Vec::new();
        for event in events {
            match self.default_storage_class_for(event)? {
                EventStorageClass::Public => public.push(event.clone()),
                EventStorageClass::Ambient => ambient.push(event.clone()),
            }
        }

        if !public.is_empty() {
            self.ingest_events_with_storage_class(&public, EventStorageClass::Public)?;
        }
        if !ambient.is_empty() {
            self.ingest_events_with_storage_class(&ambient, EventStorageClass::Ambient)?;
        }

        Ok(())
    }

    fn apply_graph_events_only(&self, events: &[Event]) -> Result<()> {
        let graph_events = events
            .iter()
            .filter(|event| is_social_graph_event(event.kind))
            .collect::<Vec<_>>();
        if graph_events.is_empty() {
            return Ok(());
        }

        {
            let mut graph = self.graph.lock().unwrap();
            let mut snapshot = SocialGraph::from_state(
                graph
                    .export_state()
                    .context("export social graph state for graph-only ingest")?,
            )
            .context("rebuild social graph state for graph-only ingest")?;
            for event in graph_events {
                snapshot.handle_event(&graph_event_from_nostr(event), true, 0.0);
            }
            graph
                .replace_state(&snapshot.export_state())
                .context("replace graph-only social graph state")?;
        }
        self.invalidate_distance_cache();
        Ok(())
    }

    fn query_events(&self, filter: &Filter, limit: usize) -> Result<Vec<Event>> {
        self.query_events_in_scope(filter, limit, EventQueryScope::All)
    }

    fn default_storage_class_for(&self, event: &Event) -> Result<EventStorageClass> {
        let graph = self.graph.lock().unwrap();
        let root_hex = graph.get_root().context("read social graph root")?;
        if root_hex != DEFAULT_ROOT_HEX && root_hex == event.pubkey.to_hex() {
            return Ok(EventStorageClass::Public);
        }
        Ok(EventStorageClass::Ambient)
    }

    fn bucket(&self, storage_class: EventStorageClass) -> &EventIndexBucket {
        match storage_class {
            EventStorageClass::Public => &self.public_events,
            EventStorageClass::Ambient => &self.ambient_events,
        }
    }

    fn ingest_event_with_storage_class(
        &self,
        event: &Event,
        storage_class: EventStorageClass,
    ) -> Result<()> {
        let current_root = self.bucket(storage_class).events_root()?;
        let next_root = self
            .bucket(storage_class)
            .store_event(current_root.as_ref(), event)?;
        self.bucket(storage_class)
            .write_events_root(Some(&next_root))?;

        self.update_profile_index_for_events(std::slice::from_ref(event))?;

        if is_social_graph_event(event.kind) {
            {
                let mut graph = self.graph.lock().unwrap();
                graph
                    .handle_event(&graph_event_from_nostr(event), true, 0.0)
                    .context("ingest social graph event into nostr-social-graph")?;
            }
            self.invalidate_distance_cache();
        }

        Ok(())
    }

    fn ingest_events_with_storage_class(
        &self,
        events: &[Event],
        storage_class: EventStorageClass,
    ) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }

        let bucket = self.bucket(storage_class);
        let current_root = bucket.events_root()?;
        let stored_events = events
            .iter()
            .map(stored_event_from_nostr)
            .collect::<Vec<_>>();
        let next_root = block_on(
            bucket
                .event_store
                .build(current_root.as_ref(), stored_events),
        )
        .map_err(map_event_store_error)?;
        bucket.write_events_root(next_root.as_ref())?;

        self.update_profile_index_for_events(events)?;

        let graph_events = events
            .iter()
            .filter(|event| is_social_graph_event(event.kind))
            .collect::<Vec<_>>();
        if graph_events.is_empty() {
            return Ok(());
        }

        {
            let mut graph = self.graph.lock().unwrap();
            let mut snapshot = SocialGraph::from_state(
                graph
                    .export_state()
                    .context("export social graph state for batch ingest")?,
            )
            .context("rebuild social graph state for batch ingest")?;
            for event in graph_events {
                snapshot.handle_event(&graph_event_from_nostr(event), true, 0.0);
            }
            graph
                .replace_state(&snapshot.export_state())
                .context("replace batched social graph state")?;
        }
        self.invalidate_distance_cache();

        Ok(())
    }

    pub(crate) fn query_events_in_scope(
        &self,
        filter: &Filter,
        limit: usize,
        scope: EventQueryScope,
    ) -> Result<Vec<Event>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let buckets: &[&EventIndexBucket] = match scope {
            EventQueryScope::PublicOnly => &[&self.public_events],
            EventQueryScope::AmbientOnly => &[&self.ambient_events],
            EventQueryScope::All => &[&self.public_events, &self.ambient_events],
        };

        let mut candidates = Vec::new();
        for bucket in buckets {
            candidates.extend(bucket.query_events(filter, limit)?);
        }

        let mut deduped = dedupe_events(candidates);
        deduped.retain(|event| filter.match_event(event));
        deduped.truncate(limit);
        Ok(deduped)
    }
}

impl EventIndexBucket {
    fn events_root(&self) -> Result<Option<Cid>> {
        let _profile = NostrProfileGuard::new("socialgraph.events_root.read");
        read_root_file(&self.root_path)
    }

    fn write_events_root(&self, root: Option<&Cid>) -> Result<()> {
        let _profile = NostrProfileGuard::new("socialgraph.events_root.write");
        write_root_file(&self.root_path, root)
    }

    fn store_event(&self, root: Option<&Cid>, event: &Event) -> Result<Cid> {
        let stored = stored_event_from_nostr(event);
        let _profile = NostrProfileGuard::new("socialgraph.event_store.add");
        block_on(self.event_store.add(root, stored)).map_err(map_event_store_error)
    }

    fn load_event_by_id(&self, root: &Cid, event_id: &str) -> Result<Option<Event>> {
        let stored = block_on(self.event_store.get_by_id(Some(root), event_id))
            .map_err(map_event_store_error)?;
        stored.map(nostr_event_from_stored).transpose()
    }

    fn load_events_for_author(
        &self,
        root: &Cid,
        author: &nostr::PublicKey,
        filter: &Filter,
        limit: usize,
        exact: bool,
    ) -> Result<Vec<Event>> {
        let kind_filter = filter.kinds.as_ref().and_then(|kinds| {
            if kinds.len() == 1 {
                kinds.iter().next().map(|kind| kind.as_u16() as u32)
            } else {
                None
            }
        });
        let author_hex = author.to_hex();
        let options = filter_list_options(filter, limit, exact);
        let stored = match kind_filter {
            Some(kind) => block_on(self.event_store.list_by_author_and_kind(
                Some(root),
                &author_hex,
                kind,
                options.clone(),
            ))
            .map_err(map_event_store_error)?,
            None => block_on(
                self.event_store
                    .list_by_author(Some(root), &author_hex, options),
            )
            .map_err(map_event_store_error)?,
        };
        stored
            .into_iter()
            .map(nostr_event_from_stored)
            .collect::<Result<Vec<_>>>()
    }

    fn load_events_for_kind(
        &self,
        root: &Cid,
        kind: Kind,
        filter: &Filter,
        limit: usize,
        exact: bool,
    ) -> Result<Vec<Event>> {
        let stored = block_on(self.event_store.list_by_kind(
            Some(root),
            kind.as_u16() as u32,
            filter_list_options(filter, limit, exact),
        ))
        .map_err(map_event_store_error)?;
        stored
            .into_iter()
            .map(nostr_event_from_stored)
            .collect::<Result<Vec<_>>>()
    }

    fn load_recent_events(
        &self,
        root: &Cid,
        filter: &Filter,
        limit: usize,
        exact: bool,
    ) -> Result<Vec<Event>> {
        let stored = block_on(
            self.event_store
                .list_recent(Some(root), filter_list_options(filter, limit, exact)),
        )
        .map_err(map_event_store_error)?;
        stored
            .into_iter()
            .map(nostr_event_from_stored)
            .collect::<Result<Vec<_>>>()
    }

    fn load_events_for_tag(
        &self,
        root: &Cid,
        tag_name: &str,
        values: &[String],
        filter: &Filter,
        limit: usize,
        exact: bool,
    ) -> Result<Vec<Event>> {
        let mut events = Vec::new();
        let options = filter_list_options(filter, limit, exact);
        for value in values {
            let stored = block_on(self.event_store.list_by_tag(
                Some(root),
                tag_name,
                value,
                options.clone(),
            ))
            .map_err(map_event_store_error)?;
            events.extend(
                stored
                    .into_iter()
                    .map(nostr_event_from_stored)
                    .collect::<Result<Vec<_>>>()?,
            );
        }
        Ok(dedupe_events(events))
    }

    fn choose_tag_source(&self, filter: &Filter) -> Option<(String, Vec<String>)> {
        filter
            .generic_tags
            .iter()
            .min_by_key(|(_, values)| values.len())
            .map(|(tag, values)| {
                (
                    tag.as_char().to_ascii_lowercase().to_string(),
                    values.iter().cloned().collect(),
                )
            })
    }

    fn load_major_index_candidates(
        &self,
        root: &Cid,
        filter: &Filter,
        limit: usize,
    ) -> Result<Option<Vec<Event>>> {
        if let Some(events) = self.load_direct_replaceable_candidates(root, filter)? {
            return Ok(Some(events));
        }

        if let Some((tag_name, values)) = self.choose_tag_source(filter) {
            let exact = filter.authors.is_none()
                && filter.kinds.is_none()
                && filter.search.is_none()
                && filter.generic_tags.len() == 1;
            return Ok(Some(self.load_events_for_tag(
                root, &tag_name, &values, filter, limit, exact,
            )?));
        }

        if let (Some(authors), Some(kinds)) = (filter.authors.as_ref(), filter.kinds.as_ref()) {
            if authors.len() == 1 && kinds.len() == 1 {
                let author = authors.iter().next().expect("checked single author");
                let exact = filter.generic_tags.is_empty() && filter.search.is_none();
                return Ok(Some(
                    self.load_events_for_author(root, author, filter, limit, exact)?,
                ));
            }

            if kinds.len() < authors.len() {
                let mut events = Vec::new();
                for kind in kinds {
                    events.extend(self.load_events_for_kind(root, *kind, filter, limit, false)?);
                }
                return Ok(Some(dedupe_events(events)));
            }

            let mut events = Vec::new();
            for author in authors {
                events.extend(self.load_events_for_author(root, author, filter, limit, false)?);
            }
            return Ok(Some(dedupe_events(events)));
        }

        if let Some(authors) = filter.authors.as_ref() {
            let mut events = Vec::new();
            let exact = filter.generic_tags.is_empty() && filter.search.is_none();
            for author in authors {
                events.extend(self.load_events_for_author(root, author, filter, limit, exact)?);
            }
            return Ok(Some(dedupe_events(events)));
        }

        if let Some(kinds) = filter.kinds.as_ref() {
            let mut events = Vec::new();
            let exact = filter.authors.is_none()
                && filter.generic_tags.is_empty()
                && filter.search.is_none();
            for kind in kinds {
                events.extend(self.load_events_for_kind(root, *kind, filter, limit, exact)?);
            }
            return Ok(Some(dedupe_events(events)));
        }

        Ok(None)
    }

    fn load_direct_replaceable_candidates(
        &self,
        root: &Cid,
        filter: &Filter,
    ) -> Result<Option<Vec<Event>>> {
        let Some(authors) = filter.authors.as_ref() else {
            return Ok(None);
        };
        let Some(kinds) = filter.kinds.as_ref() else {
            return Ok(None);
        };
        if kinds.len() != 1 {
            return Ok(None);
        }

        let kind = kinds.iter().next().expect("checked single kind").as_u16() as u32;

        if is_parameterized_replaceable_kind(kind) {
            let d_tag = SingleLetterTag::lowercase(nostr::Alphabet::D);
            let Some(d_values) = filter.generic_tags.get(&d_tag) else {
                return Ok(None);
            };
            let mut events = Vec::new();
            for author in authors {
                let author_hex = author.to_hex();
                for d_value in d_values {
                    if let Some(stored) = block_on(self.event_store.get_parameterized_replaceable(
                        Some(root),
                        &author_hex,
                        kind,
                        d_value,
                    ))
                    .map_err(map_event_store_error)?
                    {
                        events.push(nostr_event_from_stored(stored)?);
                    }
                }
            }
            return Ok(Some(dedupe_events(events)));
        }

        if is_replaceable_kind(kind) {
            let mut events = Vec::new();
            for author in authors {
                if let Some(stored) = block_on(self.event_store.get_replaceable(
                    Some(root),
                    &author.to_hex(),
                    kind,
                ))
                .map_err(map_event_store_error)?
                {
                    events.push(nostr_event_from_stored(stored)?);
                }
            }
            return Ok(Some(dedupe_events(events)));
        }

        Ok(None)
    }

    fn query_events(&self, filter: &Filter, limit: usize) -> Result<Vec<Event>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let events_root = self.events_root()?;
        let Some(root) = events_root.as_ref() else {
            return Ok(Vec::new());
        };
        let mut candidates = Vec::new();
        let mut seen: HashSet<[u8; 32]> = HashSet::new();

        if let Some(ids) = filter.ids.as_ref() {
            for id in ids {
                let id_bytes = id.to_bytes();
                if !seen.insert(id_bytes) {
                    continue;
                }
                if let Some(event) = self.load_event_by_id(root, &id.to_hex())? {
                    if filter.match_event(&event) {
                        candidates.push(event);
                    }
                }
                if candidates.len() >= limit {
                    break;
                }
            }
        } else {
            let base_events = match self.load_major_index_candidates(root, filter, limit)? {
                Some(events) => events,
                None => self.load_recent_events(
                    root,
                    filter,
                    limit,
                    filter.authors.is_none()
                        && filter.kinds.is_none()
                        && filter.generic_tags.is_empty()
                        && filter.search.is_none(),
                )?,
            };

            for event in base_events {
                let id_bytes = event.id.to_bytes();
                if !seen.insert(id_bytes) {
                    continue;
                }
                if filter.match_event(&event) {
                    candidates.push(event);
                }
                if candidates.len() >= limit {
                    break;
                }
            }
        }

        candidates.sort_by(|a, b| {
            b.created_at
                .as_u64()
                .cmp(&a.created_at.as_u64())
                .then_with(|| a.id.cmp(&b.id))
        });
        candidates.truncate(limit);
        Ok(candidates)
    }
}

impl ProfileIndexBucket {
    fn by_pubkey_root(&self) -> Result<Option<Cid>> {
        let _profile = NostrProfileGuard::new("socialgraph.profile_by_pubkey_root.read");
        read_root_file(&self.by_pubkey_root_path)
    }

    fn search_root(&self) -> Result<Option<Cid>> {
        let _profile = NostrProfileGuard::new("socialgraph.profile_search_root.read");
        read_root_file(&self.search_root_path)
    }

    fn write_by_pubkey_root(&self, root: Option<&Cid>) -> Result<()> {
        let _profile = NostrProfileGuard::new("socialgraph.profile_by_pubkey_root.write");
        write_root_file(&self.by_pubkey_root_path, root)
    }

    fn write_search_root(&self, root: Option<&Cid>) -> Result<()> {
        let _profile = NostrProfileGuard::new("socialgraph.profile_search_root.write");
        write_root_file(&self.search_root_path, root)
    }

    fn mirror_profile_event(&self, event: &Event) -> Result<Cid> {
        let bytes = event.as_json().into_bytes();
        block_on(self.tree.put_file(&bytes))
            .map(|(cid, _size)| cid)
            .context("store mirrored profile event")
    }

    fn load_profile_event(&self, cid: &Cid) -> Result<Option<Event>> {
        let bytes = block_on(self.tree.get(cid, None)).context("read mirrored profile event")?;
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        let json = String::from_utf8(bytes).context("decode mirrored profile event as utf-8")?;
        Ok(Some(
            Event::from_json(json).context("decode mirrored profile event json")?,
        ))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn profile_event_for_pubkey(&self, pubkey_hex: &str) -> Result<Option<Event>> {
        let root = self.by_pubkey_root()?;
        let Some(cid) = block_on(self.index.get_link(root.as_ref(), pubkey_hex))
            .context("read mirrored profile event cid by pubkey")?
        else {
            return Ok(None);
        };
        self.load_profile_event(&cid)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn search_entries_for_prefix(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, StoredProfileSearchEntry)>> {
        let Some(root) = self.search_root()? else {
            return Ok(Vec::new());
        };
        let entries =
            block_on(self.index.prefix(&root, prefix)).context("query profile search prefix")?;
        entries
            .into_iter()
            .map(|(key, value)| {
                let entry = serde_json::from_str(&value)
                    .context("decode stored profile search entry json")?;
                Ok((key, entry))
            })
            .collect()
    }

    fn rebuild_profile_events<'a, I>(&self, events: I) -> Result<(Option<Cid>, Option<Cid>)>
    where
        I: IntoIterator<Item = &'a Event>,
    {
        let mut by_pubkey_entries = Vec::<(String, Cid)>::new();
        let mut search_entries = Vec::<(String, String)>::new();

        for event in events {
            let pubkey = event.pubkey.to_hex();
            let mirrored_cid = self.mirror_profile_event(event)?;
            let search_value =
                serialize_profile_search_entry(&build_profile_search_entry(event, &mirrored_cid)?)?;
            by_pubkey_entries.push((pubkey.clone(), mirrored_cid.clone()));
            for term in profile_search_terms_for_event(event) {
                search_entries.push((
                    format!("{PROFILE_SEARCH_PREFIX}{term}:{pubkey}"),
                    search_value.clone(),
                ));
            }
        }

        let by_pubkey_root = block_on(self.index.build_links(by_pubkey_entries))
            .context("bulk build mirrored profile-by-pubkey index")?;
        let search_root = block_on(self.index.build(search_entries))
            .context("bulk build mirrored profile search index")?;
        Ok((by_pubkey_root, search_root))
    }

    fn update_profile_event(
        &self,
        by_pubkey_root: Option<&Cid>,
        search_root: Option<&Cid>,
        event: &Event,
    ) -> Result<(Option<Cid>, Option<Cid>, bool)> {
        let pubkey = event.pubkey.to_hex();
        let existing_cid = block_on(self.index.get_link(by_pubkey_root, &pubkey))
            .context("lookup existing mirrored profile event")?;

        let existing_event = match existing_cid.as_ref() {
            Some(cid) => self.load_profile_event(cid)?,
            None => None,
        };

        if existing_event
            .as_ref()
            .is_some_and(|current| compare_nostr_events(event, current).is_le())
        {
            return Ok((by_pubkey_root.cloned(), search_root.cloned(), false));
        }

        let mirrored_cid = self.mirror_profile_event(event)?;
        let next_by_pubkey_root = Some(
            block_on(
                self.index
                    .insert_link(by_pubkey_root, &pubkey, &mirrored_cid),
            )
            .context("write mirrored profile event index")?,
        );

        let mut next_search_root = search_root.cloned();
        if let Some(current) = existing_event.as_ref() {
            for term in profile_search_terms_for_event(current) {
                let Some(root) = next_search_root.as_ref() else {
                    break;
                };
                next_search_root = block_on(
                    self.index
                        .delete(root, &format!("{PROFILE_SEARCH_PREFIX}{term}:{pubkey}")),
                )
                .context("remove stale profile search term")?;
            }
        }

        let search_value =
            serialize_profile_search_entry(&build_profile_search_entry(event, &mirrored_cid)?)?;
        for term in profile_search_terms_for_event(event) {
            next_search_root = Some(
                block_on(self.index.insert(
                    next_search_root.as_ref(),
                    &format!("{PROFILE_SEARCH_PREFIX}{term}:{pubkey}"),
                    &search_value,
                ))
                .context("write profile search term")?,
            );
        }

        Ok((next_by_pubkey_root, next_search_root, true))
    }

    fn remove_profile_event(
        &self,
        by_pubkey_root: Option<&Cid>,
        search_root: Option<&Cid>,
        pubkey: &str,
    ) -> Result<(Option<Cid>, Option<Cid>, bool)> {
        let existing_cid = block_on(self.index.get_link(by_pubkey_root, pubkey))
            .context("lookup mirrored profile event for removal")?;
        let Some(existing_cid) = existing_cid else {
            return Ok((by_pubkey_root.cloned(), search_root.cloned(), false));
        };

        let existing_event = self.load_profile_event(&existing_cid)?;
        let next_by_pubkey_root = match by_pubkey_root {
            Some(root) => block_on(self.index.delete(root, pubkey))
                .context("remove mirrored profile-by-pubkey entry")?,
            None => None,
        };

        let mut next_search_root = search_root.cloned();
        if let Some(current) = existing_event.as_ref() {
            for term in profile_search_terms_for_event(current) {
                let Some(root) = next_search_root.as_ref() else {
                    break;
                };
                next_search_root = block_on(
                    self.index
                        .delete(root, &format!("{PROFILE_SEARCH_PREFIX}{term}:{pubkey}")),
                )
                .context("remove overmuted profile search term")?;
            }
        }

        Ok((next_by_pubkey_root, next_search_root, true))
    }
}

fn latest_metadata_events_by_pubkey<'a>(events: &'a [Event]) -> BTreeMap<String, &'a Event> {
    let mut latest_by_pubkey = BTreeMap::<String, &Event>::new();
    for event in events.iter().filter(|event| event.kind == Kind::Metadata) {
        let pubkey = event.pubkey.to_hex();
        match latest_by_pubkey.get(&pubkey) {
            Some(current) if compare_nostr_events(event, current).is_le() => {}
            _ => {
                latest_by_pubkey.insert(pubkey, event);
            }
        }
    }
    latest_by_pubkey
}

fn serialize_profile_search_entry(entry: &StoredProfileSearchEntry) -> Result<String> {
    serde_json::to_string(entry).context("encode stored profile search entry json")
}

fn cid_to_nhash(cid: &Cid) -> Result<String> {
    nhash_encode_full(&NHashData {
        hash: cid.hash,
        decrypt_key: cid.key,
    })
    .context("encode mirrored profile event nhash")
}

fn build_profile_search_entry(
    event: &Event,
    mirrored_cid: &Cid,
) -> Result<StoredProfileSearchEntry> {
    let profile = match serde_json::from_str::<serde_json::Value>(&event.content) {
        Ok(serde_json::Value::Object(profile)) => profile,
        _ => serde_json::Map::new(),
    };
    let names = extract_profile_names(&profile);
    let primary_name = names.first().cloned();
    let nip05 = normalize_profile_nip05(&profile, primary_name.as_deref());
    let name = primary_name
        .clone()
        .or_else(|| nip05.clone())
        .unwrap_or_else(|| event.pubkey.to_hex());

    Ok(StoredProfileSearchEntry {
        pubkey: event.pubkey.to_hex(),
        name,
        aliases: names.into_iter().skip(1).collect(),
        nip05,
        created_at: event.created_at.as_u64(),
        event_nhash: cid_to_nhash(mirrored_cid)?,
    })
}

fn filter_list_options(filter: &Filter, limit: usize, exact: bool) -> ListEventsOptions {
    ListEventsOptions {
        limit: exact.then_some(limit.max(1)),
        since: filter.since.map(|timestamp| timestamp.as_u64()),
        until: filter.until.map(|timestamp| timestamp.as_u64()),
    }
}

fn dedupe_events(events: Vec<Event>) -> Vec<Event> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for event in events {
        if seen.insert(event.id.to_bytes()) {
            deduped.push(event);
        }
    }
    deduped.sort_by(|a, b| {
        b.created_at
            .as_u64()
            .cmp(&a.created_at.as_u64())
            .then_with(|| a.id.cmp(&b.id))
    });
    deduped
}

impl SocialGraphBackend for SocialGraphStore {
    fn stats(&self) -> Result<SocialGraphStats> {
        SocialGraphStore::stats(self)
    }

    fn users_by_follow_distance(&self, distance: u32) -> Result<Vec<[u8; 32]>> {
        SocialGraphStore::users_by_follow_distance(self, distance)
    }

    fn follow_distance(&self, pk_bytes: &[u8; 32]) -> Result<Option<u32>> {
        SocialGraphStore::follow_distance(self, pk_bytes)
    }

    fn follow_list_created_at(&self, owner: &[u8; 32]) -> Result<Option<u64>> {
        SocialGraphStore::follow_list_created_at(self, owner)
    }

    fn followed_targets(&self, owner: &[u8; 32]) -> Result<UserSet> {
        SocialGraphStore::followed_targets(self, owner)
    }

    fn is_overmuted_user(&self, user_pk: &[u8; 32], threshold: f64) -> Result<bool> {
        SocialGraphStore::is_overmuted_user(self, user_pk, threshold)
    }

    fn profile_search_root(&self) -> Result<Option<Cid>> {
        SocialGraphStore::profile_search_root(self)
    }

    fn snapshot_chunks(&self, root: &[u8; 32], options: &BinaryBudget) -> Result<Vec<Bytes>> {
        SocialGraphStore::snapshot_chunks(self, root, options)
    }

    fn ingest_event(&self, event: &Event) -> Result<()> {
        SocialGraphStore::ingest_event(self, event)
    }

    fn ingest_event_with_storage_class(
        &self,
        event: &Event,
        storage_class: EventStorageClass,
    ) -> Result<()> {
        SocialGraphStore::ingest_event_with_storage_class(self, event, storage_class)
    }

    fn ingest_events(&self, events: &[Event]) -> Result<()> {
        SocialGraphStore::ingest_events(self, events)
    }

    fn ingest_events_with_storage_class(
        &self,
        events: &[Event],
        storage_class: EventStorageClass,
    ) -> Result<()> {
        SocialGraphStore::ingest_events_with_storage_class(self, events, storage_class)
    }

    fn ingest_graph_events(&self, events: &[Event]) -> Result<()> {
        SocialGraphStore::apply_graph_events_only(self, events)
    }

    fn query_events(&self, filter: &Filter, limit: usize) -> Result<Vec<Event>> {
        SocialGraphStore::query_events(self, filter, limit)
    }
}

impl NostrSocialGraphBackend for SocialGraphStore {
    type Error = UpstreamGraphBackendError;

    fn get_root(&self) -> std::result::Result<String, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .get_root()
            .context("read social graph root")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn set_root(&mut self, root: &str) -> std::result::Result<(), Self::Error> {
        let root_bytes =
            decode_pubkey(root).map_err(|err| UpstreamGraphBackendError(err.to_string()))?;
        SocialGraphStore::set_root(self, &root_bytes)
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn handle_event(
        &mut self,
        event: &GraphEvent,
        allow_unknown_authors: bool,
        overmute_threshold: f64,
    ) -> std::result::Result<(), Self::Error> {
        {
            let mut graph = self.graph.lock().unwrap();
            graph
                .handle_event(event, allow_unknown_authors, overmute_threshold)
                .context("ingest social graph event into heed backend")
                .map_err(|err| UpstreamGraphBackendError(err.to_string()))?;
        }
        self.invalidate_distance_cache();
        Ok(())
    }

    fn get_follow_distance(&self, user: &str) -> std::result::Result<u32, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .get_follow_distance(user)
            .context("read social graph follow distance")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn is_following(
        &self,
        follower: &str,
        followed_user: &str,
    ) -> std::result::Result<bool, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .is_following(follower, followed_user)
            .context("read social graph following edge")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn get_followed_by_user(&self, user: &str) -> std::result::Result<Vec<String>, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .get_followed_by_user(user)
            .context("read followed-by-user list")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn get_followers_by_user(&self, user: &str) -> std::result::Result<Vec<String>, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .get_followers_by_user(user)
            .context("read followers-by-user list")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn get_muted_by_user(&self, user: &str) -> std::result::Result<Vec<String>, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .get_muted_by_user(user)
            .context("read muted-by-user list")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn get_user_muted_by(&self, user: &str) -> std::result::Result<Vec<String>, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .get_user_muted_by(user)
            .context("read user-muted-by list")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn get_follow_list_created_at(
        &self,
        user: &str,
    ) -> std::result::Result<Option<u64>, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .get_follow_list_created_at(user)
            .context("read social graph follow list timestamp")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn get_mute_list_created_at(
        &self,
        user: &str,
    ) -> std::result::Result<Option<u64>, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .get_mute_list_created_at(user)
            .context("read social graph mute list timestamp")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn is_overmuted(&self, user: &str, threshold: f64) -> std::result::Result<bool, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .is_overmuted(user, threshold)
            .context("check social graph overmute")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }
}

impl<T> SocialGraphBackend for Arc<T>
where
    T: SocialGraphBackend + ?Sized,
{
    fn stats(&self) -> Result<SocialGraphStats> {
        self.as_ref().stats()
    }

    fn users_by_follow_distance(&self, distance: u32) -> Result<Vec<[u8; 32]>> {
        self.as_ref().users_by_follow_distance(distance)
    }

    fn follow_distance(&self, pk_bytes: &[u8; 32]) -> Result<Option<u32>> {
        self.as_ref().follow_distance(pk_bytes)
    }

    fn follow_list_created_at(&self, owner: &[u8; 32]) -> Result<Option<u64>> {
        self.as_ref().follow_list_created_at(owner)
    }

    fn followed_targets(&self, owner: &[u8; 32]) -> Result<UserSet> {
        self.as_ref().followed_targets(owner)
    }

    fn is_overmuted_user(&self, user_pk: &[u8; 32], threshold: f64) -> Result<bool> {
        self.as_ref().is_overmuted_user(user_pk, threshold)
    }

    fn profile_search_root(&self) -> Result<Option<Cid>> {
        self.as_ref().profile_search_root()
    }

    fn snapshot_chunks(&self, root: &[u8; 32], options: &BinaryBudget) -> Result<Vec<Bytes>> {
        self.as_ref().snapshot_chunks(root, options)
    }

    fn ingest_event(&self, event: &Event) -> Result<()> {
        self.as_ref().ingest_event(event)
    }

    fn ingest_event_with_storage_class(
        &self,
        event: &Event,
        storage_class: EventStorageClass,
    ) -> Result<()> {
        self.as_ref()
            .ingest_event_with_storage_class(event, storage_class)
    }

    fn ingest_events(&self, events: &[Event]) -> Result<()> {
        self.as_ref().ingest_events(events)
    }

    fn ingest_events_with_storage_class(
        &self,
        events: &[Event],
        storage_class: EventStorageClass,
    ) -> Result<()> {
        self.as_ref()
            .ingest_events_with_storage_class(events, storage_class)
    }

    fn ingest_graph_events(&self, events: &[Event]) -> Result<()> {
        self.as_ref().ingest_graph_events(events)
    }

    fn query_events(&self, filter: &Filter, limit: usize) -> Result<Vec<Event>> {
        self.as_ref().query_events(filter, limit)
    }
}

fn should_replace_placeholder_root(graph: &HeedSocialGraph) -> Result<bool> {
    if graph.get_root().context("read current social graph root")? != DEFAULT_ROOT_HEX {
        return Ok(false);
    }

    let GraphStats {
        users,
        follows,
        mutes,
        ..
    } = graph.size().context("size social graph")?;
    Ok(users <= 1 && follows == 0 && mutes == 0)
}

fn decode_pubkey_set(values: Vec<String>) -> Result<UserSet> {
    let mut set = UserSet::new();
    for value in values {
        set.insert(decode_pubkey(&value)?);
    }
    Ok(set)
}

fn decode_pubkey(value: &str) -> Result<[u8; 32]> {
    let mut bytes = [0u8; 32];
    hex::decode_to_slice(value, &mut bytes)
        .with_context(|| format!("decode social graph pubkey {value}"))?;
    Ok(bytes)
}

fn is_social_graph_event(kind: Kind) -> bool {
    kind == Kind::ContactList || kind == Kind::MuteList
}

fn graph_event_from_nostr(event: &Event) -> GraphEvent {
    GraphEvent {
        created_at: event.created_at.as_u64(),
        content: event.content.clone(),
        tags: event
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect(),
        kind: event.kind.as_u16() as u32,
        pubkey: event.pubkey.to_hex(),
        id: event.id.to_hex(),
        sig: event.sig.to_string(),
    }
}

fn stored_event_from_nostr(event: &Event) -> StoredNostrEvent {
    StoredNostrEvent {
        id: event.id.to_hex(),
        pubkey: event.pubkey.to_hex(),
        created_at: event.created_at.as_u64(),
        kind: event.kind.as_u16() as u32,
        tags: event
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect(),
        content: event.content.clone(),
        sig: event.sig.to_string(),
    }
}

fn nostr_event_from_stored(event: StoredNostrEvent) -> Result<Event> {
    let value = serde_json::json!({
        "id": event.id,
        "pubkey": event.pubkey,
        "created_at": event.created_at,
        "kind": event.kind,
        "tags": event.tags,
        "content": event.content,
        "sig": event.sig,
    });
    Event::from_json(value.to_string()).context("decode stored nostr event")
}

pub(crate) fn stored_event_to_nostr_event(event: StoredNostrEvent) -> Result<Event> {
    nostr_event_from_stored(event)
}

fn encode_cid(cid: &Cid) -> Result<Vec<u8>> {
    rmp_serde::to_vec_named(&StoredCid {
        hash: cid.hash,
        key: cid.key,
    })
    .context("encode social graph events root")
}

fn decode_cid(bytes: &[u8]) -> Result<Option<Cid>> {
    let stored: StoredCid =
        rmp_serde::from_slice(bytes).context("decode social graph events root")?;
    Ok(Some(Cid {
        hash: stored.hash,
        key: stored.key,
    }))
}

fn read_root_file(path: &Path) -> Result<Option<Cid>> {
    let Ok(bytes) = std::fs::read(path) else {
        return Ok(None);
    };
    decode_cid(&bytes)
}

fn write_root_file(path: &Path, root: Option<&Cid>) -> Result<()> {
    let Some(root) = root else {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        return Ok(());
    };

    let encoded = encode_cid(root)?;
    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, encoded)?;
    std::fs::rename(tmp_path, path)?;
    Ok(())
}

fn normalize_profile_name(value: &serde_json::Value) -> Option<String> {
    let raw = value.as_str()?;
    let trimmed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(PROFILE_NAME_MAX_LENGTH).collect())
}

fn extract_profile_names(profile: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    let mut names = Vec::new();
    let mut seen = HashSet::new();

    for key in ["display_name", "displayName", "name", "username"] {
        let Some(value) = profile.get(key).and_then(normalize_profile_name) else {
            continue;
        };
        let lowered = value.to_lowercase();
        if seen.insert(lowered) {
            names.push(value);
        }
    }

    names
}

fn should_reject_profile_nip05(local_part: &str, primary_name: &str) -> bool {
    if local_part.len() == 1 || local_part.starts_with("npub1") {
        return true;
    }

    primary_name
        .to_lowercase()
        .split_whitespace()
        .collect::<String>()
        .contains(local_part)
}

fn normalize_profile_nip05(
    profile: &serde_json::Map<String, serde_json::Value>,
    primary_name: Option<&str>,
) -> Option<String> {
    let raw = profile.get("nip05")?.as_str()?;
    let local_part = raw.split('@').next()?.trim().to_lowercase();
    if local_part.is_empty() {
        return None;
    }
    let truncated: String = local_part.chars().take(PROFILE_NAME_MAX_LENGTH).collect();
    if truncated.is_empty() {
        return None;
    }
    if primary_name.is_some_and(|name| should_reject_profile_nip05(&truncated, name)) {
        return None;
    }
    Some(truncated)
}

fn is_search_stop_word(word: &str) -> bool {
    matches!(
        word,
        "a" | "an"
            | "the"
            | "and"
            | "or"
            | "but"
            | "in"
            | "on"
            | "at"
            | "to"
            | "for"
            | "of"
            | "with"
            | "by"
            | "from"
            | "is"
            | "it"
            | "as"
            | "be"
            | "was"
            | "are"
            | "this"
            | "that"
            | "these"
            | "those"
            | "i"
            | "you"
            | "he"
            | "she"
            | "we"
            | "they"
            | "my"
            | "your"
            | "his"
            | "her"
            | "its"
            | "our"
            | "their"
            | "what"
            | "which"
            | "who"
            | "whom"
            | "how"
            | "when"
            | "where"
            | "why"
            | "will"
            | "would"
            | "could"
            | "should"
            | "can"
            | "may"
            | "might"
            | "must"
            | "have"
            | "has"
            | "had"
            | "do"
            | "does"
            | "did"
            | "been"
            | "being"
            | "get"
            | "got"
            | "just"
            | "now"
            | "then"
            | "so"
            | "if"
            | "not"
            | "no"
            | "yes"
            | "all"
            | "any"
            | "some"
            | "more"
            | "most"
            | "other"
            | "into"
            | "over"
            | "after"
            | "before"
            | "about"
            | "up"
            | "down"
            | "out"
            | "off"
            | "through"
            | "during"
            | "under"
            | "again"
            | "further"
            | "once"
    )
}

fn is_pure_search_number(word: &str) -> bool {
    if !word.chars().all(|ch| ch.is_ascii_digit()) {
        return false;
    }
    !(word.len() == 4
        && word
            .parse::<u16>()
            .is_ok_and(|year| (1900..=2099).contains(&year)))
}

fn split_compound_search_word(word: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = word.chars().collect();

    for (index, ch) in chars.iter().copied().enumerate() {
        let split_before = current.chars().last().is_some_and(|prev| {
            (prev.is_lowercase() && ch.is_uppercase())
                || (prev.is_ascii_digit() && ch.is_alphabetic())
                || (prev.is_alphabetic() && ch.is_ascii_digit())
                || (prev.is_uppercase()
                    && ch.is_uppercase()
                    && chars.get(index + 1).is_some_and(|next| next.is_lowercase()))
        });

        if split_before && !current.is_empty() {
            parts.push(std::mem::take(&mut current));
        }

        current.push(ch);
    }

    if !current.is_empty() {
        parts.push(current);
    }

    parts
}

fn parse_search_keywords(text: &str) -> Vec<String> {
    let mut keywords = Vec::new();
    let mut seen = HashSet::new();

    for word in text
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
    {
        let mut variants = Vec::with_capacity(1 + word.len() / 4);
        variants.push(word.to_lowercase());
        variants.extend(
            split_compound_search_word(word)
                .into_iter()
                .map(|part| part.to_lowercase()),
        );

        for lowered in variants {
            if lowered.chars().count() < 2
                || is_search_stop_word(&lowered)
                || is_pure_search_number(&lowered)
            {
                continue;
            }
            if seen.insert(lowered.clone()) {
                keywords.push(lowered);
            }
        }
    }

    keywords
}

fn profile_search_terms_for_event(event: &Event) -> Vec<String> {
    let profile = match serde_json::from_str::<serde_json::Value>(&event.content) {
        Ok(serde_json::Value::Object(profile)) => profile,
        _ => serde_json::Map::new(),
    };
    let names = extract_profile_names(&profile);
    let primary_name = names.first().map(String::as_str);
    let mut parts = Vec::new();
    if let Some(name) = primary_name {
        parts.push(name.to_string());
    }
    if let Some(nip05) = normalize_profile_nip05(&profile, primary_name) {
        parts.push(nip05);
    }
    parts.push(event.pubkey.to_hex());
    if names.len() > 1 {
        parts.extend(names.into_iter().skip(1));
    }
    parse_search_keywords(&parts.join(" "))
}

fn compare_nostr_events(left: &Event, right: &Event) -> std::cmp::Ordering {
    left.created_at
        .as_u64()
        .cmp(&right.created_at.as_u64())
        .then_with(|| left.id.to_hex().cmp(&right.id.to_hex()))
}

fn map_event_store_error(err: NostrEventStoreError) -> anyhow::Error {
    anyhow::anyhow!("nostr event store error: {err}")
}

fn ensure_social_graph_mapsize(db_dir: &Path, requested_bytes: u64) -> Result<()> {
    let requested = requested_bytes.max(DEFAULT_SOCIALGRAPH_MAP_SIZE_BYTES);
    let page_size = page_size_bytes() as u64;
    let rounded = requested
        .checked_add(page_size.saturating_sub(1))
        .map(|size| size / page_size * page_size)
        .unwrap_or(requested);
    let map_size = usize::try_from(rounded).context("social graph mapsize exceeds usize")?;

    let env = unsafe {
        heed::EnvOpenOptions::new()
            .map_size(DEFAULT_SOCIALGRAPH_MAP_SIZE_BYTES as usize)
            .max_dbs(SOCIALGRAPH_MAX_DBS)
            .open(db_dir)
    }
    .context("open social graph LMDB env for resize")?;
    if env.info().map_size < map_size {
        unsafe { env.resize(map_size) }.context("resize social graph LMDB env")?;
    }

    Ok(())
}

fn page_size_bytes() -> usize {
    page_size::get_granularity()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use hashtree_config::StorageBackend;
    use hashtree_core::{Hash, MemoryStore, Store, StoreError};
    use hashtree_nostr::NostrEventStoreOptions;
    use std::collections::HashSet;
    use std::fs::{self, File};
    use std::io::{BufRead, BufReader};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::Mutex;
    use std::time::Duration;

    use nostr::{EventBuilder, JsonUtil, Keys, Tag, Timestamp};
    use tempfile::TempDir;

    const WELLORDER_FIXTURE_URL: &str =
        "https://wellorder.xyz/nostr/nostr-wellorder-early-500k-v1.jsonl.bz2";

    #[derive(Debug, Clone, Default)]
    struct ReadTraceSnapshot {
        get_calls: u64,
        total_bytes: u64,
        unique_blocks: usize,
        unique_bytes: u64,
        cache_hits: u64,
        remote_fetches: u64,
        remote_bytes: u64,
    }

    #[derive(Debug, Default)]
    struct ReadTraceState {
        get_calls: u64,
        total_bytes: u64,
        unique_hashes: HashSet<Hash>,
        unique_bytes: u64,
        cache_hits: u64,
        remote_fetches: u64,
        remote_bytes: u64,
    }

    #[derive(Debug)]
    struct CountingStore<S: Store> {
        base: Arc<S>,
        state: Mutex<ReadTraceState>,
    }

    impl<S: Store> CountingStore<S> {
        fn new(base: Arc<S>) -> Self {
            Self {
                base,
                state: Mutex::new(ReadTraceState::default()),
            }
        }

        fn reset(&self) {
            *self.state.lock().unwrap() = ReadTraceState::default();
        }

        fn snapshot(&self) -> ReadTraceSnapshot {
            let state = self.state.lock().unwrap();
            ReadTraceSnapshot {
                get_calls: state.get_calls,
                total_bytes: state.total_bytes,
                unique_blocks: state.unique_hashes.len(),
                unique_bytes: state.unique_bytes,
                cache_hits: state.cache_hits,
                remote_fetches: state.remote_fetches,
                remote_bytes: state.remote_bytes,
            }
        }

        fn record_read(&self, hash: &Hash, bytes: usize) {
            let mut state = self.state.lock().unwrap();
            state.get_calls += 1;
            state.total_bytes += bytes as u64;
            if state.unique_hashes.insert(*hash) {
                state.unique_bytes += bytes as u64;
            }
        }
    }

    #[async_trait]
    impl<S: Store> Store for CountingStore<S> {
        async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
            self.base.put(hash, data).await
        }

        async fn put_many(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
            self.base.put_many(items).await
        }

        async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
            let data = self.base.get(hash).await?;
            if let Some(bytes) = data.as_ref() {
                self.record_read(hash, bytes.len());
            }
            Ok(data)
        }

        async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
            self.base.has(hash).await
        }

        async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
            self.base.delete(hash).await
        }
    }

    #[derive(Debug)]
    struct ReadThroughStore<R: Store> {
        cache: Arc<MemoryStore>,
        remote: Arc<R>,
        state: Mutex<ReadTraceState>,
    }

    impl<R: Store> ReadThroughStore<R> {
        fn new(cache: Arc<MemoryStore>, remote: Arc<R>) -> Self {
            Self {
                cache,
                remote,
                state: Mutex::new(ReadTraceState::default()),
            }
        }

        fn snapshot(&self) -> ReadTraceSnapshot {
            let state = self.state.lock().unwrap();
            ReadTraceSnapshot {
                get_calls: state.get_calls,
                total_bytes: state.total_bytes,
                unique_blocks: state.unique_hashes.len(),
                unique_bytes: state.unique_bytes,
                cache_hits: state.cache_hits,
                remote_fetches: state.remote_fetches,
                remote_bytes: state.remote_bytes,
            }
        }
    }

    #[async_trait]
    impl<R: Store> Store for ReadThroughStore<R> {
        async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
            self.cache.put(hash, data).await
        }

        async fn put_many(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
            self.cache.put_many(items).await
        }

        async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
            {
                let mut state = self.state.lock().unwrap();
                state.get_calls += 1;
            }

            if let Some(bytes) = self.cache.get(hash).await? {
                let mut state = self.state.lock().unwrap();
                state.cache_hits += 1;
                state.total_bytes += bytes.len() as u64;
                if state.unique_hashes.insert(*hash) {
                    state.unique_bytes += bytes.len() as u64;
                }
                return Ok(Some(bytes));
            }

            let data = self.remote.get(hash).await?;
            if let Some(bytes) = data.as_ref() {
                let _ = self.cache.put(*hash, bytes.clone()).await?;
                let mut state = self.state.lock().unwrap();
                state.remote_fetches += 1;
                state.remote_bytes += bytes.len() as u64;
                state.total_bytes += bytes.len() as u64;
                if state.unique_hashes.insert(*hash) {
                    state.unique_bytes += bytes.len() as u64;
                }
            }
            Ok(data)
        }

        async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
            if self.cache.has(hash).await? {
                return Ok(true);
            }
            self.remote.has(hash).await
        }

        async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
            let cache_deleted = self.cache.delete(hash).await?;
            let remote_deleted = self.remote.delete(hash).await?;
            Ok(cache_deleted || remote_deleted)
        }
    }

    #[derive(Debug, Clone)]
    enum BenchmarkQueryCase {
        ById {
            id: String,
        },
        ByAuthor {
            pubkey: String,
            limit: usize,
        },
        ByAuthorKind {
            pubkey: String,
            kind: u32,
            limit: usize,
        },
        ByKind {
            kind: u32,
            limit: usize,
        },
        ByTag {
            tag_name: String,
            tag_value: String,
            limit: usize,
        },
        Recent {
            limit: usize,
        },
        Replaceable {
            pubkey: String,
            kind: u32,
        },
        ParameterizedReplaceable {
            pubkey: String,
            kind: u32,
            d_tag: String,
        },
    }

    impl BenchmarkQueryCase {
        fn name(&self) -> &'static str {
            match self {
                BenchmarkQueryCase::ById { .. } => "by_id",
                BenchmarkQueryCase::ByAuthor { .. } => "by_author",
                BenchmarkQueryCase::ByAuthorKind { .. } => "by_author_kind",
                BenchmarkQueryCase::ByKind { .. } => "by_kind",
                BenchmarkQueryCase::ByTag { .. } => "by_tag",
                BenchmarkQueryCase::Recent { .. } => "recent",
                BenchmarkQueryCase::Replaceable { .. } => "replaceable",
                BenchmarkQueryCase::ParameterizedReplaceable { .. } => "parameterized_replaceable",
            }
        }

        async fn execute<S: Store>(
            &self,
            store: &NostrEventStore<S>,
            root: &Cid,
        ) -> Result<usize, NostrEventStoreError> {
            match self {
                BenchmarkQueryCase::ById { id } => {
                    Ok(store.get_by_id(Some(root), id).await?.into_iter().count())
                }
                BenchmarkQueryCase::ByAuthor { pubkey, limit } => Ok(store
                    .list_by_author(
                        Some(root),
                        pubkey,
                        ListEventsOptions {
                            limit: Some(*limit),
                            ..Default::default()
                        },
                    )
                    .await?
                    .len()),
                BenchmarkQueryCase::ByAuthorKind {
                    pubkey,
                    kind,
                    limit,
                } => Ok(store
                    .list_by_author_and_kind(
                        Some(root),
                        pubkey,
                        *kind,
                        ListEventsOptions {
                            limit: Some(*limit),
                            ..Default::default()
                        },
                    )
                    .await?
                    .len()),
                BenchmarkQueryCase::ByKind { kind, limit } => Ok(store
                    .list_by_kind(
                        Some(root),
                        *kind,
                        ListEventsOptions {
                            limit: Some(*limit),
                            ..Default::default()
                        },
                    )
                    .await?
                    .len()),
                BenchmarkQueryCase::ByTag {
                    tag_name,
                    tag_value,
                    limit,
                } => Ok(store
                    .list_by_tag(
                        Some(root),
                        tag_name,
                        tag_value,
                        ListEventsOptions {
                            limit: Some(*limit),
                            ..Default::default()
                        },
                    )
                    .await?
                    .len()),
                BenchmarkQueryCase::Recent { limit } => Ok(store
                    .list_recent(
                        Some(root),
                        ListEventsOptions {
                            limit: Some(*limit),
                            ..Default::default()
                        },
                    )
                    .await?
                    .len()),
                BenchmarkQueryCase::Replaceable { pubkey, kind } => Ok(store
                    .get_replaceable(Some(root), pubkey, *kind)
                    .await?
                    .into_iter()
                    .count()),
                BenchmarkQueryCase::ParameterizedReplaceable {
                    pubkey,
                    kind,
                    d_tag,
                } => Ok(store
                    .get_parameterized_replaceable(Some(root), pubkey, *kind, d_tag)
                    .await?
                    .into_iter()
                    .count()),
            }
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct NetworkModel {
        name: &'static str,
        rtt_ms: f64,
        bandwidth_mib_per_s: f64,
    }

    #[derive(Debug, Clone)]
    struct QueryBenchmarkResult {
        average_duration: Duration,
        p95_duration: Duration,
        reads: ReadTraceSnapshot,
    }

    const NETWORK_MODELS: [NetworkModel; 3] = [
        NetworkModel {
            name: "lan",
            rtt_ms: 2.0,
            bandwidth_mib_per_s: 100.0,
        },
        NetworkModel {
            name: "wan",
            rtt_ms: 40.0,
            bandwidth_mib_per_s: 20.0,
        },
        NetworkModel {
            name: "slow",
            rtt_ms: 120.0,
            bandwidth_mib_per_s: 5.0,
        },
    ];

    #[test]
    fn test_open_social_graph_store() {
        let _guard = test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = open_social_graph_store(tmp.path()).unwrap();
        assert_eq!(Arc::strong_count(&graph_store), 1);
    }

    #[test]
    fn test_set_root_and_get_follow_distance() {
        let _guard = test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = open_social_graph_store(tmp.path()).unwrap();
        let root_pk = [1u8; 32];
        set_social_graph_root(&graph_store, &root_pk);
        assert_eq!(get_follow_distance(&graph_store, &root_pk), Some(0));
    }

    #[test]
    fn test_ingest_event_updates_follows_and_mutes() {
        let _guard = test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = open_social_graph_store(tmp.path()).unwrap();

        let root_keys = Keys::generate();
        let alice_keys = Keys::generate();
        let bob_keys = Keys::generate();

        let root_pk = root_keys.public_key().to_bytes();
        set_social_graph_root(&graph_store, &root_pk);

        let follow = EventBuilder::new(
            Kind::ContactList,
            "",
            vec![Tag::public_key(alice_keys.public_key())],
        )
        .custom_created_at(Timestamp::from_secs(10))
        .to_event(&root_keys)
        .unwrap();
        ingest_event(&graph_store, "follow", &follow.as_json());

        let mute = EventBuilder::new(
            Kind::MuteList,
            "",
            vec![Tag::public_key(bob_keys.public_key())],
        )
        .custom_created_at(Timestamp::from_secs(11))
        .to_event(&root_keys)
        .unwrap();
        ingest_event(&graph_store, "mute", &mute.as_json());

        assert_eq!(
            get_follow_distance(&graph_store, &alice_keys.public_key().to_bytes()),
            Some(1)
        );
        assert!(is_overmuted(
            &graph_store,
            &root_pk,
            &bob_keys.public_key().to_bytes(),
            1.0
        ));
    }

    #[test]
    fn test_metadata_ingest_builds_profile_search_index_and_replaces_old_terms() {
        let _guard = test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = open_social_graph_store(tmp.path()).unwrap();
        let keys = Keys::generate();

        let older = EventBuilder::new(
            Kind::Metadata,
            serde_json::json!({
                "display_name": "sirius",
                "name": "Martti Malmi",
                "username": "mmalmi",
                "nip05": "siriusdev@iris.to",
            })
            .to_string(),
            [],
        )
        .custom_created_at(Timestamp::from_secs(5))
        .to_event(&keys)
        .unwrap();
        let newer = EventBuilder::new(
            Kind::Metadata,
            serde_json::json!({
                "display_name": "bird",
                "nip05": "birdman@iris.to",
            })
            .to_string(),
            [],
        )
        .custom_created_at(Timestamp::from_secs(6))
        .to_event(&keys)
        .unwrap();

        ingest_parsed_event(&graph_store, &older).unwrap();

        let pubkey = keys.public_key().to_hex();
        let entries = graph_store
            .profile_search_entries_for_prefix("p:sirius")
            .unwrap();
        assert!(entries
            .iter()
            .any(|(key, entry)| key == &format!("p:sirius:{pubkey}") && entry.name == "sirius"));
        assert!(entries.iter().any(|(key, entry)| {
            key == &format!("p:siriusdev:{pubkey}")
                && entry.nip05.as_deref() == Some("siriusdev")
                && entry.aliases == vec!["Martti Malmi".to_string(), "mmalmi".to_string()]
                && entry.event_nhash.starts_with("nhash1")
        }));
        assert!(entries.iter().all(|(_, entry)| entry.pubkey == pubkey));
        assert_eq!(
            graph_store
                .latest_profile_event(&pubkey)
                .unwrap()
                .expect("latest mirrored profile")
                .id,
            older.id
        );

        ingest_parsed_event(&graph_store, &newer).unwrap();

        assert!(graph_store
            .profile_search_entries_for_prefix("p:sirius")
            .unwrap()
            .is_empty());
        let bird_entries = graph_store
            .profile_search_entries_for_prefix("p:bird")
            .unwrap();
        assert_eq!(bird_entries.len(), 2);
        assert!(bird_entries
            .iter()
            .any(|(key, entry)| key == &format!("p:bird:{pubkey}") && entry.name == "bird"));
        assert!(bird_entries.iter().any(|(key, entry)| {
            key == &format!("p:birdman:{pubkey}")
                && entry.nip05.as_deref() == Some("birdman")
                && entry.aliases.is_empty()
        }));
        assert_eq!(
            graph_store
                .latest_profile_event(&pubkey)
                .unwrap()
                .expect("latest mirrored profile")
                .id,
            newer.id
        );
    }

    #[test]
    fn test_ambient_metadata_events_are_mirrored_into_public_profile_index() {
        let _guard = test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = open_social_graph_store(tmp.path()).unwrap();
        let keys = Keys::generate();

        let profile = EventBuilder::new(
            Kind::Metadata,
            serde_json::json!({
                "display_name": "ambient bird",
            })
            .to_string(),
            [],
        )
        .custom_created_at(Timestamp::from_secs(5))
        .to_event(&keys)
        .unwrap();

        ingest_parsed_event_with_storage_class(&graph_store, &profile, EventStorageClass::Ambient)
            .unwrap();

        let pubkey = keys.public_key().to_hex();
        let mirrored = graph_store
            .latest_profile_event(&pubkey)
            .unwrap()
            .expect("mirrored ambient profile");
        assert_eq!(mirrored.id, profile.id);
        assert_eq!(
            graph_store
                .profile_search_entries_for_prefix("p:ambient")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn test_metadata_ingest_splits_compound_profile_terms_without_losing_whole_token() {
        let _guard = test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = open_social_graph_store(tmp.path()).unwrap();
        let keys = Keys::generate();

        let profile = EventBuilder::new(
            Kind::Metadata,
            serde_json::json!({
                "display_name": "SirLibre",
                "username": "XMLHttpRequest42",
            })
            .to_string(),
            [],
        )
        .custom_created_at(Timestamp::from_secs(5))
        .to_event(&keys)
        .unwrap();

        ingest_parsed_event(&graph_store, &profile).unwrap();

        let pubkey = keys.public_key().to_hex();
        assert!(
            graph_store
                .profile_search_entries_for_prefix("p:sirlibre")
                .unwrap()
                .iter()
                .any(|(key, entry)| key == &format!("p:sirlibre:{pubkey}")
                    && entry.name == "SirLibre")
        );
        assert!(graph_store
            .profile_search_entries_for_prefix("p:libre")
            .unwrap()
            .iter()
            .any(|(key, entry)| key == &format!("p:libre:{pubkey}") && entry.name == "SirLibre"));
        assert!(graph_store
            .profile_search_entries_for_prefix("p:xml")
            .unwrap()
            .iter()
            .any(|(key, entry)| {
                key == &format!("p:xml:{pubkey}")
                    && entry.aliases == vec!["XMLHttpRequest42".to_string()]
            }));
        assert!(graph_store
            .profile_search_entries_for_prefix("p:request")
            .unwrap()
            .iter()
            .any(|(key, entry)| {
                key == &format!("p:request:{pubkey}")
                    && entry.aliases == vec!["XMLHttpRequest42".to_string()]
            }));
    }

    #[test]
    fn test_profile_search_index_persists_across_reopen() {
        let _guard = test_lock();
        let tmp = TempDir::new().unwrap();
        let keys = Keys::generate();
        let pubkey = keys.public_key().to_hex();

        {
            let graph_store = open_social_graph_store(tmp.path()).unwrap();
            let profile = EventBuilder::new(
                Kind::Metadata,
                serde_json::json!({
                    "display_name": "reopen user",
                })
                .to_string(),
                [],
            )
            .custom_created_at(Timestamp::from_secs(5))
            .to_event(&keys)
            .unwrap();
            ingest_parsed_event(&graph_store, &profile).unwrap();
            assert!(graph_store.profile_search_root().unwrap().is_some());
        }

        let reopened = open_social_graph_store(tmp.path()).unwrap();
        assert!(reopened.profile_search_root().unwrap().is_some());
        assert_eq!(
            reopened
                .latest_profile_event(&pubkey)
                .unwrap()
                .expect("mirrored profile after reopen")
                .pubkey,
            keys.public_key()
        );
        let links = reopened
            .profile_search_entries_for_prefix("p:reopen")
            .unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].0, format!("p:reopen:{pubkey}"));
        assert_eq!(links[0].1.name, "reopen user");
    }

    #[test]
    fn test_profile_search_index_with_shared_hashtree_storage() {
        let _guard = test_lock();
        let tmp = TempDir::new().unwrap();
        let store =
            crate::storage::HashtreeStore::with_options(tmp.path(), None, 1024 * 1024 * 1024)
                .unwrap();
        let graph_store =
            open_social_graph_store_with_storage(tmp.path(), store.store_arc(), None).unwrap();
        let keys = Keys::generate();
        let pubkey = keys.public_key().to_hex();

        let profile = EventBuilder::new(
            Kind::Metadata,
            serde_json::json!({
                "display_name": "shared storage user",
                "nip05": "shareduser@example.com",
            })
            .to_string(),
            [],
        )
        .custom_created_at(Timestamp::from_secs(5))
        .to_event(&keys)
        .unwrap();

        graph_store
            .sync_profile_index_for_events(std::slice::from_ref(&profile))
            .unwrap();
        assert!(graph_store.profile_search_root().unwrap().is_some());
        assert!(graph_store.profile_search_root().unwrap().is_some());
        let links = graph_store
            .profile_search_entries_for_prefix("p:shared")
            .unwrap();
        assert_eq!(links.len(), 2);
        assert!(links
            .iter()
            .any(|(key, entry)| key == &format!("p:shared:{pubkey}")
                && entry.name == "shared storage user"));
        assert!(links
            .iter()
            .any(|(key, entry)| key == &format!("p:shareduser:{pubkey}")
                && entry.nip05.as_deref() == Some("shareduser")));
    }

    #[test]
    fn test_rebuild_profile_index_from_stored_events_uses_ambient_and_public_metadata() {
        let _guard = test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = open_social_graph_store(tmp.path()).unwrap();
        let public_keys = Keys::generate();
        let ambient_keys = Keys::generate();
        let public_pubkey = public_keys.public_key().to_hex();
        let ambient_pubkey = ambient_keys.public_key().to_hex();

        let older = EventBuilder::new(
            Kind::Metadata,
            serde_json::json!({
                "display_name": "petri old",
            })
            .to_string(),
            [],
        )
        .custom_created_at(Timestamp::from_secs(5))
        .to_event(&public_keys)
        .unwrap();
        let newer = EventBuilder::new(
            Kind::Metadata,
            serde_json::json!({
                "display_name": "petri",
                "name": "Petri Example",
                "nip05": "petri@example.com",
            })
            .to_string(),
            [],
        )
        .custom_created_at(Timestamp::from_secs(6))
        .to_event(&public_keys)
        .unwrap();
        let ambient = EventBuilder::new(
            Kind::Metadata,
            serde_json::json!({
                "display_name": "ambient petri",
            })
            .to_string(),
            [],
        )
        .custom_created_at(Timestamp::from_secs(7))
        .to_event(&ambient_keys)
        .unwrap();

        ingest_parsed_event_with_storage_class(&graph_store, &older, EventStorageClass::Public)
            .unwrap();
        ingest_parsed_event_with_storage_class(&graph_store, &newer, EventStorageClass::Public)
            .unwrap();
        ingest_parsed_event_with_storage_class(&graph_store, &ambient, EventStorageClass::Ambient)
            .unwrap();

        graph_store
            .profile_index
            .write_by_pubkey_root(None)
            .unwrap();
        graph_store.profile_index.write_search_root(None).unwrap();

        let rebuilt = graph_store
            .rebuild_profile_index_from_stored_events()
            .unwrap();
        assert_eq!(rebuilt, 2);

        let entries = graph_store
            .profile_search_entries_for_prefix("p:petri")
            .unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|(key, entry)| {
            key == &format!("p:petri:{public_pubkey}")
                && entry.name == "petri"
                && entry.aliases == vec!["Petri Example".to_string()]
                && entry.nip05.is_none()
        }));
        assert!(entries.iter().any(|(key, entry)| {
            key == &format!("p:petri:{ambient_pubkey}")
                && entry.name == "ambient petri"
                && entry.aliases.is_empty()
                && entry.nip05.is_none()
        }));
    }

    #[test]
    fn test_rebuild_profile_index_excludes_overmuted_users() {
        let _guard = test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = open_social_graph_store(tmp.path()).unwrap();
        let root_keys = Keys::generate();
        let muted_keys = Keys::generate();
        let muted_pubkey = muted_keys.public_key().to_hex();

        set_social_graph_root(&graph_store, &root_keys.public_key().to_bytes());
        graph_store.set_profile_index_overmute_threshold(1.0);

        let profile = EventBuilder::new(
            Kind::Metadata,
            serde_json::json!({
                "display_name": "muted petri",
            })
            .to_string(),
            [],
        )
        .custom_created_at(Timestamp::from_secs(5))
        .to_event(&muted_keys)
        .unwrap();
        ingest_parsed_event(&graph_store, &profile).unwrap();
        assert!(graph_store
            .latest_profile_event(&muted_pubkey)
            .unwrap()
            .is_some());

        let mute = EventBuilder::new(
            Kind::MuteList,
            "",
            vec![Tag::public_key(muted_keys.public_key())],
        )
        .custom_created_at(Timestamp::from_secs(6))
        .to_event(&root_keys)
        .unwrap();
        ingest_parsed_event(&graph_store, &mute).unwrap();
        assert!(graph_store
            .is_overmuted_user(&muted_keys.public_key().to_bytes(), 1.0)
            .unwrap());

        let rebuilt = graph_store
            .rebuild_profile_index_from_stored_events()
            .unwrap();
        assert_eq!(rebuilt, 0);
        assert!(graph_store
            .latest_profile_event(&muted_pubkey)
            .unwrap()
            .is_none());
        assert!(graph_store
            .profile_search_entries_for_prefix("p:muted")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn test_query_events_by_author() {
        let _guard = test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = open_social_graph_store(tmp.path()).unwrap();
        let keys = Keys::generate();

        let older = EventBuilder::new(Kind::TextNote, "older", [])
            .custom_created_at(Timestamp::from_secs(5))
            .to_event(&keys)
            .unwrap();
        let newer = EventBuilder::new(Kind::TextNote, "newer", [])
            .custom_created_at(Timestamp::from_secs(6))
            .to_event(&keys)
            .unwrap();

        ingest_parsed_event(&graph_store, &older).unwrap();
        ingest_parsed_event(&graph_store, &newer).unwrap();

        let filter = Filter::new().author(keys.public_key()).kind(Kind::TextNote);
        let events = query_events(&graph_store, &filter, 10);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].id, newer.id);
        assert_eq!(events[1].id, older.id);
    }

    #[test]
    fn test_query_events_by_kind() {
        let _guard = test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = open_social_graph_store(tmp.path()).unwrap();
        let first_keys = Keys::generate();
        let second_keys = Keys::generate();

        let older = EventBuilder::new(Kind::TextNote, "older", [])
            .custom_created_at(Timestamp::from_secs(5))
            .to_event(&first_keys)
            .unwrap();
        let newer = EventBuilder::new(Kind::TextNote, "newer", [])
            .custom_created_at(Timestamp::from_secs(6))
            .to_event(&second_keys)
            .unwrap();
        let other_kind = EventBuilder::new(Kind::Metadata, "profile", [])
            .custom_created_at(Timestamp::from_secs(7))
            .to_event(&second_keys)
            .unwrap();

        ingest_parsed_event(&graph_store, &older).unwrap();
        ingest_parsed_event(&graph_store, &newer).unwrap();
        ingest_parsed_event(&graph_store, &other_kind).unwrap();

        let filter = Filter::new().kind(Kind::TextNote);
        let events = query_events(&graph_store, &filter, 10);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].id, newer.id);
        assert_eq!(events[1].id, older.id);
    }

    #[test]
    fn test_query_events_by_id() {
        let _guard = test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = open_social_graph_store(tmp.path()).unwrap();
        let keys = Keys::generate();

        let first = EventBuilder::new(Kind::TextNote, "first", [])
            .custom_created_at(Timestamp::from_secs(5))
            .to_event(&keys)
            .unwrap();
        let target = EventBuilder::new(Kind::TextNote, "target", [])
            .custom_created_at(Timestamp::from_secs(6))
            .to_event(&keys)
            .unwrap();

        ingest_parsed_event(&graph_store, &first).unwrap();
        ingest_parsed_event(&graph_store, &target).unwrap();

        let filter = Filter::new().id(target.id).kind(Kind::TextNote);
        let events = query_events(&graph_store, &filter, 10);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, target.id);
    }

    #[test]
    fn test_query_events_search_is_case_insensitive() {
        let _guard = test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = open_social_graph_store(tmp.path()).unwrap();
        let keys = Keys::generate();
        let other_keys = Keys::generate();

        let matching = EventBuilder::new(Kind::TextNote, "Hello Nostr Search", [])
            .custom_created_at(Timestamp::from_secs(5))
            .to_event(&keys)
            .unwrap();
        let other = EventBuilder::new(Kind::TextNote, "goodbye world", [])
            .custom_created_at(Timestamp::from_secs(6))
            .to_event(&other_keys)
            .unwrap();

        ingest_parsed_event(&graph_store, &matching).unwrap();
        ingest_parsed_event(&graph_store, &other).unwrap();

        let filter = Filter::new().kind(Kind::TextNote).search("nostr search");
        let events = query_events(&graph_store, &filter, 10);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, matching.id);
    }

    #[test]
    fn test_query_events_since_until_are_inclusive() {
        let _guard = test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = open_social_graph_store(tmp.path()).unwrap();
        let keys = Keys::generate();

        let before = EventBuilder::new(Kind::TextNote, "before", [])
            .custom_created_at(Timestamp::from_secs(5))
            .to_event(&keys)
            .unwrap();
        let start = EventBuilder::new(Kind::TextNote, "start", [])
            .custom_created_at(Timestamp::from_secs(6))
            .to_event(&keys)
            .unwrap();
        let end = EventBuilder::new(Kind::TextNote, "end", [])
            .custom_created_at(Timestamp::from_secs(10))
            .to_event(&keys)
            .unwrap();
        let after = EventBuilder::new(Kind::TextNote, "after", [])
            .custom_created_at(Timestamp::from_secs(11))
            .to_event(&keys)
            .unwrap();

        ingest_parsed_event(&graph_store, &before).unwrap();
        ingest_parsed_event(&graph_store, &start).unwrap();
        ingest_parsed_event(&graph_store, &end).unwrap();
        ingest_parsed_event(&graph_store, &after).unwrap();

        let filter = Filter::new()
            .kind(Kind::TextNote)
            .since(Timestamp::from_secs(6))
            .until(Timestamp::from_secs(10));
        let events = query_events(&graph_store, &filter, 10);
        let ids = events.into_iter().map(|event| event.id).collect::<Vec<_>>();
        assert_eq!(ids, vec![end.id, start.id]);
    }

    #[test]
    fn test_query_events_replaceable_kind_returns_latest_winner() {
        let _guard = test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = open_social_graph_store(tmp.path()).unwrap();
        let keys = Keys::generate();

        let older = EventBuilder::new(Kind::Custom(10_000), "older mute list", [])
            .custom_created_at(Timestamp::from_secs(5))
            .to_event(&keys)
            .unwrap();
        let newer = EventBuilder::new(Kind::Custom(10_000), "newer mute list", [])
            .custom_created_at(Timestamp::from_secs(6))
            .to_event(&keys)
            .unwrap();

        ingest_parsed_event(&graph_store, &older).unwrap();
        ingest_parsed_event(&graph_store, &newer).unwrap();

        let filter = Filter::new()
            .author(keys.public_key())
            .kind(Kind::Custom(10_000));
        let events = query_events(&graph_store, &filter, 10);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, newer.id);
    }

    #[test]
    fn test_query_events_kind_41_replaceable_returns_latest_winner() {
        let _guard = test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = open_social_graph_store(tmp.path()).unwrap();
        let keys = Keys::generate();

        let older = EventBuilder::new(Kind::Custom(41), "older channel metadata", [])
            .custom_created_at(Timestamp::from_secs(5))
            .to_event(&keys)
            .unwrap();
        let newer = EventBuilder::new(Kind::Custom(41), "newer channel metadata", [])
            .custom_created_at(Timestamp::from_secs(6))
            .to_event(&keys)
            .unwrap();

        ingest_parsed_event(&graph_store, &older).unwrap();
        ingest_parsed_event(&graph_store, &newer).unwrap();

        let filter = Filter::new()
            .author(keys.public_key())
            .kind(Kind::Custom(41));
        let events = query_events(&graph_store, &filter, 10);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, newer.id);
    }

    #[test]
    fn test_public_and_ambient_indexes_stay_separate() {
        let _guard = test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = open_social_graph_store(tmp.path()).unwrap();
        let public_keys = Keys::generate();
        let ambient_keys = Keys::generate();

        let public_event = EventBuilder::new(Kind::TextNote, "public", [])
            .custom_created_at(Timestamp::from_secs(5))
            .to_event(&public_keys)
            .unwrap();
        let ambient_event = EventBuilder::new(Kind::TextNote, "ambient", [])
            .custom_created_at(Timestamp::from_secs(6))
            .to_event(&ambient_keys)
            .unwrap();

        ingest_parsed_event_with_storage_class(
            &graph_store,
            &public_event,
            EventStorageClass::Public,
        )
        .unwrap();
        ingest_parsed_event_with_storage_class(
            &graph_store,
            &ambient_event,
            EventStorageClass::Ambient,
        )
        .unwrap();

        let filter = Filter::new().kind(Kind::TextNote);
        let all_events = graph_store
            .query_events_in_scope(&filter, 10, EventQueryScope::All)
            .unwrap();
        assert_eq!(all_events.len(), 2);

        let public_events = graph_store
            .query_events_in_scope(&filter, 10, EventQueryScope::PublicOnly)
            .unwrap();
        assert_eq!(public_events.len(), 1);
        assert_eq!(public_events[0].id, public_event.id);

        let ambient_events = graph_store
            .query_events_in_scope(&filter, 10, EventQueryScope::AmbientOnly)
            .unwrap();
        assert_eq!(ambient_events.len(), 1);
        assert_eq!(ambient_events[0].id, ambient_event.id);
    }

    #[test]
    fn test_default_ingest_classifies_root_author_as_public() {
        let _guard = test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = open_social_graph_store(tmp.path()).unwrap();
        let root_keys = Keys::generate();
        let other_keys = Keys::generate();
        set_social_graph_root(&graph_store, &root_keys.public_key().to_bytes());

        let root_event = EventBuilder::new(Kind::TextNote, "root", [])
            .custom_created_at(Timestamp::from_secs(5))
            .to_event(&root_keys)
            .unwrap();
        let other_event = EventBuilder::new(Kind::TextNote, "other", [])
            .custom_created_at(Timestamp::from_secs(6))
            .to_event(&other_keys)
            .unwrap();

        ingest_parsed_event(&graph_store, &root_event).unwrap();
        ingest_parsed_event(&graph_store, &other_event).unwrap();

        let filter = Filter::new().kind(Kind::TextNote);
        let public_events = graph_store
            .query_events_in_scope(&filter, 10, EventQueryScope::PublicOnly)
            .unwrap();
        assert_eq!(public_events.len(), 1);
        assert_eq!(public_events[0].id, root_event.id);

        let ambient_events = graph_store
            .query_events_in_scope(&filter, 10, EventQueryScope::AmbientOnly)
            .unwrap();
        assert_eq!(ambient_events.len(), 1);
        assert_eq!(ambient_events[0].id, other_event.id);
    }

    #[test]
    fn test_query_events_survives_reopen() {
        let _guard = test_lock();
        let tmp = TempDir::new().unwrap();
        let db_dir = tmp.path().join("socialgraph-store");
        let keys = Keys::generate();
        let other_keys = Keys::generate();

        {
            let graph_store = open_social_graph_store_at_path(&db_dir, None).unwrap();
            let older = EventBuilder::new(Kind::TextNote, "older", [])
                .custom_created_at(Timestamp::from_secs(5))
                .to_event(&keys)
                .unwrap();
            let newer = EventBuilder::new(Kind::TextNote, "newer", [])
                .custom_created_at(Timestamp::from_secs(6))
                .to_event(&keys)
                .unwrap();
            let latest = EventBuilder::new(Kind::TextNote, "latest", [])
                .custom_created_at(Timestamp::from_secs(7))
                .to_event(&other_keys)
                .unwrap();

            ingest_parsed_event(&graph_store, &older).unwrap();
            ingest_parsed_event(&graph_store, &newer).unwrap();
            ingest_parsed_event(&graph_store, &latest).unwrap();
        }

        let reopened = open_social_graph_store_at_path(&db_dir, None).unwrap();

        let author_filter = Filter::new().author(keys.public_key()).kind(Kind::TextNote);
        let author_events = query_events(&reopened, &author_filter, 10);
        assert_eq!(author_events.len(), 2);
        assert_eq!(author_events[0].content, "newer");
        assert_eq!(author_events[1].content, "older");

        let recent_filter = Filter::new().kind(Kind::TextNote);
        let recent_events = query_events(&reopened, &recent_filter, 2);
        assert_eq!(recent_events.len(), 2);
        assert_eq!(recent_events[0].content, "latest");
        assert_eq!(recent_events[1].content, "newer");
    }

    #[test]
    fn test_query_events_parameterized_replaceable_by_d_tag() {
        let _guard = test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = open_social_graph_store(tmp.path()).unwrap();
        let keys = Keys::generate();

        let older = EventBuilder::new(
            Kind::Custom(30078),
            "",
            vec![
                Tag::identifier("video"),
                Tag::parse(&["l", "hashtree"]).unwrap(),
                Tag::parse(&["hash", &"11".repeat(32)]).unwrap(),
            ],
        )
        .custom_created_at(Timestamp::from_secs(5))
        .to_event(&keys)
        .unwrap();
        let newer = EventBuilder::new(
            Kind::Custom(30078),
            "",
            vec![
                Tag::identifier("video"),
                Tag::parse(&["l", "hashtree"]).unwrap(),
                Tag::parse(&["hash", &"22".repeat(32)]).unwrap(),
            ],
        )
        .custom_created_at(Timestamp::from_secs(6))
        .to_event(&keys)
        .unwrap();
        let other_tree = EventBuilder::new(
            Kind::Custom(30078),
            "",
            vec![
                Tag::identifier("files"),
                Tag::parse(&["l", "hashtree"]).unwrap(),
                Tag::parse(&["hash", &"33".repeat(32)]).unwrap(),
            ],
        )
        .custom_created_at(Timestamp::from_secs(7))
        .to_event(&keys)
        .unwrap();

        ingest_parsed_event(&graph_store, &older).unwrap();
        ingest_parsed_event(&graph_store, &newer).unwrap();
        ingest_parsed_event(&graph_store, &other_tree).unwrap();

        let filter = Filter::new()
            .author(keys.public_key())
            .kind(Kind::Custom(30078))
            .identifier("video");
        let events = query_events(&graph_store, &filter, 10);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, newer.id);
    }

    #[test]
    fn test_query_events_by_hashtag_uses_tag_index() {
        let _guard = test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = open_social_graph_store(tmp.path()).unwrap();
        let keys = Keys::generate();
        let other_keys = Keys::generate();

        let first = EventBuilder::new(
            Kind::TextNote,
            "first",
            vec![Tag::parse(&["t", "hashtree"]).unwrap()],
        )
        .custom_created_at(Timestamp::from_secs(5))
        .to_event(&keys)
        .unwrap();
        let second = EventBuilder::new(
            Kind::TextNote,
            "second",
            vec![Tag::parse(&["t", "hashtree"]).unwrap()],
        )
        .custom_created_at(Timestamp::from_secs(6))
        .to_event(&other_keys)
        .unwrap();
        let unrelated = EventBuilder::new(
            Kind::TextNote,
            "third",
            vec![Tag::parse(&["t", "other"]).unwrap()],
        )
        .custom_created_at(Timestamp::from_secs(7))
        .to_event(&other_keys)
        .unwrap();

        ingest_parsed_event(&graph_store, &first).unwrap();
        ingest_parsed_event(&graph_store, &second).unwrap();
        ingest_parsed_event(&graph_store, &unrelated).unwrap();

        let filter = Filter::new().hashtag("hashtree");
        let events = query_events(&graph_store, &filter, 10);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].id, second.id);
        assert_eq!(events[1].id, first.id);
    }

    #[test]
    fn test_query_events_combines_indexes_then_applies_search_filter() {
        let _guard = test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = open_social_graph_store(tmp.path()).unwrap();
        let keys = Keys::generate();
        let other_keys = Keys::generate();

        let matching = EventBuilder::new(
            Kind::TextNote,
            "hashtree video release",
            vec![Tag::parse(&["t", "hashtree"]).unwrap()],
        )
        .custom_created_at(Timestamp::from_secs(5))
        .to_event(&keys)
        .unwrap();
        let non_matching = EventBuilder::new(
            Kind::TextNote,
            "plain text note",
            vec![Tag::parse(&["t", "hashtree"]).unwrap()],
        )
        .custom_created_at(Timestamp::from_secs(6))
        .to_event(&other_keys)
        .unwrap();

        ingest_parsed_event(&graph_store, &matching).unwrap();
        ingest_parsed_event(&graph_store, &non_matching).unwrap();

        let filter = Filter::new().hashtag("hashtree").search("video");
        let events = query_events(&graph_store, &filter, 10);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, matching.id);
    }

    fn benchmark_dataset_path() -> Option<PathBuf> {
        std::env::var_os("HASHTREE_BENCH_DATASET_PATH").map(PathBuf::from)
    }

    fn benchmark_dataset_url() -> String {
        std::env::var("HASHTREE_BENCH_DATASET_URL")
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| WELLORDER_FIXTURE_URL.to_string())
    }

    fn benchmark_stream_warmup_events(measured_events: usize) -> usize {
        std::env::var("HASHTREE_BENCH_WARMUP_EVENTS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or_else(|| measured_events.clamp(1, 200))
    }

    fn ensure_benchmark_dataset(path: &Path, url: &str) -> Result<()> {
        if path.exists() {
            return Ok(());
        }

        let parent = path
            .parent()
            .context("benchmark dataset path has no parent directory")?;
        fs::create_dir_all(parent).context("create benchmark dataset directory")?;

        let tmp = path.with_extension("tmp");
        let mut response = reqwest::blocking::get(url)
            .context("download benchmark dataset")?
            .error_for_status()
            .context("benchmark dataset request failed")?;
        let mut file = File::create(&tmp).context("create temporary benchmark dataset file")?;
        std::io::copy(&mut response, &mut file).context("write benchmark dataset")?;
        fs::rename(&tmp, path).context("move benchmark dataset into place")?;

        Ok(())
    }

    fn load_benchmark_dataset(path: &Path, max_events: usize) -> Result<Vec<Event>> {
        if max_events == 0 {
            return Ok(Vec::new());
        }

        let mut child = Command::new("bzip2")
            .args(["-dc", &path.to_string_lossy()])
            .stdout(Stdio::piped())
            .spawn()
            .context("spawn bzip2 for benchmark dataset")?;
        let stdout = child
            .stdout
            .take()
            .context("benchmark dataset stdout missing")?;
        let mut events = Vec::with_capacity(max_events);

        {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                if events.len() >= max_events {
                    break;
                }
                let line = line.context("read benchmark dataset line")?;
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(event) = Event::from_json(trimmed.to_string()) {
                    events.push(event);
                }
            }
        }

        if events.len() < max_events {
            let status = child.wait().context("wait for benchmark dataset reader")?;
            anyhow::ensure!(
                status.success(),
                "benchmark dataset reader exited with status {status}"
            );
        } else {
            let _ = child.kill();
            let _ = child.wait();
        }

        Ok(events)
    }

    fn build_synthetic_benchmark_events(event_count: usize, author_count: usize) -> Vec<Event> {
        let authors = (0..author_count)
            .map(|_| Keys::generate())
            .collect::<Vec<_>>();
        let mut events = Vec::with_capacity(event_count);
        for i in 0..event_count {
            let kind = if i % 8 < 5 {
                Kind::TextNote
            } else {
                Kind::Custom(30_023)
            };
            let mut tags = Vec::new();
            if kind == Kind::TextNote && i % 16 == 0 {
                tags.push(Tag::parse(&["t", "hashtree"]).unwrap());
            }
            let content = if kind == Kind::TextNote && i % 32 == 0 {
                format!("benchmark target event {i}")
            } else {
                format!("benchmark event {i}")
            };
            let event = EventBuilder::new(kind, content, tags)
                .custom_created_at(Timestamp::from_secs(1_700_000_000 + i as u64))
                .to_event(&authors[i % author_count])
                .unwrap();
            events.push(event);
        }
        events
    }

    fn load_benchmark_events(
        event_count: usize,
        author_count: usize,
    ) -> Result<(String, Vec<Event>)> {
        if let Some(path) = benchmark_dataset_path() {
            let url = benchmark_dataset_url();
            ensure_benchmark_dataset(&path, &url)?;
            let events = load_benchmark_dataset(&path, event_count)?;
            return Ok((format!("dataset:{}", path.display()), events));
        }

        Ok((
            format!("synthetic:{author_count}-authors"),
            build_synthetic_benchmark_events(event_count, author_count),
        ))
    }

    fn first_tag_filter(event: &Event) -> Option<Filter> {
        event.tags.iter().find_map(|tag| match tag.as_slice() {
            [name, value, ..]
                if name.len() == 1
                    && !value.is_empty()
                    && name.as_bytes()[0].is_ascii_lowercase() =>
            {
                let letter = SingleLetterTag::from_char(name.chars().next()?).ok()?;
                Some(Filter::new().custom_tag(letter, [value.to_string()]))
            }
            _ => None,
        })
    }

    fn first_search_term(event: &Event) -> Option<String> {
        event
            .content
            .split(|ch: char| !ch.is_alphanumeric())
            .find(|token| token.len() >= 4)
            .map(|token| token.to_ascii_lowercase())
    }

    fn benchmark_match_count(events: &[Event], filter: &Filter, limit: usize) -> usize {
        events
            .iter()
            .filter(|event| filter.match_event(event))
            .count()
            .min(limit)
    }

    fn benchmark_btree_orders() -> Vec<usize> {
        std::env::var("HASHTREE_BTREE_ORDERS")
            .ok()
            .map(|value| {
                value
                    .split(',')
                    .filter_map(|part| part.trim().parse::<usize>().ok())
                    .filter(|order| *order >= 2)
                    .collect::<Vec<_>>()
            })
            .filter(|orders| !orders.is_empty())
            .unwrap_or_else(|| vec![16, 24, 32, 48, 64])
    }

    fn benchmark_read_iterations() -> usize {
        std::env::var("HASHTREE_BENCH_READ_ITERATIONS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(5)
            .max(1)
    }

    fn average_duration(samples: &[Duration]) -> Duration {
        if samples.is_empty() {
            return Duration::ZERO;
        }

        Duration::from_secs_f64(
            samples.iter().map(Duration::as_secs_f64).sum::<f64>() / samples.len() as f64,
        )
    }

    fn average_read_trace(samples: &[ReadTraceSnapshot]) -> ReadTraceSnapshot {
        if samples.is_empty() {
            return ReadTraceSnapshot::default();
        }

        let len = samples.len() as u64;
        ReadTraceSnapshot {
            get_calls: samples.iter().map(|sample| sample.get_calls).sum::<u64>() / len,
            total_bytes: samples.iter().map(|sample| sample.total_bytes).sum::<u64>() / len,
            unique_blocks: (samples
                .iter()
                .map(|sample| sample.unique_blocks as u64)
                .sum::<u64>()
                / len) as usize,
            unique_bytes: samples
                .iter()
                .map(|sample| sample.unique_bytes)
                .sum::<u64>()
                / len,
            cache_hits: samples.iter().map(|sample| sample.cache_hits).sum::<u64>() / len,
            remote_fetches: samples
                .iter()
                .map(|sample| sample.remote_fetches)
                .sum::<u64>()
                / len,
            remote_bytes: samples
                .iter()
                .map(|sample| sample.remote_bytes)
                .sum::<u64>()
                / len,
        }
    }

    fn estimate_serialized_remote_ms(snapshot: &ReadTraceSnapshot, model: NetworkModel) -> f64 {
        let transfer_ms = if model.bandwidth_mib_per_s <= 0.0 {
            0.0
        } else {
            (snapshot.remote_bytes as f64 / (model.bandwidth_mib_per_s * 1024.0 * 1024.0)) * 1000.0
        };
        snapshot.remote_fetches as f64 * model.rtt_ms + transfer_ms
    }

    #[derive(Debug, Clone)]
    struct IndexBenchmarkDataset {
        source: String,
        events: Vec<Event>,
        guaranteed_tag_name: String,
        guaranteed_tag_value: String,
        replaceable_pubkey: String,
        replaceable_kind: u32,
        parameterized_pubkey: String,
        parameterized_kind: u32,
        parameterized_d_tag: String,
    }

    fn load_index_benchmark_dataset(
        event_count: usize,
        author_count: usize,
    ) -> Result<IndexBenchmarkDataset> {
        let (source, mut events) = load_benchmark_events(event_count, author_count)?;
        let base_timestamp = events
            .iter()
            .map(|event| event.created_at.as_u64())
            .max()
            .unwrap_or(1_700_000_000)
            + 1;

        let replaceable_keys = Keys::generate();
        let parameterized_keys = Keys::generate();
        let tagged_keys = Keys::generate();
        let guaranteed_tag_name = "t".to_string();
        let guaranteed_tag_value = "btreebench".to_string();
        let replaceable_kind = 10_000u32;
        let parameterized_kind = 30_023u32;
        let parameterized_d_tag = "btree-bench".to_string();

        let tagged = EventBuilder::new(
            Kind::TextNote,
            "btree benchmark tagged note",
            vec![Tag::parse(&["t", &guaranteed_tag_value]).unwrap()],
        )
        .custom_created_at(Timestamp::from_secs(base_timestamp))
        .to_event(&tagged_keys)
        .unwrap();
        let replaceable_old = EventBuilder::new(
            Kind::Custom(replaceable_kind.try_into().unwrap()),
            "replaceable old",
            [],
        )
        .custom_created_at(Timestamp::from_secs(base_timestamp + 1))
        .to_event(&replaceable_keys)
        .unwrap();
        let replaceable_new = EventBuilder::new(
            Kind::Custom(replaceable_kind.try_into().unwrap()),
            "replaceable new",
            [],
        )
        .custom_created_at(Timestamp::from_secs(base_timestamp + 2))
        .to_event(&replaceable_keys)
        .unwrap();
        let parameterized_old = EventBuilder::new(
            Kind::Custom(parameterized_kind.try_into().unwrap()),
            "",
            vec![Tag::identifier(&parameterized_d_tag)],
        )
        .custom_created_at(Timestamp::from_secs(base_timestamp + 3))
        .to_event(&parameterized_keys)
        .unwrap();
        let parameterized_new = EventBuilder::new(
            Kind::Custom(parameterized_kind.try_into().unwrap()),
            "",
            vec![Tag::identifier(&parameterized_d_tag)],
        )
        .custom_created_at(Timestamp::from_secs(base_timestamp + 4))
        .to_event(&parameterized_keys)
        .unwrap();

        events.extend([
            tagged,
            replaceable_old,
            replaceable_new,
            parameterized_old,
            parameterized_new,
        ]);

        Ok(IndexBenchmarkDataset {
            source,
            events,
            guaranteed_tag_name,
            guaranteed_tag_value,
            replaceable_pubkey: replaceable_keys.public_key().to_hex(),
            replaceable_kind,
            parameterized_pubkey: parameterized_keys.public_key().to_hex(),
            parameterized_kind,
            parameterized_d_tag,
        })
    }

    fn build_btree_query_cases(dataset: &IndexBenchmarkDataset) -> Vec<BenchmarkQueryCase> {
        let primary_kind = dataset
            .events
            .iter()
            .find(|event| event.kind == Kind::TextNote)
            .map(|event| event.kind)
            .or_else(|| dataset.events.first().map(|event| event.kind))
            .expect("benchmark requires at least one event");
        let primary_kind_u32 = primary_kind.as_u16() as u32;

        let author_pubkey = dataset
            .events
            .iter()
            .filter(|event| event.kind == primary_kind)
            .fold(HashMap::<String, usize>::new(), |mut counts, event| {
                *counts.entry(event.pubkey.to_hex()).or_default() += 1;
                counts
            })
            .into_iter()
            .max_by_key(|(_, count)| *count)
            .map(|(pubkey, _)| pubkey)
            .expect("benchmark requires an author for the selected kind");

        let by_id_id = dataset.events[dataset.events.len() / 2].id.to_hex();

        vec![
            BenchmarkQueryCase::ById { id: by_id_id },
            BenchmarkQueryCase::ByAuthor {
                pubkey: author_pubkey.clone(),
                limit: 50,
            },
            BenchmarkQueryCase::ByAuthorKind {
                pubkey: author_pubkey,
                kind: primary_kind_u32,
                limit: 50,
            },
            BenchmarkQueryCase::ByKind {
                kind: primary_kind_u32,
                limit: 200,
            },
            BenchmarkQueryCase::ByTag {
                tag_name: dataset.guaranteed_tag_name.clone(),
                tag_value: dataset.guaranteed_tag_value.clone(),
                limit: 100,
            },
            BenchmarkQueryCase::Recent { limit: 100 },
            BenchmarkQueryCase::Replaceable {
                pubkey: dataset.replaceable_pubkey.clone(),
                kind: dataset.replaceable_kind,
            },
            BenchmarkQueryCase::ParameterizedReplaceable {
                pubkey: dataset.parameterized_pubkey.clone(),
                kind: dataset.parameterized_kind,
                d_tag: dataset.parameterized_d_tag.clone(),
            },
        ]
    }

    fn benchmark_warm_query_case<S: Store + 'static>(
        base: Arc<S>,
        root: &Cid,
        order: usize,
        case: &BenchmarkQueryCase,
        iterations: usize,
    ) -> QueryBenchmarkResult {
        let trace_store = Arc::new(CountingStore::new(base));
        let event_store = NostrEventStore::with_options(
            Arc::clone(&trace_store),
            NostrEventStoreOptions {
                btree_order: Some(order),
            },
        );
        let mut durations = Vec::with_capacity(iterations);
        let mut traces = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            trace_store.reset();
            let started = Instant::now();
            let matches = block_on(case.execute(&event_store, root)).unwrap();
            durations.push(started.elapsed());
            traces.push(trace_store.snapshot());
            assert!(
                matches > 0,
                "benchmark query {} returned no matches",
                case.name()
            );
        }
        let mut sorted = durations.clone();
        sorted.sort_unstable();
        QueryBenchmarkResult {
            average_duration: average_duration(&durations),
            p95_duration: duration_percentile(&sorted, 95, 100),
            reads: average_read_trace(&traces),
        }
    }

    fn benchmark_cold_query_case<S: Store + 'static>(
        remote: Arc<S>,
        root: &Cid,
        order: usize,
        case: &BenchmarkQueryCase,
        iterations: usize,
    ) -> QueryBenchmarkResult {
        let mut durations = Vec::with_capacity(iterations);
        let mut traces = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let cache = Arc::new(MemoryStore::new());
            let trace_store = Arc::new(ReadThroughStore::new(cache, Arc::clone(&remote)));
            let event_store = NostrEventStore::with_options(
                Arc::clone(&trace_store),
                NostrEventStoreOptions {
                    btree_order: Some(order),
                },
            );
            let started = Instant::now();
            let matches = block_on(case.execute(&event_store, root)).unwrap();
            durations.push(started.elapsed());
            traces.push(trace_store.snapshot());
            assert!(
                matches > 0,
                "benchmark query {} returned no matches",
                case.name()
            );
        }
        let mut sorted = durations.clone();
        sorted.sort_unstable();
        QueryBenchmarkResult {
            average_duration: average_duration(&durations),
            p95_duration: duration_percentile(&sorted, 95, 100),
            reads: average_read_trace(&traces),
        }
    }

    fn duration_percentile(
        sorted: &[std::time::Duration],
        numerator: usize,
        denominator: usize,
    ) -> std::time::Duration {
        if sorted.is_empty() {
            return std::time::Duration::ZERO;
        }
        let index = ((sorted.len() - 1) * numerator) / denominator;
        sorted[index]
    }

    #[test]
    #[ignore = "benchmark"]
    fn benchmark_query_events_large_dataset() {
        let _guard = test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store =
            open_social_graph_store_with_mapsize(tmp.path(), Some(512 * 1024 * 1024)).unwrap();
        set_nostr_profile_enabled(true);
        reset_nostr_profile();

        let author_count = 64usize;
        let measured_event_count = std::env::var("HASHTREE_BENCH_EVENTS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(600usize);
        let warmup_event_count = benchmark_stream_warmup_events(measured_event_count);
        let total_event_count = warmup_event_count + measured_event_count;
        let (source, events) = load_benchmark_events(total_event_count, author_count).unwrap();
        let loaded_event_count = events.len();
        let warmup_event_count = warmup_event_count.min(loaded_event_count.saturating_sub(1));
        let (warmup_events, measured_events) = events.split_at(warmup_event_count);

        println!(
            "starting steady-state dataset benchmark with {} warmup events and {} measured stream events from {}",
            warmup_events.len(),
            measured_events.len(),
            source
        );
        if !warmup_events.is_empty() {
            ingest_parsed_events(&graph_store, warmup_events).unwrap();
        }

        let stream_start = Instant::now();
        let mut per_event_latencies = Vec::with_capacity(measured_events.len());
        for event in measured_events {
            let event_start = Instant::now();
            ingest_parsed_event(&graph_store, event).unwrap();
            per_event_latencies.push(event_start.elapsed());
        }
        let ingest_duration = stream_start.elapsed();

        let mut sorted_latencies = per_event_latencies.clone();
        sorted_latencies.sort_unstable();
        let average_latency = if per_event_latencies.is_empty() {
            std::time::Duration::ZERO
        } else {
            std::time::Duration::from_secs_f64(
                per_event_latencies
                    .iter()
                    .map(std::time::Duration::as_secs_f64)
                    .sum::<f64>()
                    / per_event_latencies.len() as f64,
            )
        };
        let ingest_capacity_eps = if ingest_duration.is_zero() {
            f64::INFINITY
        } else {
            measured_events.len() as f64 / ingest_duration.as_secs_f64()
        };
        println!(
            "benchmark steady-state ingest complete in {:?} (avg={:?} p50={:?} p95={:?} p99={:?} capacity={:.2} events/s)",
            ingest_duration,
            average_latency,
            duration_percentile(&sorted_latencies, 50, 100),
            duration_percentile(&sorted_latencies, 95, 100),
            duration_percentile(&sorted_latencies, 99, 100),
            ingest_capacity_eps
        );
        let mut profile = take_nostr_profile();
        profile.sort_by(|left, right| right.total.cmp(&left.total));
        for stat in profile {
            let pct = if ingest_duration.is_zero() {
                0.0
            } else {
                (stat.total.as_secs_f64() / ingest_duration.as_secs_f64()) * 100.0
            };
            let average = if stat.count == 0 {
                std::time::Duration::ZERO
            } else {
                std::time::Duration::from_secs_f64(stat.total.as_secs_f64() / stat.count as f64)
            };
            println!(
                "ingest profile: label={} total={:?} pct={:.1}% count={} avg={:?} max={:?}",
                stat.label, stat.total, pct, stat.count, average, stat.max
            );
        }
        set_nostr_profile_enabled(false);

        let kind = events
            .iter()
            .find(|event| event.kind == Kind::TextNote)
            .map(|event| event.kind)
            .or_else(|| events.first().map(|event| event.kind))
            .expect("benchmark requires at least one event");
        let kind_filter = Filter::new().kind(kind);
        let kind_start = Instant::now();
        let kind_events = query_events(&graph_store, &kind_filter, 200);
        let kind_duration = kind_start.elapsed();
        assert_eq!(
            kind_events.len(),
            benchmark_match_count(&events, &kind_filter, 200)
        );
        assert!(kind_events
            .windows(2)
            .all(|window| window[0].created_at >= window[1].created_at));

        let author_pubkey = events
            .iter()
            .find(|event| event.kind == kind)
            .map(|event| event.pubkey)
            .expect("benchmark requires an author for the selected kind");
        let author_filter = Filter::new().author(author_pubkey).kind(kind);
        let author_start = Instant::now();
        let author_events = query_events(&graph_store, &author_filter, 50);
        let author_duration = author_start.elapsed();
        assert_eq!(
            author_events.len(),
            benchmark_match_count(&events, &author_filter, 50)
        );

        let tag_filter = events
            .iter()
            .find_map(first_tag_filter)
            .expect("benchmark requires at least one tagged event");
        let tag_start = Instant::now();
        let tag_events = query_events(&graph_store, &tag_filter, 100);
        let tag_duration = tag_start.elapsed();
        assert_eq!(
            tag_events.len(),
            benchmark_match_count(&events, &tag_filter, 100)
        );

        let search_source = events
            .iter()
            .find_map(|event| first_search_term(event).map(|term| (event.kind, term)))
            .expect("benchmark requires at least one searchable event");
        let search_filter = Filter::new().kind(search_source.0).search(search_source.1);
        let search_start = Instant::now();
        let search_events = query_events(&graph_store, &search_filter, 100);
        let search_duration = search_start.elapsed();
        assert_eq!(
            search_events.len(),
            benchmark_match_count(&events, &search_filter, 100)
        );

        println!(
            "steady-state benchmark: source={} warmup_events={} stream_events={} ingest={:?} avg={:?} p50={:?} p95={:?} p99={:?} capacity_eps={:.2} kind={:?} author={:?} tag={:?} search={:?}",
            source,
            warmup_events.len(),
            measured_events.len(),
            ingest_duration,
            average_latency,
            duration_percentile(&sorted_latencies, 50, 100),
            duration_percentile(&sorted_latencies, 95, 100),
            duration_percentile(&sorted_latencies, 99, 100),
            ingest_capacity_eps,
            kind_duration,
            author_duration,
            tag_duration,
            search_duration
        );
    }

    #[test]
    #[ignore = "benchmark"]
    fn benchmark_nostr_btree_query_tradeoffs() {
        let _guard = test_lock();
        let event_count = std::env::var("HASHTREE_BENCH_EVENTS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(2_000usize);
        let iterations = benchmark_read_iterations();
        let orders = benchmark_btree_orders();
        let dataset = load_index_benchmark_dataset(event_count, 64).unwrap();
        let cases = build_btree_query_cases(&dataset);
        let stored_events = dataset
            .events
            .iter()
            .map(stored_event_from_nostr)
            .collect::<Vec<_>>();

        println!(
            "btree-order benchmark: source={} events={} iterations={} orders={:?}",
            dataset.source,
            stored_events.len(),
            iterations,
            orders
        );
        println!(
            "network models are serialized fetch estimates: {}",
            NETWORK_MODELS
                .iter()
                .map(|model| format!(
                    "{}={}ms_rtt/{}MiBps",
                    model.name, model.rtt_ms, model.bandwidth_mib_per_s
                ))
                .collect::<Vec<_>>()
                .join(", ")
        );

        for order in orders {
            let tmp = TempDir::new().unwrap();
            let local_store =
                Arc::new(LocalStore::new(tmp.path().join("blobs"), &StorageBackend::Lmdb).unwrap());
            let event_store = NostrEventStore::with_options(
                Arc::clone(&local_store),
                NostrEventStoreOptions {
                    btree_order: Some(order),
                },
            );
            let root = block_on(event_store.build(None, stored_events.clone()))
                .unwrap()
                .expect("benchmark build root");

            println!("btree-order={} root={}", order, hex::encode(root.hash));
            let mut warm_total_ms = 0.0f64;
            let mut model_totals = NETWORK_MODELS
                .iter()
                .map(|model| (model.name, 0.0f64))
                .collect::<HashMap<_, _>>();

            for case in &cases {
                let warm = benchmark_warm_query_case(
                    Arc::clone(&local_store),
                    &root,
                    order,
                    case,
                    iterations,
                );
                let cold = benchmark_cold_query_case(
                    Arc::clone(&local_store),
                    &root,
                    order,
                    case,
                    iterations,
                );
                warm_total_ms += warm.average_duration.as_secs_f64() * 1000.0;

                let model_estimates = NETWORK_MODELS
                    .iter()
                    .map(|model| {
                        let estimate = estimate_serialized_remote_ms(&cold.reads, *model);
                        *model_totals.get_mut(model.name).unwrap() += estimate;
                        format!("{}={:.2}ms", model.name, estimate)
                    })
                    .collect::<Vec<_>>()
                    .join(" ");

                println!(
                    "btree-order={} query={} warm_avg={:?} warm_p95={:?} warm_blocks={} warm_unique_bytes={} cold_fetches={} cold_bytes={} cold_local_avg={:?} {}",
                    order,
                    case.name(),
                    warm.average_duration,
                    warm.p95_duration,
                    warm.reads.unique_blocks,
                    warm.reads.unique_bytes,
                    cold.reads.remote_fetches,
                    cold.reads.remote_bytes,
                    cold.average_duration,
                    model_estimates
                );
            }

            println!(
                "btree-order={} summary unweighted_warm_avg_ms={:.3} {}",
                order,
                warm_total_ms / cases.len() as f64,
                NETWORK_MODELS
                    .iter()
                    .map(|model| format!(
                        "{}={:.2}ms",
                        model.name,
                        model_totals[model.name] / cases.len() as f64
                    ))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
    }

    #[test]
    fn test_ensure_social_graph_mapsize_rounds_and_applies() {
        let _guard = test_lock();
        let tmp = TempDir::new().unwrap();
        ensure_social_graph_mapsize(tmp.path(), DEFAULT_SOCIALGRAPH_MAP_SIZE_BYTES).unwrap();
        let requested = 70 * 1024 * 1024;
        ensure_social_graph_mapsize(tmp.path(), requested).unwrap();
        let env = unsafe {
            heed::EnvOpenOptions::new()
                .map_size(DEFAULT_SOCIALGRAPH_MAP_SIZE_BYTES as usize)
                .max_dbs(SOCIALGRAPH_MAX_DBS)
                .open(tmp.path())
        }
        .unwrap();
        assert!(env.info().map_size >= requested as usize);
        assert_eq!(env.info().map_size % page_size_bytes(), 0);
    }

    #[test]
    fn test_ingest_events_batches_graph_updates() {
        let _guard = test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = open_social_graph_store(tmp.path()).unwrap();

        let root_keys = Keys::generate();
        let alice_keys = Keys::generate();
        let bob_keys = Keys::generate();

        let root_pk = root_keys.public_key().to_bytes();
        set_social_graph_root(&graph_store, &root_pk);

        let root_follows_alice = EventBuilder::new(
            Kind::ContactList,
            "",
            vec![Tag::public_key(alice_keys.public_key())],
        )
        .custom_created_at(Timestamp::from_secs(10))
        .to_event(&root_keys)
        .unwrap();
        let alice_follows_bob = EventBuilder::new(
            Kind::ContactList,
            "",
            vec![Tag::public_key(bob_keys.public_key())],
        )
        .custom_created_at(Timestamp::from_secs(11))
        .to_event(&alice_keys)
        .unwrap();

        ingest_parsed_events(
            &graph_store,
            &[root_follows_alice.clone(), alice_follows_bob.clone()],
        )
        .unwrap();

        assert_eq!(
            get_follow_distance(&graph_store, &alice_keys.public_key().to_bytes()),
            Some(1)
        );
        assert_eq!(
            get_follow_distance(&graph_store, &bob_keys.public_key().to_bytes()),
            Some(2)
        );

        let filter = Filter::new().kind(Kind::ContactList);
        let stored = query_events(&graph_store, &filter, 10);
        let ids = stored.into_iter().map(|event| event.id).collect::<Vec<_>>();
        assert!(ids.contains(&root_follows_alice.id));
        assert!(ids.contains(&alice_follows_bob.id));
    }

    #[test]
    fn test_ingest_graph_events_updates_graph_without_indexing_events() {
        let _guard = test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = open_social_graph_store(tmp.path()).unwrap();

        let root_keys = Keys::generate();
        let alice_keys = Keys::generate();

        let root_pk = root_keys.public_key().to_bytes();
        set_social_graph_root(&graph_store, &root_pk);

        let root_follows_alice = EventBuilder::new(
            Kind::ContactList,
            "",
            vec![Tag::public_key(alice_keys.public_key())],
        )
        .custom_created_at(Timestamp::from_secs(10))
        .to_event(&root_keys)
        .unwrap();

        ingest_graph_parsed_events(&graph_store, std::slice::from_ref(&root_follows_alice))
            .unwrap();

        assert_eq!(
            get_follow_distance(&graph_store, &alice_keys.public_key().to_bytes()),
            Some(1)
        );
        let filter = Filter::new().kind(Kind::ContactList);
        assert!(query_events(&graph_store, &filter, 10).is_empty());
    }
}
