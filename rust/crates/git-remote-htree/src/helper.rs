//! Git remote helper protocol implementation
//!
//! Implements the stateless git remote helper protocol.
//! See: https://git-scm.com/docs/gitremote-helpers

use crate::git::object::ObjectType;
use crate::git::refs::Ref;
use crate::git::storage::GitStorage;
use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Threshold for showing detailed progress (3 seconds)
const VERBOSE_THRESHOLD: Duration = Duration::from_secs(3);
/// Number of old-tree hashes to probe per server before deciding whether an
/// incremental push can safely skip unchanged content.
const SERVER_COVERAGE_SAMPLE_SIZE: usize = 32;

use crate::nostr_client::{BlossomResult, NostrClient, PullRequestStateFilter};
use hashtree_config::Config;

// CachedStore: local store first, then Blossom fallback
mod cached_store {
    use hashtree_blossom::BlossomStore;
    use hashtree_core::{Hash, Store, StoreError};
    use std::sync::Arc;

    pub struct CachedStore {
        local: Arc<dyn Store + Send + Sync>,
        blossom: BlossomStore,
    }

    impl CachedStore {
        pub fn new(local: Arc<dyn Store + Send + Sync>, blossom: BlossomStore) -> Self {
            Self { local, blossom }
        }
    }

    #[async_trait::async_trait]
    impl Store for CachedStore {
        async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
            // Store locally
            self.local.put(hash, data).await
        }

        async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
            // Try local first
            if let Ok(Some(data)) = self.local.get(hash).await {
                return Ok(Some(data));
            }
            // Fallback to Blossom
            let result = self.blossom.get(hash).await;
            // Cache locally if found
            if let Ok(Some(ref data)) = result {
                let _ = self.local.put(*hash, data.clone()).await;
            }
            result
        }

        async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
            // Check local first
            if self.local.has(hash).await? {
                return Ok(true);
            }
            // Fallback to Blossom
            self.blossom.has(hash).await
        }

        async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
            // Delete from local only (don't delete from remote)
            self.local.delete(hash).await
        }
    }
}

/// Get the shared hashtree data directory
fn get_hashtree_data_dir() -> PathBuf {
    hashtree_config::get_data_dir()
}

fn format_upload_progress(
    processed: usize,
    total: usize,
    uploaded: usize,
    skipped_diff: usize,
    skipped_server: usize,
    failed: usize,
    has_old_tree: bool,
) -> String {
    let total = total.max(processed);
    if has_old_tree {
        if failed > 0 {
            format!(
                "  Uploading: {}/{} ({} new, {} unchanged, {} exist, {} FAILED)",
                processed, total, uploaded, skipped_diff, skipped_server, failed
            )
        } else {
            format!(
                "  Uploading: {}/{} ({} new, {} unchanged, {} exist)",
                processed, total, uploaded, skipped_diff, skipped_server
            )
        }
    } else if failed > 0 {
        format!(
            "  Uploading: {}/{} ({} new, {} exist, {} FAILED)",
            processed, total, uploaded, skipped_server, failed
        )
    } else {
        format!(
            "  Uploading: {}/{} ({} new, {} exist)",
            processed, total, uploaded, skipped_server
        )
    }
}

fn format_upload_progress_discovering(
    processed: usize,
    discovered: usize,
    uploaded: usize,
    skipped_diff: usize,
    skipped_server: usize,
    failed: usize,
    has_old_tree: bool,
) -> String {
    if has_old_tree {
        if failed > 0 {
            format!(
                "  Uploading: {}/? ({} discovered, {} new, {} unchanged, {} exist, {} FAILED)",
                processed, discovered, uploaded, skipped_diff, skipped_server, failed
            )
        } else {
            format!(
                "  Uploading: {}/? ({} discovered, {} new, {} unchanged, {} exist)",
                processed, discovered, uploaded, skipped_diff, skipped_server
            )
        }
    } else if failed > 0 {
        format!(
            "  Uploading: {}/? ({} discovered, {} new, {} exist, {} FAILED)",
            processed, discovered, uploaded, skipped_server, failed
        )
    } else {
        format!(
            "  Uploading: {}/? ({} discovered, {} new, {} exist)",
            processed, discovered, uploaded, skipped_server
        )
    }
}

fn emit_upload_progress(
    processed: usize,
    discovered: usize,
    total: Option<usize>,
    uploaded: usize,
    skipped_diff: usize,
    skipped_server: usize,
    failed: usize,
    has_old_tree: bool,
) {
    let line = if let Some(total) = total {
        format_upload_progress(
            processed,
            total,
            uploaded,
            skipped_diff,
            skipped_server,
            failed,
            has_old_tree,
        )
    } else {
        format_upload_progress_discovering(
            processed,
            discovered,
            uploaded,
            skipped_diff,
            skipped_server,
            failed,
            has_old_tree,
        )
    };
    eprint!("\r{}", line);
    let _ = std::io::stderr().flush();
}

fn queue_hash_if_new(
    queue: &mut Vec<([u8; 32], Option<[u8; 32]>)>,
    queued: &mut HashSet<[u8; 32]>,
    hash: [u8; 32],
    key: Option<[u8; 32]>,
) -> bool {
    if queued.insert(hash) {
        queue.push((hash, key));
        true
    } else {
        false
    }
}

/// Create local blob store based on config
fn create_local_store(
    path: &std::path::Path,
) -> Result<std::sync::Arc<dyn hashtree_core::Store + Send + Sync>> {
    use hashtree_config::StorageBackend;
    use hashtree_fs::FsBlobStore;

    let config = Config::load_or_default();
    let max_size_bytes = config
        .storage
        .max_size_gb
        .saturating_mul(1024 * 1024 * 1024);
    match config.storage.backend {
        StorageBackend::Fs => {
            if max_size_bytes > 0 {
                Ok(std::sync::Arc::new(FsBlobStore::with_max_bytes(
                    path,
                    max_size_bytes,
                )?))
            } else {
                Ok(std::sync::Arc::new(FsBlobStore::new(path)?))
            }
        }
        #[cfg(feature = "lmdb")]
        StorageBackend::Lmdb => Ok(std::sync::Arc::new(if max_size_bytes > 0 {
            hashtree_lmdb::LmdbBlobStore::with_max_bytes(path, max_size_bytes)?
        } else {
            hashtree_lmdb::LmdbBlobStore::new(path)?
        })),
        #[cfg(not(feature = "lmdb"))]
        StorageBackend::Lmdb => {
            warn!("LMDB backend requested but lmdb feature not enabled, using filesystem storage");
            if max_size_bytes > 0 {
                Ok(std::sync::Arc::new(FsBlobStore::with_max_bytes(
                    path,
                    max_size_bytes,
                )?))
            } else {
                Ok(std::sync::Arc::new(FsBlobStore::new(path)?))
            }
        }
    }
}

fn build_repo_viewer_url(path: &str, url_secret: Option<&[u8; 32]>) -> String {
    match url_secret {
        Some(secret) => format!("https://git.iris.to/#/{}?k={}", path, hex::encode(secret)),
        None => format!("https://git.iris.to/#/{}", path),
    }
}

/// Git remote helper state machine
pub struct RemoteHelper {
    #[allow(dead_code)]
    pubkey: String,
    repo_name: String,
    storage: GitStorage,
    nostr: NostrClient,
    #[allow(dead_code)]
    config: Config,
    should_exit: bool,
    /// Refs advertised by remote
    remote_refs: HashMap<String, String>,
    /// Objects to push
    push_specs: Vec<PushSpec>,
    /// Objects to fetch
    fetch_specs: Vec<FetchSpec>,
    /// Secret key from URL fragment #k=<hex> (for link-visible repos)
    /// If set, use this for encryption instead of CHK, and don't publish key in event
    url_secret: Option<[u8; 32]>,
    /// Whether this is a private (author-only) repo using NIP-44 encryption
    is_private: bool,
    /// Start time for current operation (for conditional verbose logging)
    op_start: Option<Instant>,
}

#[derive(Debug)]
struct PushSpec {
    src: String, // local ref or sha
    dst: String, // remote ref
    force: bool,
}

#[derive(Debug)]
struct FetchSpec {
    sha: String,
    name: String,
}

#[derive(Debug, PartialEq, Eq)]
enum AncestorCheck {
    /// Remote tip is an ancestor of local tip: fast-forward allowed.
    Ancestor,
    /// Remote tip is not an ancestor of local tip: true non-fast-forward.
    NotAncestor,
    /// We could not determine ancestry (merge-base command/object failure).
    Unknown(String),
}

impl RemoteHelper {
    pub fn new(
        pubkey: &str,
        repo_name: &str,
        signing_key: Option<String>,
        url_secret: Option<[u8; 32]>,
        is_private: bool,
        config: Config,
    ) -> Result<Self> {
        // Use shared hashtree storage at ~/.hashtree/data
        let data_dir = get_hashtree_data_dir();
        debug!(?data_dir, "RemoteHelper::new");
        let storage = GitStorage::open(&data_dir)?;
        let nostr = NostrClient::new(pubkey, signing_key, url_secret, is_private, &config)?;

        if is_private {
            info!("Private repo: using NIP-44 encryption (author-only)");
        } else if url_secret.is_some() {
            info!("Link-visible repo: using secret from URL fragment");
        }

        Ok(Self {
            pubkey: pubkey.to_string(),
            repo_name: repo_name.to_string(),
            storage,
            nostr,
            config,
            should_exit: false,
            remote_refs: HashMap::new(),
            push_specs: Vec::new(),
            fetch_specs: Vec::new(),
            url_secret,
            is_private,
            op_start: None,
        })
    }

    pub fn should_exit(&self) -> bool {
        self.should_exit
    }

    /// Start timing an operation (for conditional verbose logging)
    fn start_op(&mut self) {
        self.op_start = Some(Instant::now());
    }

    /// Check if operation has been running long enough to show details
    /// Also returns true if HTREE_VERBOSE=1 is set (for testing/debugging)
    fn is_slow(&self) -> bool {
        if std::env::var("HTREE_VERBOSE").is_ok() {
            return true;
        }
        self.op_start
            .map(|start| start.elapsed() >= VERBOSE_THRESHOLD)
            .unwrap_or(false)
    }

    /// Log detail message only if operation is slow
    fn detail(&self, msg: &str) {
        if self.is_slow() {
            eprintln!("{}", msg);
        }
    }

    /// Handle a single command from git
    pub fn handle_command(&mut self, line: &str) -> Result<Option<Vec<String>>> {
        let parts: Vec<&str> = line.splitn(2, ' ').collect();
        let cmd = parts[0];
        let arg = parts.get(1).copied();

        match cmd {
            "capabilities" => Ok(Some(self.capabilities())),
            "list" => {
                let for_push = arg == Some("for-push");
                self.list_refs(for_push)
            }
            "fetch" => {
                if let Some(arg) = arg {
                    self.queue_fetch(arg)?;
                }
                Ok(None)
            }
            "push" => {
                if let Some(arg) = arg {
                    self.queue_push(arg)?;
                }
                Ok(None)
            }
            "" => {
                // Empty line - execute queued operations
                if !self.fetch_specs.is_empty() {
                    self.execute_fetch()?;
                }
                if !self.push_specs.is_empty() {
                    return self.execute_push();
                }
                // Final empty line means exit
                self.should_exit = true;
                Ok(Some(vec![String::new()]))
            }
            "option" => {
                // Options like "option verbosity 1"
                if let Some(arg) = arg {
                    let mut parts = arg.split_whitespace();
                    let name = parts.next().unwrap_or("");
                    if name == "update-head-ok" {
                        return Ok(Some(vec!["ok".to_string()]));
                    }
                    if name == "progress" || name == "verbosity" {
                        return Ok(Some(vec!["ok".to_string()]));
                    }
                }
                debug!("Ignoring option: {:?}", arg);
                Ok(Some(vec!["unsupported".to_string()]))
            }
            _ => {
                warn!("Unknown command: {}", cmd);
                Ok(None)
            }
        }
    }

    /// Return supported capabilities
    fn capabilities(&self) -> Vec<String> {
        vec![
            "fetch".to_string(),
            "push".to_string(),
            "option".to_string(),
            String::new(), // Empty line terminates
        ]
    }

    /// List refs available on remote
    fn list_refs(&mut self, for_push: bool) -> Result<Option<Vec<String>>> {
        // For push, always return empty refs to force re-push
        // This ensures content is always re-uploaded to blossom servers
        // and we regenerate the index file each time
        if for_push {
            debug!("Returning empty refs for push to force re-upload");
            self.remote_refs.clear();
            return Ok(Some(vec![String::new()]));
        }

        // For clone/pull, fetch actual refs from nostr
        self.remote_refs.clear();
        let refs = self.nostr.fetch_refs(&self.repo_name)?;

        let mut lines = Vec::new();

        for (name, sha) in &refs {
            self.remote_refs.insert(name.clone(), sha.clone());
            if name == "HEAD" {
                // HEAD can be a symref or a direct SHA.
                if let Some(target_branch) = sha.strip_prefix("ref: ") {
                    lines.push(format!("@{} HEAD", target_branch));
                } else {
                    lines.push(format!("{} HEAD", sha));
                }
            } else {
                lines.push(format!("{} {}", sha, name));
            }
        }

        // Empty repo
        if lines.is_empty() {
            debug!("Remote has no refs");
        }

        lines.push(String::new()); // Empty line terminates
        Ok(Some(lines))
    }

    /// Queue a fetch operation
    fn queue_fetch(&mut self, arg: &str) -> Result<()> {
        // Format: <sha> <name>
        let parts: Vec<&str> = arg.splitn(2, ' ').collect();
        if parts.len() != 2 {
            bail!("Invalid fetch spec: {}", arg);
        }

        self.fetch_specs.push(FetchSpec {
            sha: parts[0].to_string(),
            name: parts[1].to_string(),
        });
        Ok(())
    }

    /// Execute queued fetch operations
    fn execute_fetch(&mut self) -> Result<()> {
        self.start_op(); // Start timing for conditional verbose logging
        info!("Fetching {} refs", self.fetch_specs.len());
        for spec in &self.fetch_specs {
            debug!(sha = %spec.sha, name = %spec.name, "Queued fetch");
        }

        // Get the cached root hash from nostr (set during list command)
        let root_hash = self.nostr.get_cached_root_hash(&self.repo_name).cloned();

        if let Some(ref root) = root_hash {
            // Fetch all git objects from the hashtree structure
            let objects = self.fetch_all_git_objects(root)?;
            info!("Loaded {} git objects from hashtree", objects.len());

            // Batch check which objects git already has
            let existing =
                self.git_batch_check_objects(objects.iter().map(|(oid, _)| oid.as_str()))?;

            // Filter to only objects git doesn't have
            let to_write: Vec<_> = objects
                .into_iter()
                .filter(|(oid, _)| !existing.contains(oid))
                .collect();

            let total = to_write.len();
            let skipped = existing.len();

            if total == 0 {
                eprintln!("  Writing to .git: 0 new, {} cached    ", skipped);
            } else {
                for (i, (oid, data)) in to_write.into_iter().enumerate() {
                    self.write_git_object(&oid, &data)?;
                    let count = i + 1;
                    if count % 50 == 0 || count == total || count == 1 {
                        eprint!("\r  Writing to .git: {}/{}    ", count, total);
                        let _ = std::io::stderr().flush();
                    }
                }
                if skipped > 0 {
                    eprintln!("\r  Writing to .git: {} new, {} cached    ", total, skipped);
                } else {
                    eprintln!("\r  Writing to .git: {}/{}    ", total, total);
                }
            }
        } else {
            bail!("No root hash found for repository - cannot fetch");
        }

        self.fetch_specs.clear();
        Ok(())
    }

    /// Fetch all git objects from hashtree's .git/objects/ directory
    fn fetch_all_git_objects(&self, root_hash: &str) -> Result<Vec<(String, Vec<u8>)>> {
        // NostrClient now handles unmasking for link-visible repos (url_secret)
        // The cached key is already the real CHK key
        let encryption_key = self
            .nostr
            .get_cached_encryption_key(&self.repo_name)
            .cloned();

        info!(
            "fetch_all_git_objects: root={}, has encryption_key: {}, link_visible: {}",
            &root_hash[..12],
            encryption_key.is_some(),
            self.url_secret.is_some()
        );

        // Create tokio runtime for async blossom downloads
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("Failed to create tokio runtime")?;

        rt.block_on(self.fetch_git_objects_async(root_hash, encryption_key.as_ref()))
    }

    /// Async implementation of git object fetching using HashTree helpers
    async fn fetch_git_objects_async(
        &self,
        root_hash: &str,
        encryption_key: Option<&[u8; 32]>,
    ) -> Result<Vec<(String, Vec<u8>)>> {
        use hashtree_blossom::BlossomStore;
        use hashtree_core::{Cid, HashTree, HashTreeConfig};

        let blossom = self.nostr.blossom();
        let mut objects = Vec::new();

        // Log the servers being used
        let servers = blossom.read_servers().to_vec();
        info!(
            "Creating CachedStore with local + Blossom (servers: {:?})",
            servers
        );

        // Create local blob store based on config
        let data_dir = get_hashtree_data_dir();
        let blobs_path = data_dir.join("blobs");
        let local_store =
            create_local_store(&blobs_path).context("Failed to create local blob store")?;
        let local_store_for_eviction = local_store.clone();

        // Create Blossom store for remote fallback
        let blossom_store = BlossomStore::with_servers(
            nostr::Keys::generate(), // Temporary keys for read-only ops
            servers,
        );

        // Create cached store: local first, then Blossom
        let store = cached_store::CachedStore::new(local_store, blossom_store);
        let tree = HashTree::new(HashTreeConfig::new(std::sync::Arc::new(store)));

        // Parse root hash and create Cid with encryption key
        let root_bytes = hex::decode(root_hash).context("Invalid root hash hex")?;
        let root_arr: [u8; 32] = root_bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("Root hash must be 32 bytes"))?;

        let root_cid = Cid {
            hash: root_arr,
            key: encryption_key.copied(),
        };

        // Resolve .git/objects path
        let objects_cid = match tree.resolve_path(&root_cid, ".git/objects").await {
            Ok(Some(cid)) => cid,
            Ok(None) => {
                warn!("No .git/objects directory found");
                return Ok(objects);
            }
            Err(e) => {
                warn!("Failed to resolve .git/objects: {}", e);
                return Ok(objects);
            }
        };

        info!("Resolved .git/objects: {}", hex::encode(objects_cid.hash));

        use futures::stream::{self, StreamExt};
        use hashtree_core::LinkType;
        use std::io::Write;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc as StdArc;

        // Walk the objects tree with parallel fetching and progress reporting
        let progress = StdArc::new(AtomicUsize::new(0));
        let done = StdArc::new(AtomicBool::new(false));

        // Spawn progress reporter
        let progress_clone = progress.clone();
        let done_clone = done.clone();
        let progress_task = tokio::spawn(async move {
            let mut last = 0;
            loop {
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                if done_clone.load(Ordering::Relaxed) {
                    break;
                }
                let current = progress_clone.load(Ordering::Relaxed);
                if current != last {
                    eprint!("\r  Loading objects tree... {} nodes", current);
                    let _ = std::io::stderr().flush();
                    last = current;
                }
            }
        });

        const WALK_CONCURRENCY: usize = 32;
        let walk_entries = match tree
            .walk_parallel_with_progress(&objects_cid, "", WALK_CONCURRENCY, Some(&progress))
            .await
        {
            Ok(entries) => entries,
            Err(e) => {
                done.store(true, Ordering::Relaxed);
                let _ = progress_task.await;
                eprintln!("\r  Loading objects tree... failed: {}", e);
                warn!("Failed to walk objects directory: {}", e);
                return Ok(objects);
            }
        };
        done.store(true, Ordering::Relaxed);
        let _ = progress_task.await;
        let walk_done_time = std::time::Instant::now();
        if self.is_slow() {
            eprintln!(
                "\r  Loading objects tree... done ({} entries)        ",
                walk_entries.len()
            );
        } else {
            eprint!("\r                                                        \r");
            // Clear the line
        }

        // Extract git objects from walk entries (files with 40 char hex names like "ab/cdef..." -> "abcdef...")
        let mut fetch_tasks: Vec<(String, Cid)> = Vec::new();
        for entry in walk_entries {
            // Skip directories
            if entry.link_type == LinkType::Dir {
                continue;
            }

            // Parse path like "ab/cdef1234..." into oid "abcdef1234..."
            let parts: Vec<&str> = entry.path.split('/').collect();
            if parts.len() == 2 && parts[0].len() == 2 && parts[1].len() == 38 {
                if hex::decode(parts[0]).is_ok() && hex::decode(parts[1]).is_ok() {
                    let oid = format!("{}{}", parts[0], parts[1]);
                    let obj_cid = Cid {
                        hash: entry.hash,
                        key: entry.key,
                    };
                    fetch_tasks.push((oid, obj_cid));
                }
            } else if parts.len() == 1 && parts[0].len() == 40 {
                // Flat layout: object files directly in objects/
                if hex::decode(parts[0]).is_ok() {
                    let oid = parts[0].to_string();
                    let obj_cid = Cid {
                        hash: entry.hash,
                        key: entry.key,
                    };
                    fetch_tasks.push((oid, obj_cid));
                }
            }
        }

        let total_objects = fetch_tasks.len();
        let prep_elapsed = walk_done_time.elapsed();
        if self.is_slow() {
            eprintln!("  Prepared {} objects in {:?}", total_objects, prep_elapsed);
        }

        let downloaded = StdArc::new(AtomicUsize::new(0));
        let download_done = StdArc::new(AtomicBool::new(false));

        // Spawn progress reporter
        let downloaded_clone = downloaded.clone();
        let download_done_clone = download_done.clone();
        let total_for_timer = total_objects;
        let timer_task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                if download_done_clone.load(Ordering::Relaxed) {
                    break;
                }
                let count = downloaded_clone.load(Ordering::Relaxed);
                eprint!("\r  Loading: {}/{}    ", count, total_for_timer);
                let _ = std::io::stderr().flush();
            }
        });

        // Parallel fetch with concurrency limit
        const CONCURRENCY: usize = 20;
        type FetchObjectResult = std::result::Result<(String, Vec<u8>), (String, Cid)>;

        // First pass: fetch all objects with normal timeout
        let results: Vec<FetchObjectResult> = stream::iter(fetch_tasks)
            .map(|(oid, obj_cid)| {
                let tree = &tree;
                let downloaded = StdArc::clone(&downloaded);
                async move {
                    let result = match tree.get(&obj_cid, None).await {
                        Ok(Some(content)) => Ok((oid, content)),
                        Ok(None) => Err((oid, obj_cid)),
                        Err(_) => Err((oid, obj_cid)),
                    };
                    downloaded.fetch_add(1, Ordering::Relaxed);
                    result
                }
            })
            .buffer_unordered(CONCURRENCY)
            .collect()
            .await;

        download_done.store(true, Ordering::Relaxed);
        let _ = timer_task.await;

        // Collect successes and failures
        let mut failed: Vec<(String, Cid)> = Vec::new();
        for result in results {
            match result {
                Ok((oid, content)) => objects.push((oid, content)),
                Err((oid, cid)) => failed.push((oid, cid)),
            }
        }

        let success_count = objects.len();
        eprintln!("\r  Loading: {}/{}    ", success_count, total_objects);

        // Retry failed downloads sequentially
        let mut missing_objects: Vec<(String, String)> = Vec::new(); // (oid, hash)
        if !failed.is_empty() {
            eprintln!("  Retrying {} failed downloads...", failed.len());
            for (i, (oid, obj_cid)) in failed.iter().enumerate() {
                let hash_hex = hex::encode(obj_cid.hash);
                eprint!("\r  Retrying {}/{}: {}...    ", i + 1, failed.len(), oid);
                let _ = std::io::stderr().flush();

                match tree.get(obj_cid, None).await {
                    Ok(Some(content)) => {
                        objects.push((oid.clone(), content));
                    }
                    Ok(None) => {
                        eprintln!("\n  ERROR: Object {} not found (hash: {})", oid, hash_hex);
                        missing_objects.push((oid.clone(), hash_hex));
                    }
                    Err(e) => {
                        eprintln!(
                            "\n  ERROR: Failed to fetch {}: {} (hash: {})",
                            oid, e, hash_hex
                        );
                        missing_objects.push((oid.clone(), hash_hex));
                    }
                }
            }
            eprintln!(
                "\r  Retried: {}/{} objects available        ",
                objects.len(),
                total_objects
            );
        }

        // Fail if any objects are missing - git clone will fail anyway
        if !missing_objects.is_empty() {
            let obj_list: Vec<String> = missing_objects
                .iter()
                .take(5)
                .map(|(oid, hash)| format!("{} ({})", oid, hash))
                .collect();
            bail!(
                "Failed to fetch {} required git objects:\n  {}",
                missing_objects.len(),
                obj_list.join("\n  ")
            );
        }

        info!("Fetched {} git objects from hashtree", objects.len());
        match local_store_for_eviction.evict_if_needed().await {
            Ok(freed) if freed > 0 => {
                info!(
                    "Evicted {} bytes from shared git blob cache after fetch",
                    freed
                );
            }
            Ok(_) => {}
            Err(err) => {
                warn!("Failed to evict shared git blob cache after fetch: {}", err);
            }
        }
        Ok(objects)
    }

    /// Batch check which objects git already has (returns set of existing oids)
    fn git_batch_check_objects<'a>(
        &self,
        oids: impl Iterator<Item = &'a str>,
    ) -> Result<HashSet<String>> {
        let mut existing = HashSet::new();
        let oids: Vec<_> = oids.collect();

        // Process in chunks to avoid memory issues with huge repos
        const BATCH_SIZE: usize = 1000;
        for chunk in oids.chunks(BATCH_SIZE) {
            let mut child = Command::new("git")
                .args(["cat-file", "--batch-check=%(objectname)"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .context("Failed to spawn git cat-file")?;

            {
                let stdin = child.stdin.as_mut().context("Failed to open stdin")?;
                for oid in chunk {
                    writeln!(stdin, "{}", oid)?;
                }
            }

            let output = child
                .wait_with_output()
                .context("Failed to read git cat-file output")?;

            // Parse output - valid objects return just the oid, missing ones return "oid missing"
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let line = line.trim();
                if line.len() == 40 && !line.contains(' ') {
                    existing.insert(line.to_string());
                }
            }
        }
        Ok(existing)
    }

    /// Write loose object to local git object store.
    /// The data is zlib-compressed loose object format.
    fn write_git_object(&self, oid: &str, data: &[u8]) -> Result<()> {
        // Git objects are stored as .git/objects/xx/yy... where xx is first 2 chars
        if oid.len() < 3 {
            bail!("Invalid object id: {}", oid);
        }

        let git_dir = std::env::var("GIT_DIR").unwrap_or_else(|_| ".git".to_string());
        let (dir_name, file_name) = oid.split_at(2);
        let obj_dir = std::path::Path::new(&git_dir)
            .join("objects")
            .join(dir_name);
        std::fs::create_dir_all(&obj_dir).context("Failed to create object directory")?;

        let obj_path = obj_dir.join(file_name);
        if obj_path.exists() {
            return Ok(());
        }

        std::fs::write(&obj_path, data).context("Failed to write git object")?;
        debug!("Wrote git object {} as loose object", oid);
        Ok(())
    }

    /// Queue a push operation
    fn queue_push(&mut self, arg: &str) -> Result<()> {
        // Format: [+]<src>:<dst>
        let force = arg.starts_with('+');
        let arg = if force { &arg[1..] } else { arg };

        let parts: Vec<&str> = arg.splitn(2, ':').collect();
        if parts.len() != 2 {
            bail!("Invalid push spec: {}", arg);
        }

        self.push_specs.push(PushSpec {
            src: parts[0].to_string(),
            dst: parts[1].to_string(),
            force,
        });
        Ok(())
    }

    /// Execute queued push operations
    fn execute_push(&mut self) -> Result<Option<Vec<String>>> {
        self.start_op(); // Start timing for conditional verbose logging
        debug!(refs_count = self.push_specs.len(), "execute_push called");
        info!("Pushing {} refs", self.push_specs.len());

        // First, load existing refs and objects from remote to preserve other branches
        // Check if any push is a force push
        let has_force_push = self.push_specs.iter().any(|s| s.force);
        debug!(
            force = has_force_push,
            "About to call load_existing_remote_state"
        );

        if let Err(e) = self.load_existing_remote_state() {
            let err_str = e.to_string();

            // Check if this is an access restriction error (changing visibility modes)
            // These are expected and we should proceed with fresh state
            let is_access_error = err_str.contains("link-visible")
                || err_str.contains("private")
                || err_str.contains("secret key");

            // Check if this might be a new repo (no refs found is OK)
            let is_likely_new_repo = err_str.contains("No root hash")
                || err_str.contains("not found")
                || err_str.contains("timeout");

            if is_access_error {
                // Changing visibility mode - proceed with fresh state
                debug!("Cannot access existing repo (visibility change): {}", e);
            } else if has_force_push {
                // Force push - proceed without existing state
                eprintln!("  Warning: Could not load existing remote state: {}", e);
                eprintln!("  Proceeding with force push (may overwrite other branches)");
            } else if is_likely_new_repo {
                debug!("Error loading remote state (likely new repo): {}", e);
                info!(
                    "Could not load existing remote state: {} (likely new repo)",
                    e
                );
            } else {
                // There's an existing remote but we can't load it - warn user
                eprintln!("  Warning: Could not load existing remote state: {}", e);
                eprintln!("  Other branches may be lost. Use 'git push --force' to override.");
                eprintln!("  Or check your network connection and try again.");
            }
        }

        let mut results = Vec::new();
        let mut pushed_refs: Vec<(String, String)> = Vec::new();

        // Clone specs to avoid borrow issues
        let specs: Vec<_> = std::mem::take(&mut self.push_specs);

        for spec in specs {
            debug!(
                "Pushing {} -> {} (force={})",
                spec.src, spec.dst, spec.force
            );

            // Resolve src to sha
            let sha = if spec.src.is_empty() {
                // Delete ref
                String::new()
            } else {
                self.resolve_ref(&spec.src)?
            };

            if sha.is_empty() {
                // Delete
                match self.storage.delete_ref(&spec.dst) {
                    Ok(_) => {
                        self.nostr.delete_ref(&self.repo_name, &spec.dst)?;
                        results.push(format!("ok {}", spec.dst));
                    }
                    Err(e) => results.push(format!("error {} {}", spec.dst, e)),
                }
            } else {
                // Check for non-fast-forward push (unless force)
                if !spec.force {
                    if let Some(remote_sha) = self.remote_refs.get(&spec.dst) {
                        match self.check_ancestor(remote_sha, &sha) {
                            AncestorCheck::Ancestor => {}
                            AncestorCheck::NotAncestor => {
                                results.push(format!(
                                    "error {} non-fast-forward (use --force to override)",
                                    spec.dst
                                ));
                                eprintln!(
                                    "  Rejected: {} has commits you don't have. Pull first or use --force.",
                                    spec.dst
                                );
                                eprintln!("  remote: {}", remote_sha);
                                eprintln!("  local : {}", sha);
                                continue;
                            }
                            AncestorCheck::Unknown(reason) => {
                                results.push(format!(
                                    "error {} fast-forward-check-failed (use --force to override)",
                                    spec.dst
                                ));
                                eprintln!("  Rejected: {} fast-forward check failed.", spec.dst);
                                eprintln!("  Could not verify ancestry between:");
                                eprintln!("    remote: {}", remote_sha);
                                eprintln!("    local : {}", sha);
                                eprintln!("  merge-base error: {}", reason);
                                continue;
                            }
                        }
                    }
                }

                // Push objects
                match self.push_objects(&sha, &spec.dst) {
                    Ok(()) => {
                        results.push(format!("ok {}", spec.dst));
                        pushed_refs.push((spec.dst, sha));
                    }
                    Err(e) => results.push(format!("error {} {}", spec.dst, e)),
                }
            }
        }

        // Detect and mark merged PRs (non-blocking)
        if self.nostr.can_sign() && !pushed_refs.is_empty() {
            self.detect_and_mark_merged_prs(&pushed_refs);
        }

        results.push(String::new()); // Empty line terminates
        Ok(Some(results))
    }

    /// Load existing refs and objects from remote before pushing
    /// This preserves branches that aren't being pushed
    fn load_existing_remote_state(&mut self) -> Result<()> {
        let data_dir = get_hashtree_data_dir();
        self.detail(&format!(
            "  Loading existing remote state... (data_dir: {:?})",
            data_dir
        ));

        // Fetch refs from nostr (this also caches root hash)
        let (refs, root_hash, _encryption_key) =
            self.nostr.fetch_refs_with_root(&self.repo_name)?;

        if refs.is_empty() {
            self.detail("  No existing refs found (new repository)");
            return Ok(());
        }

        self.detail(&format!("  Found {} existing refs", refs.len()));

        // Store remote refs for non-fast-forward detection
        self.remote_refs.clear();
        for (ref_name, ref_value) in &refs {
            // Only track branch refs (not HEAD symref)
            if ref_name.starts_with("refs/") && !ref_value.starts_with("ref: ") {
                self.remote_refs.insert(ref_name.clone(), ref_value.clone());
            }
        }

        // Import refs into storage (these will be merged with pushed refs)
        for (ref_name, ref_value) in &refs {
            // Skip refs that we're about to push (they'll be overwritten anyway)
            let is_being_pushed = self.push_specs.iter().any(|s| s.dst == *ref_name);
            if !is_being_pushed {
                self.storage.import_ref(ref_name, ref_value)?;
                debug!(
                    "Imported existing ref: {} -> {}",
                    ref_name,
                    &ref_value[..12.min(ref_value.len())]
                );
            }
        }

        let preserved_refs: Vec<(String, String)> = refs
            .iter()
            .filter(|(ref_name, ref_value)| {
                ref_name.starts_with("refs/")
                    && !ref_value.starts_with("ref: ")
                    && !self.push_specs.iter().any(|spec| spec.dst == **ref_name)
            })
            .map(|(ref_name, ref_value)| (ref_name.clone(), ref_value.clone()))
            .collect();

        if preserved_refs.is_empty() {
            self.detail("  No untouched direct refs to preserve");
            self.detail("  Remote state loaded");
            return Ok(());
        }

        if self.import_preserved_remote_objects_from_local_git(&preserved_refs)? {
            self.detail("  Reused preserved remote objects from local git");
        } else if let Some(root) = root_hash {
            self.detail(
                "  Falling back to remote object import for preserved refs not available locally",
            );
            let objects = self.fetch_all_git_objects(&root)?;
            self.detail(&format!("  Importing {} existing objects", objects.len()));

            for (oid, content) in objects {
                // Content from hashtree is already the compressed loose object
                // (that's what we store in build_objects_dir)
                self.storage.import_compressed_object(&oid, content)?;
            }
        } else {
            bail!("No root hash found for repository - cannot preserve untouched refs");
        }

        self.detail("  Remote state loaded");
        Ok(())
    }

    fn import_preserved_remote_objects_from_local_git(
        &self,
        preserved_refs: &[(String, String)],
    ) -> Result<bool> {
        let mut include_shas: Vec<String> =
            preserved_refs.iter().map(|(_, sha)| sha.clone()).collect();
        include_shas.sort();
        include_shas.dedup();

        if include_shas.is_empty() {
            return Ok(true);
        }

        let existing = self.git_batch_check_objects(include_shas.iter().map(|sha| sha.as_str()))?;
        if existing.len() != include_shas.len() {
            let missing: Vec<String> = include_shas
                .iter()
                .filter(|sha| !existing.contains(*sha))
                .cloned()
                .collect();
            self.detail(&format!(
                "  Local git is missing {} preserved remote tip(s): {}",
                missing.len(),
                missing
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            return Ok(false);
        }

        let exclude_shas = self.resolved_push_tip_shas();
        let objects = match self.list_objects_for_shas(&include_shas, &exclude_shas) {
            Ok(objects) => objects,
            Err(err) => {
                self.detail(&format!(
                    "  Could not enumerate preserved remote objects from local git: {}",
                    err
                ));
                return Ok(false);
            }
        };

        self.detail(&format!(
            "  Importing {} preserved object(s) from local git for {} untouched ref(s)",
            objects.len(),
            preserved_refs.len()
        ));

        let objects_with_content = match self.read_git_objects_batch(&objects) {
            Ok(objects_with_content) => objects_with_content,
            Err(err) => {
                self.detail(&format!(
                    "  Could not read preserved remote objects from local git: {}",
                    err
                ));
                return Ok(false);
            }
        };

        for (obj_type, content) in objects_with_content {
            self.storage.write_raw_object(obj_type, &content)?;
        }

        Ok(true)
    }

    fn resolved_push_tip_shas(&self) -> Vec<String> {
        let mut shas = Vec::new();
        for spec in &self.push_specs {
            if spec.src.is_empty() {
                continue;
            }
            if let Ok(sha) = self.resolve_ref(&spec.src) {
                shas.push(sha);
            }
        }
        shas.sort();
        shas.dedup();
        shas
    }

    /// Resolve a ref to its sha
    fn resolve_ref(&self, refspec: &str) -> Result<String> {
        let output = Command::new("git").args(["rev-parse", refspec]).output()?;

        if !output.status.success() {
            bail!("Failed to resolve ref: {}", refspec);
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Check if ancestor_sha is an ancestor of descendant_sha
    fn check_ancestor(&self, ancestor_sha: &str, descendant_sha: &str) -> AncestorCheck {
        // git merge-base --is-ancestor returns:
        //   0 => true
        //   1 => false (not ancestor)
        //   >1 => error
        let output = Command::new("git")
            .args(["merge-base", "--is-ancestor", ancestor_sha, descendant_sha])
            .output();

        match output {
            Ok(o) => Self::classify_merge_base_result(o.status.code(), &o.stderr),
            Err(e) => AncestorCheck::Unknown(format!("failed to run git merge-base: {}", e)),
        }
    }

    fn classify_merge_base_result(status_code: Option<i32>, stderr: &[u8]) -> AncestorCheck {
        match status_code {
            Some(0) => AncestorCheck::Ancestor,
            Some(1) => AncestorCheck::NotAncestor,
            Some(code) => {
                let stderr = String::from_utf8_lossy(stderr).trim().to_string();
                if stderr.is_empty() {
                    AncestorCheck::Unknown(format!("git merge-base exited with exit code {}", code))
                } else {
                    AncestorCheck::Unknown(format!(
                        "git merge-base exited with exit code {}: {}",
                        code, stderr
                    ))
                }
            }
            None => {
                let stderr = String::from_utf8_lossy(stderr).trim().to_string();
                if stderr.is_empty() {
                    AncestorCheck::Unknown(
                        "git merge-base terminated with no exit code".to_string(),
                    )
                } else {
                    AncestorCheck::Unknown(format!(
                        "git merge-base terminated with no exit code: {}",
                        stderr
                    ))
                }
            }
        }
    }

    /// Push all objects reachable from sha
    fn push_objects(&mut self, sha: &str, dst_ref: &str) -> Result<()> {
        // Get list of objects to push
        eprint!("  Listing objects...");
        let _ = std::io::stderr().flush();
        let objects = self.list_objects_to_push(sha)?;
        eprintln!(" {} objects", objects.len());

        info!("Pushing {} objects for {}", objects.len(), sha);

        // Read all objects in batch using git cat-file --batch
        let objects_with_content = self.read_git_objects_batch(&objects)?;
        eprintln!(); // Newline after reading progress

        eprint!("  Writing to local store...");
        let _ = std::io::stderr().flush();
        let total = objects_with_content.len();
        for (i, (obj_type, content)) in objects_with_content.into_iter().enumerate() {
            self.storage.write_raw_object(obj_type, &content)?;
            if (i + 1) % 1000 == 0 || i + 1 == total {
                eprint!("\r  Writing to local store: {}/{}", i + 1, total);
                let _ = std::io::stderr().flush();
            }
        }
        eprintln!();

        // Update ref in storage
        let oid = crate::git::object::ObjectId::from_hex(sha)
            .ok_or_else(|| anyhow::anyhow!("Invalid object id: {}", sha))?;
        self.storage.write_ref(dst_ref, &Ref::Direct(oid))?;

        // Set HEAD to point to this branch if it's a branch ref
        // This is needed for wasm-git to detect the current branch
        if dst_ref.starts_with("refs/heads/") {
            self.storage
                .write_ref("HEAD", &Ref::Symbolic(dst_ref.to_string()))?;
            debug!("Set HEAD -> {}", dst_ref);
        }

        // Check if we can sign before doing any work
        if !self.nostr.can_sign() {
            anyhow::bail!(
                "Cannot push: no secret key for {}. You can only push to your own repos.",
                self.nostr.npub()
            );
        }

        // Build the merkle tree
        if self.is_slow() {
            eprint!("  Building merkle tree...");
            let _ = std::io::stderr().flush();
        }
        let root_cid = self.storage.build_tree()?;
        let root_hash_hex = hex::encode(root_cid.hash);
        let chk_key = root_cid.key;
        let is_link_visible = self.url_secret.is_some();
        if self.is_slow() {
            eprintln!(
                " done (encrypted: {}, link_visible: {}, private: {})",
                chk_key.is_some(),
                is_link_visible,
                self.is_private
            );
        }

        // For private repos: XOR the CHK key with url_secret so only URL holders can decrypt
        // For public repos: publish the CHK key directly
        let key_to_publish = if let (Some(chk), Some(secret)) = (chk_key, self.url_secret) {
            // XOR the keys - to decrypt, recipient XORs with their copy of secret
            let mut masked = [0u8; 32];
            for i in 0..32 {
                masked[i] = chk[i] ^ secret[i];
            }
            Some(masked)
        } else {
            chk_key
        };

        // Push to file servers (blossom) first
        // This makes content available before we advertise the hash
        // Get old root hash if it exists (for efficient diff-based upload)
        let old_root_hash = self.nostr.get_cached_root_hash(&self.repo_name).cloned();
        let old_encryption_key = self
            .nostr
            .get_cached_encryption_key(&self.repo_name)
            .copied();
        let blossom_result = self.push_to_file_servers_with_diff(
            &root_hash_hex,
            chk_key.as_ref(),
            old_root_hash.as_deref(),
            old_encryption_key.as_ref(),
        );

        // Then publish to nostr (kind 30078 with hashtree label)
        // Include masked key (encryptedKey tag) for private or raw CHK key (key tag) for public repos
        let key_with_privacy = key_to_publish
            .as_ref()
            .map(|k| (k, is_link_visible, self.is_private));
        let (npub_url, relay_result) = self
            .nostr
            .publish_repo(&self.repo_name, &root_hash_hex, key_with_privacy)
            .map_err(|e| anyhow::anyhow!("Failed to publish repo metadata to relays: {}", e))?;

        // Build full URL with secret fragment if private
        let full_url = if let Some(secret) = self.url_secret {
            format!("{}#k={}", npub_url, hex::encode(secret))
        } else {
            npub_url.clone()
        };

        // Print summary
        eprintln!("Published to: {}", full_url);

        // Print relay details
        if !relay_result.connected.is_empty() {
            eprintln!("  Relays: {}", relay_result.connected.join(", "));
        } else {
            eprintln!("  Relays: none");
        }
        if !relay_result.failed.is_empty() {
            eprintln!("  Relays failed: {}", relay_result.failed.join(", "));
        }

        // Print blossom details
        if !blossom_result.succeeded.is_empty() {
            eprintln!("  Blossom: {}", blossom_result.succeeded.join(", "));
        }
        if !blossom_result.failed.is_empty() {
            eprintln!("  Blossom failed: {}", blossom_result.failed.join(", "));
        }

        eprintln!("  Config: ~/.hashtree/config.toml");

        // Print web viewer URL
        if let Some(path) = npub_url.strip_prefix("htree://") {
            let viewer_url = build_repo_viewer_url(path, self.url_secret.as_ref());
            eprintln!("View at: {}", viewer_url);
        }

        match self.storage.evict_if_needed() {
            Ok(freed) if freed > 0 => {
                info!(
                    "Evicted {} bytes from shared git blob cache after push",
                    freed
                );
            }
            Ok(_) => {}
            Err(err) => {
                warn!("Failed to evict shared git blob cache after push: {}", err);
            }
        }

        Ok(())
    }

    /// Find merged-in parent SHAs from merge commits in a pushed range.
    ///
    /// For `git rev-list --merges --parents`, each line is:
    /// `<merge_commit_sha> <first_parent> <merged_parent_1> [<merged_parent_2> ...]`
    /// We only care about the merged-in parents (`parts[2..]`) for PR matching.
    fn find_merged_parent_shas(&self, range: &str) -> Result<HashSet<String>> {
        let output = Command::new("git")
            .args(["rev-list", "--merges", "--parents", range])
            .output()
            .context("Failed to run git rev-list")?;

        if !output.status.success() {
            return Ok(HashSet::new());
        }

        // Skip merge commit SHA (field 0) and first parent (field 1); collect merged-in
        // branch parent SHAs (`parts[2..]`).
        let merged_parent_shas: HashSet<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .flat_map(|line| line.split_whitespace().skip(2).map(str::to_owned))
            .collect();

        Ok(merged_parent_shas)
    }

    /// Detect merged PRs in pushed refs and publish status events
    fn detect_and_mark_merged_prs(&self, pushed_refs: &[(String, String)]) {
        // Fetch a single snapshot of open PRs for this push.
        let open_prs = match self
            .nostr
            .fetch_prs(&self.repo_name, PullRequestStateFilter::Open)
        {
            Ok(prs) => prs,
            Err(e) => {
                debug!("Failed to fetch open PRs: {}", e);
                return;
            }
        };

        if open_prs.is_empty() {
            return;
        }

        let merge_candidates = pushed_refs
            .iter()
            // Only check branch refs.
            .filter_map(|(dst_ref, sha)| {
                dst_ref
                    .strip_prefix("refs/heads/")
                    .map(|branch_name| (dst_ref, branch_name, sha))
            })
            .filter_map(|(dst_ref, branch_name, sha)| {
                let Some(old_sha) = self.remote_refs.get(dst_ref) else {
                    debug!(
                        "Skipping PR auto-merge detection for {}: previous remote tip is unknown",
                        dst_ref
                    );
                    return None;
                };

                let range = format!("{}..{}", old_sha, sha);
                let merged_parent_shas = match self.find_merged_parent_shas(&range) {
                    Ok(m) => m,
                    Err(e) => {
                        debug!("Failed to find merge commits for {}: {}", dst_ref, e);
                        return None;
                    }
                };

                if merged_parent_shas.is_empty() {
                    return None;
                }

                debug!(
                    "Found {} merged parent SHAs in push to {}",
                    merged_parent_shas.len(),
                    dst_ref
                );

                Some((branch_name, merged_parent_shas))
            });

        for (branch_name, merged_parent_shas) in merge_candidates {
            let matching_prs = open_prs
                .iter()
                .filter(|pr| pr.target_branch.as_deref().unwrap_or("master") == branch_name)
                // Check if any merge commit's second+ parent matches PR's commit tip
                .filter(|pr| {
                    pr.commit_tip
                        .as_ref()
                        .is_some_and(|commit_tip| merged_parent_shas.contains(commit_tip))
                });

            // Publish status events
            for pr in matching_prs {
                match self
                    .nostr
                    .publish_pr_merged_status(&pr.event_id, &pr.author_pubkey)
                {
                    Ok(()) => {
                        eprintln!(
                            "PR auto-merged: ({})...",
                            &pr.event_id[..12.min(pr.event_id.len())]
                        );
                    }
                    Err(e) => {
                        debug!("Failed to publish PR merged status: {}", e);
                    }
                }
            }
        }
    }

    /// Push content to file servers (blossom) with efficient diff-based upload
    ///
    /// When an old root hash is provided, computes the diff and only uploads
    /// hashes that don't exist in the old tree. This significantly reduces
    /// upload time for incremental pushes.
    ///
    /// Returns BlossomResult with server details
    fn push_to_file_servers_with_diff(
        &self,
        root_hash: &str,
        encryption_key: Option<&[u8; 32]>,
        old_root_hash: Option<&str>,
        old_encryption_key: Option<&[u8; 32]>,
    ) -> BlossomResult {
        use hashtree_core::crypto::decrypt_chk;
        use hashtree_core::try_decode_tree_node;

        let store = self.storage.store();
        let blossom = self.nostr.blossom();
        let configured: Vec<String> = blossom.write_servers().to_vec();

        // Create runtime for async uploads
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                warn!("Failed to create runtime for blossom upload: {}", e);
                return BlossomResult {
                    configured: configured.clone(),
                    succeeded: vec![],
                    failed: configured,
                };
            }
        };

        // Parse root hash
        let root_bytes = match hex::decode(root_hash) {
            Ok(b) if b.len() == 32 => {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&b);
                arr
            }
            _ => {
                warn!("Invalid root hash: {}", root_hash);
                return BlossomResult {
                    configured: configured.clone(),
                    succeeded: vec![],
                    failed: configured,
                };
            }
        };

        // Parse old root hash if provided
        let old_root_bytes: Option<[u8; 32]> = old_root_hash.and_then(|h| {
            hex::decode(h).ok().and_then(|b| {
                if b.len() == 32 {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&b);
                    Some(arr)
                } else {
                    None
                }
            })
        });

        let verbose = self.is_slow(); // Capture before async block
        let force_upload = self.config.blossom.force_upload;
        let success = rt.block_on(async {
            use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
            use std::sync::Arc;
            use tokio::sync::mpsc;
            use hashtree_core::{HashTree, HashTreeConfig, Cid, collect_hashes};

            let uploaded = Arc::new(AtomicUsize::new(0));
            let skipped_diff = Arc::new(AtomicUsize::new(0)); // Skipped due to diff (already in old tree)
            let skipped_server = Arc::new(AtomicUsize::new(0)); // Skipped due to server already having it
            let failed = Arc::new(AtomicUsize::new(0));
            let completed = Arc::new(AtomicUsize::new(0));
            let discovered_total = Arc::new(AtomicUsize::new(1));
            let discovery_complete = Arc::new(AtomicBool::new(false));

            // Collect old tree hashes if we have an old root
            let old_hashes: HashSet<[u8; 32]> = if let Some(old_root) = old_root_bytes {
                // Check if old and new root are the same (no changes)
                if old_root == root_bytes {
                    if verbose {
                        eprintln!("  No changes detected (same root hash)");
                    }
                    return true;
                }

                if verbose {
                    eprint!("  Computing diff from previous tree...");
                    let _ = std::io::stderr().flush();
                }

                // Only walk the local store here. If the previous tree is missing locally,
                // fall back to a full upload instead of blocking on remote Blossom reads.
                let tree = HashTree::new(HashTreeConfig::new(store.clone()));
                let old_cid = Cid {
                    hash: old_root,
                    key: old_encryption_key.copied(),
                };

                match collect_hashes(&tree, &old_cid, 32).await {
                    Ok(hashes) => {
                        if verbose {
                            eprintln!(" {} hashes in old tree", hashes.len());
                        }
                        hashes
                    }
                    Err(e) => {
                        if verbose {
                            eprintln!(" failed: {}", e);
                            eprintln!("  Falling back to full upload");
                        }
                        HashSet::new()
                    }
                }
            } else {
                HashSet::new()
            };

            let has_old_tree = !old_hashes.is_empty();

            // Check which servers need full upload (don't have old tree)
            // If force_upload is true, all servers get full upload (skip server-has check)
            let all_servers: Vec<String> = blossom.write_servers().to_vec();
            let servers_needing_full: Arc<Vec<String>> = if force_upload {
                // Force upload to all servers
                Arc::new(all_servers.clone())
            } else if has_old_tree && !all_servers.is_empty() {
                // Always include the root hash first, then sample additional old hashes.
                // Root is critical - if server doesn't have root, it can't serve the tree
                let old_root = old_root_bytes.unwrap();
                let mut sample_hashes = vec![hex::encode(old_root)];
                for hash in old_hashes
                    .iter()
                    .filter(|h| **h != old_root)
                    .take(SERVER_COVERAGE_SAMPLE_SIZE.saturating_sub(1))
                {
                    sample_hashes.push(hex::encode(hash));
                }
                let sample_refs: Vec<&str> = sample_hashes.iter().map(|s| s.as_str()).collect();
                let mut needs_full = Vec::new();
                for server in &all_servers {
                    if !blossom
                        .server_has_tree_samples(
                            server,
                            &sample_refs,
                            SERVER_COVERAGE_SAMPLE_SIZE,
                        )
                        .await
                    {
                        needs_full.push(server.clone());
                    }
                }
                if !needs_full.is_empty() && verbose {
                    let server_names: Vec<_> = needs_full.iter()
                        .map(|s| s.trim_start_matches("https://").trim_start_matches("http://").split('/').next().unwrap_or(s))
                        .collect();
                    eprintln!("  Full upload needed: {} (missing old tree)", server_names.join(", "));
                }
                Arc::new(needs_full)
            } else {
                Arc::new(Vec::new())
            };

            // Channel sends:
            // - data: encrypted blob bytes
            // - from_old_tree: whether hash existed in previous tree
            // - force_all_servers: whether this hash must be pushed to every write server
            const CHANNEL_SIZE: usize = 100;
            const UPLOAD_CONCURRENCY: usize = 10;
            let (tx, rx) = mpsc::channel::<(Vec<u8>, bool, bool)>(CHANNEL_SIZE);

            // Spawn upload workers
            let upload_handle = {
                let blossom = blossom.clone();
                let uploaded = Arc::clone(&uploaded);
                let skipped_server = Arc::clone(&skipped_server);
                let failed = Arc::clone(&failed);
                let completed = Arc::clone(&completed);
                let skipped_diff = Arc::clone(&skipped_diff);
                let discovered_total = Arc::clone(&discovered_total);
                let discovery_complete = Arc::clone(&discovery_complete);
                let servers_needing_full = Arc::clone(&servers_needing_full);

                tokio::spawn(async move {
                    use futures::stream::StreamExt;
                    use tokio_stream::wrappers::ReceiverStream;

                    let stream = ReceiverStream::new(rx);
                    stream
                        .map(|(data, from_old_tree, force_all_servers)| {
                            let blossom = &blossom;
                            let uploaded = Arc::clone(&uploaded);
                            let skipped_server = Arc::clone(&skipped_server);
                            let failed = Arc::clone(&failed);
                            let completed = Arc::clone(&completed);
                            let skipped_diff = Arc::clone(&skipped_diff);
                            let discovered_total = Arc::clone(&discovered_total);
                            let discovery_complete = Arc::clone(&discovery_complete);
                            let servers_needing_full = Arc::clone(&servers_needing_full);
                            async move {
                                // If from old tree and some servers need full upload, push the
                                // reused content to every configured write server.
                                let result = if force_all_servers
                                    || (from_old_tree && !servers_needing_full.is_empty())
                                {
                                    blossom.upload_to_all_servers(&data).await.map(|(h, c)| (h, c > 0))
                                } else {
                                    blossom.upload_if_missing(&data).await
                                };
                                match result {
                                    Ok((_, true)) => { uploaded.fetch_add(1, Ordering::Relaxed); }
                                    Ok((_, false)) => { skipped_server.fetch_add(1, Ordering::Relaxed); }
                                    Err(e) => {
                                        failed.fetch_add(1, Ordering::Relaxed);
                                        eprintln!("\n  Upload failed ({} bytes): {}", data.len(), e);
                                    }
                                }
                                let count = completed.fetch_add(1, Ordering::Relaxed) + 1;
                                if count == 1 || count.is_multiple_of(10) {
                                    let discovered = discovered_total.load(Ordering::Relaxed);
                                    let total = discovery_complete
                                        .load(Ordering::Relaxed)
                                        .then_some(discovered);
                                    emit_upload_progress(
                                        count,
                                        discovered,
                                        total,
                                        uploaded.load(Ordering::Relaxed),
                                        skipped_diff.load(Ordering::Relaxed),
                                        skipped_server.load(Ordering::Relaxed),
                                        failed.load(Ordering::Relaxed),
                                        has_old_tree,
                                    );
                                }
                            }
                        })
                        .buffer_unordered(UPLOAD_CONCURRENCY)
                        .for_each(|_| async {})
                        .await;
                })
            };

            // Walk tree and send blobs to upload channel
            // Queue entries are (hash, optional decryption key)
            let mut visited: HashSet<[u8; 32]> = HashSet::new();
            let mut queued: HashSet<[u8; 32]> = HashSet::new();
            let mut queue: Vec<([u8; 32], Option<[u8; 32]>)> = Vec::new();
            let _ = queue_hash_if_new(&mut queue, &mut queued, root_bytes, encryption_key.copied());

            eprint!(
                "{}",
                format_upload_progress_discovering(
                    0,
                    discovered_total.load(Ordering::Relaxed),
                    0,
                    0,
                    0,
                    0,
                    has_old_tree
                )
            );
            let _ = std::io::stderr().flush();

            while let Some((hash, key)) = queue.pop() {
                if visited.contains(&hash) {
                    continue;
                }
                visited.insert(hash);
                let discovered = discovered_total.load(Ordering::Relaxed);

                // Check if this hash exists in old tree
                let from_old_tree = old_hashes.contains(&hash);

                // If the server sample says coverage is intact, unchanged hashes can
                // usually be skipped. When coverage looks incomplete, or when a hash is
                // missing despite the sample passing, force upload to all write servers.
                let mut force_all_servers_for_hash =
                    from_old_tree && !servers_needing_full.is_empty();
                if from_old_tree && !force_all_servers_for_hash {
                    let hash_hex = hex::encode(hash);
                    let mut missing_on_any_server = false;
                    for server in blossom.write_servers() {
                        if !blossom.exists_on_server(&hash_hex, server).await {
                            missing_on_any_server = true;
                            break;
                        }
                    }
                    if !missing_on_any_server {
                        skipped_diff.fetch_add(1, Ordering::Relaxed);
                        let count = completed.fetch_add(1, Ordering::Relaxed) + 1;
                        if count == 1 || count.is_multiple_of(10) {
                            emit_upload_progress(
                                count,
                                discovered,
                                None,
                                uploaded.load(Ordering::Relaxed),
                                skipped_diff.load(Ordering::Relaxed),
                                skipped_server.load(Ordering::Relaxed),
                                failed.load(Ordering::Relaxed),
                                has_old_tree,
                            );
                        }
                        continue;
                    }
                    // At least one server is missing this "unchanged" hash.
                    // Re-upload it to all servers to repair coverage.
                    force_all_servers_for_hash = true;
                }

                // Load blob from store (stored encrypted)
                let data = match store.get_sync(&hash) {
                    Ok(Some(data)) => data,
                    Ok(None) => {
                        failed.fetch_add(1, Ordering::Relaxed);
                        let count = completed.fetch_add(1, Ordering::Relaxed) + 1;
                        eprintln!("\n  Missing from local store: {}", hex::encode(hash));
                        if count == 1 || count.is_multiple_of(10) {
                            emit_upload_progress(
                                count,
                                discovered,
                                None,
                                uploaded.load(Ordering::Relaxed),
                                skipped_diff.load(Ordering::Relaxed),
                                skipped_server.load(Ordering::Relaxed),
                                failed.load(Ordering::Relaxed),
                                has_old_tree,
                            );
                        }
                        continue;
                    }
                    Err(e) => {
                        failed.fetch_add(1, Ordering::Relaxed);
                        let count = completed.fetch_add(1, Ordering::Relaxed) + 1;
                        eprintln!("\n  Store read error for {}: {}", hex::encode(hash), e);
                        if count == 1 || count.is_multiple_of(10) {
                            emit_upload_progress(
                                count,
                                discovered,
                                None,
                                uploaded.load(Ordering::Relaxed),
                                skipped_diff.load(Ordering::Relaxed),
                                skipped_server.load(Ordering::Relaxed),
                                failed.load(Ordering::Relaxed),
                                has_old_tree,
                            );
                        }
                        continue;
                    }
                };

                // Decrypt if we have a key, then check if it's a tree node
                let plaintext = if let Some(k) = key {
                    match decrypt_chk(&data, &k) {
                        Ok(p) => p,
                        Err(_) => data.clone(), // Decryption failed, try as-is
                    }
                } else {
                    data.clone()
                };

                // Check if it's a tree node and queue children with their keys
                if let Some(node) = try_decode_tree_node(&plaintext) {
                    for link in node.links {
                        if queue_hash_if_new(&mut queue, &mut queued, link.hash, link.key) {
                            discovered_total.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }

                // Send encrypted blob to upload channel (blossom stores ciphertext)
                if tx
                    .send((data, from_old_tree, force_all_servers_for_hash))
                    .await
                    .is_err()
                {
                    break; // Channel closed
                }
                let discovered = discovered_total.load(Ordering::Relaxed);
                if discovered.is_multiple_of(100) {
                    emit_upload_progress(
                        completed.load(Ordering::Relaxed),
                        discovered,
                        None,
                        uploaded.load(Ordering::Relaxed),
                        skipped_diff.load(Ordering::Relaxed),
                        skipped_server.load(Ordering::Relaxed),
                        failed.load(Ordering::Relaxed),
                        has_old_tree,
                    );
                }
            }

            discovery_complete.store(true, Ordering::Relaxed);

            let final_total_seen = discovered_total.load(Ordering::Relaxed);
            emit_upload_progress(
                completed.load(Ordering::Relaxed),
                final_total_seen,
                Some(final_total_seen),
                uploaded.load(Ordering::Relaxed),
                skipped_diff.load(Ordering::Relaxed),
                skipped_server.load(Ordering::Relaxed),
                failed.load(Ordering::Relaxed),
                has_old_tree,
            );

            // Close channel and wait for uploads to complete
            drop(tx);
            let _ = upload_handle.await;

            let final_uploaded = uploaded.load(Ordering::Relaxed);
            let final_skipped_diff = skipped_diff.load(Ordering::Relaxed);
            let final_skipped_server = skipped_server.load(Ordering::Relaxed);
            let final_failed = failed.load(Ordering::Relaxed);
            let final_completed = completed.load(Ordering::Relaxed);

            // Final progress
            emit_upload_progress(
                final_completed,
                final_total_seen,
                Some(final_total_seen),
                final_uploaded,
                final_skipped_diff,
                final_skipped_server,
                final_failed,
                has_old_tree,
            );
            eprintln!();

            info!(
                "Blossom upload complete: {} uploaded, {} unchanged (diff), {} already on server, {} failed",
                final_uploaded, final_skipped_diff, final_skipped_server, final_failed
            );

            final_uploaded > 0 || final_skipped_server > 0 || final_skipped_diff > 0
        });

        // For now, we can't track per-server success because blossom client
        // returns on first successful server. Report all as succeeded if any worked.
        if success {
            BlossomResult {
                configured: configured.clone(),
                succeeded: configured,
                failed: vec![],
            }
        } else {
            BlossomResult {
                configured: configured.clone(),
                succeeded: vec![],
                failed: configured,
            }
        }
    }

    /// Collect all hashes reachable from a root hash by walking the merkle tree
    #[allow(dead_code)]
    fn collect_tree_hashes(&self, root_hash: &str) -> Result<Vec<[u8; 32]>> {
        use hashtree_core::try_decode_tree_node;

        let store = self.storage.store();
        let mut hashes = Vec::new();
        let mut visited: HashSet<[u8; 32]> = HashSet::new();

        // Parse root hash
        let root_bytes = hex::decode(root_hash).context("Invalid root hash hex")?;
        if root_bytes.len() != 32 {
            bail!("Root hash must be 32 bytes");
        }
        let mut root: [u8; 32] = [0u8; 32];
        root.copy_from_slice(&root_bytes);

        let mut queue = vec![root];

        while let Some(hash) = queue.pop() {
            if visited.contains(&hash) {
                continue;
            }
            visited.insert(hash);
            hashes.push(hash);

            // Get blob data and check if it's a tree node
            if let Ok(Some(data)) = store.get_sync(&hash) {
                // Try to decode as tree node
                if let Some(node) = try_decode_tree_node(&data) {
                    // Queue all child hashes
                    for link in node.links {
                        if !visited.contains(&link.hash) {
                            queue.push(link.hash);
                        }
                    }
                }
                // If not a tree node, it's a leaf blob - already added to hashes
            }
        }

        debug!(
            "Collected {} hashes from tree {}",
            hashes.len(),
            &root_hash[..12]
        );
        Ok(hashes)
    }

    /// List objects that need to be pushed (not on remote)
    fn list_objects_to_push(&self, sha: &str) -> Result<Vec<String>> {
        self.list_objects_for_shas(&[sha.to_string()], &[])
    }

    fn list_objects_for_shas(&self, include: &[String], exclude: &[String]) -> Result<Vec<String>> {
        if include.is_empty() {
            return Ok(Vec::new());
        }

        let mut command = Command::new("git");
        command.arg("rev-list").arg("--objects");
        for sha in include {
            command.arg(sha);
        }
        if !exclude.is_empty() {
            command.arg("--not");
            for sha in exclude {
                command.arg(sha);
            }
        }

        let output = command.output()?;
        if !output.status.success() {
            bail!("Failed to list objects");
        }

        let mut seen = HashSet::new();
        let mut objects = Vec::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            // Format: <sha> [path]
            if let Some(oid) = line.split_whitespace().next() {
                let oid = oid.to_string();
                if seen.insert(oid.clone()) {
                    objects.push(oid);
                }
            }
        }

        Ok(objects)
    }

    /// Read multiple git objects using git cat-file --batch
    /// Processes in batches to avoid pipe buffer deadlock
    fn read_git_objects_batch(&self, oids: &[String]) -> Result<Vec<(ObjectType, Vec<u8>)>> {
        use std::io::{BufRead, BufReader, Read, Write};

        if oids.is_empty() {
            return Ok(Vec::new());
        }

        let total = oids.len();
        let mut results = Vec::with_capacity(total);

        // Process in batches of 100 to avoid pipe buffer issues
        const BATCH_SIZE: usize = 100;

        for (batch_idx, batch) in oids.chunks(BATCH_SIZE).enumerate() {
            let mut child = Command::new("git")
                .args(["cat-file", "--batch"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()?;

            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| anyhow::anyhow!("Failed to open stdin"))?;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| anyhow::anyhow!("Failed to open stdout"))?;

            // Write batch OIDs to stdin
            for oid in batch {
                writeln!(stdin, "{}", oid)?;
            }
            drop(stdin);

            // Read responses
            let mut reader = BufReader::new(stdout);

            for (i, oid) in batch.iter().enumerate() {
                let mut header = String::new();
                reader.read_line(&mut header)?;
                let header = header.trim();

                let parts: Vec<&str> = header.split_whitespace().collect();
                if parts.len() < 3 {
                    bail!("Object not found or invalid header for {}: {}", oid, header);
                }

                let obj_type = match parts[1] {
                    "blob" => ObjectType::Blob,
                    "tree" => ObjectType::Tree,
                    "commit" => ObjectType::Commit,
                    "tag" => ObjectType::Tag,
                    _ => bail!("Unknown object type: {}", parts[1]),
                };

                let size: usize = parts[2].parse()?;
                let mut content = vec![0u8; size];
                reader.read_exact(&mut content)?;

                // Read trailing newline
                let mut newline = [0u8; 1];
                reader.read_exact(&mut newline)?;

                results.push((obj_type, content));

                // Progress indicator
                let done = batch_idx * BATCH_SIZE + i + 1;
                if done == 1 || done.is_multiple_of(100) || done == total {
                    eprint!("\r  Reading objects: {}/{}", done, total);
                    let _ = std::io::stderr().flush();
                }
            }

            child.wait()?;
        }

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, Bytes},
        extract::{Path as AxumPath, State},
        http::{header, HeaderMap, Response, StatusCode},
        routing::put,
        Router,
    };
    use hashtree_core::{HashTree, HashTreeConfig, MemoryStore, Store};
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::thread::JoinHandle;
    use tempfile::TempDir;
    use tokio::sync::oneshot;

    const TEST_PUBKEY: &str = "4523be58d395b1b196a9b8c82b038b6895cb02b683d0c253a955068dba1facd0";
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[derive(Default)]
    struct CountingBlossomState {
        blobs: HashMap<String, Vec<u8>>,
        get_requests: usize,
    }

    struct CountingBlossomServer {
        state: Arc<Mutex<CountingBlossomState>>,
        shutdown_tx: Option<oneshot::Sender<()>>,
        server_thread: Option<JoinHandle<()>>,
        base_url: String,
    }

    impl CountingBlossomServer {
        fn new() -> Self {
            let state = Arc::new(Mutex::new(CountingBlossomState::default()));
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake blossom");
            let port = listener.local_addr().expect("fake blossom addr").port();
            listener
                .set_nonblocking(true)
                .expect("set fake blossom nonblocking");
            let state_clone = Arc::clone(&state);
            let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

            let server_thread = std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("build fake blossom runtime");

                rt.block_on(async move {
                    let app = Router::new()
                        .route("/upload", put(upload_blob))
                        .route("/:id", axum::routing::get(get_blob).head(head_blob))
                        .with_state(state_clone);

                    let listener = tokio::net::TcpListener::from_std(listener)
                        .expect("tokio fake blossom listener");

                    axum::serve(listener, app)
                        .with_graceful_shutdown(async {
                            let _ = shutdown_rx.await;
                        })
                        .await
                        .expect("fake blossom serve");
                });
            });

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    return Self {
                        state,
                        shutdown_tx: Some(shutdown_tx),
                        server_thread: Some(server_thread),
                        base_url: format!("http://127.0.0.1:{}", port),
                    };
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }

            panic!("fake blossom did not start");
        }

        fn base_url(&self) -> &str {
            &self.base_url
        }

        fn get_request_count(&self) -> usize {
            self.state.lock().expect("state lock").get_requests
        }
    }

    impl Drop for CountingBlossomServer {
        fn drop(&mut self) {
            if let Some(tx) = self.shutdown_tx.take() {
                let _ = tx.send(());
            }
            if let Some(handle) = self.server_thread.take() {
                let _ = handle.join();
            }
        }
    }

    fn parse_hash_from_path(id: &str) -> Option<String> {
        let hash = id.strip_suffix(".bin").unwrap_or(id);
        if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            Some(hash.to_ascii_lowercase())
        } else {
            None
        }
    }

    async fn upload_blob(
        State(state): State<Arc<Mutex<CountingBlossomState>>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> StatusCode {
        let mut hasher = Sha256::new();
        hasher.update(&body);
        let computed_hash = hex::encode(hasher.finalize());

        if let Some(expected_hash) = headers
            .get("x-sha-256")
            .and_then(|value| value.to_str().ok())
            .map(|value| value.to_ascii_lowercase())
        {
            if expected_hash != computed_hash {
                return StatusCode::BAD_REQUEST;
            }
        }

        let mut state = state.lock().expect("state lock");
        if state.blobs.insert(computed_hash, body.to_vec()).is_some() {
            StatusCode::CONFLICT
        } else {
            StatusCode::OK
        }
    }

    async fn head_blob(
        State(state): State<Arc<Mutex<CountingBlossomState>>>,
        AxumPath(id): AxumPath<String>,
    ) -> Response<Body> {
        let Some(hash) = parse_hash_from_path(&id) else {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::empty())
                .unwrap();
        };

        let state = state.lock().expect("state lock");
        if let Some(data) = state.blobs.get(&hash) {
            return Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .header(header::CONTENT_LENGTH, data.len().to_string())
                .body(Body::empty())
                .unwrap();
        }

        Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .unwrap()
    }

    async fn get_blob(
        State(state): State<Arc<Mutex<CountingBlossomState>>>,
        AxumPath(id): AxumPath<String>,
    ) -> Response<Body> {
        let Some(hash) = parse_hash_from_path(&id) else {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::empty())
                .unwrap();
        };

        let data = {
            let mut state = state.lock().expect("state lock");
            state.get_requests += 1;
            state.blobs.get(&hash).cloned()
        };

        match data {
            Some(bytes) => Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .header(header::CONTENT_LENGTH, bytes.len().to_string())
                .body(Body::from(bytes))
                .unwrap(),
            None => Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap(),
        }
    }

    struct HomeGuard {
        previous: Option<String>,
    }

    impl HomeGuard {
        fn set(path: &std::path::Path) -> Self {
            let previous = std::env::var("HOME").ok();
            std::env::set_var("HOME", path);
            Self { previous }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            if let Some(previous) = self.previous.as_deref() {
                std::env::set_var("HOME", previous);
            } else {
                std::env::remove_var("HOME");
            }
        }
    }

    struct CwdGuard {
        previous: std::path::PathBuf,
    }

    impl CwdGuard {
        fn set(path: &std::path::Path) -> Self {
            let previous = std::env::current_dir().expect("current dir");
            std::env::set_current_dir(path).expect("set current dir");
            Self { previous }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.previous).expect("restore current dir");
        }
    }

    fn git(repo: &std::path::Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap_or_else(|err| panic!("git {:?} failed to start: {}", args, err))
    }

    fn create_repo_with_diverged_master_and_dev() -> (TempDir, TempDir, String, String, String) {
        let home = TempDir::new().expect("temp home");
        let _home_guard = HomeGuard::set(home.path());

        let repo = TempDir::new().expect("temp repo");
        assert!(git(repo.path(), &["init", "-b", "master"]).status.success());
        assert!(
            git(repo.path(), &["config", "user.email", "test@example.com"])
                .status
                .success()
        );
        assert!(git(repo.path(), &["config", "user.name", "Test User"])
            .status
            .success());

        std::fs::write(repo.path().join("README.md"), "initial\n").unwrap();
        assert!(git(repo.path(), &["add", "README.md"]).status.success());
        assert!(git(repo.path(), &["commit", "-m", "Initial commit"])
            .status
            .success());
        let base_sha = String::from_utf8_lossy(&git(repo.path(), &["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_string();

        assert!(git(repo.path(), &["checkout", "-b", "dev"])
            .status
            .success());
        std::fs::write(repo.path().join("dev-only.txt"), "dev-only\n").unwrap();
        assert!(git(repo.path(), &["add", "dev-only.txt"]).status.success());
        assert!(git(repo.path(), &["commit", "-m", "Dev commit"])
            .status
            .success());
        let dev_sha = String::from_utf8_lossy(&git(repo.path(), &["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_string();

        assert!(git(repo.path(), &["checkout", "master"]).status.success());
        std::fs::write(repo.path().join("master-only.txt"), "master-only\n").unwrap();
        assert!(git(repo.path(), &["add", "master-only.txt"])
            .status
            .success());
        assert!(git(repo.path(), &["commit", "-m", "Master commit"])
            .status
            .success());
        let master_sha = String::from_utf8_lossy(&git(repo.path(), &["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_string();

        (home, repo, base_sha, master_sha, dev_sha)
    }

    fn create_test_helper() -> Option<RemoteHelper> {
        let config = Config::default();
        RemoteHelper::new(TEST_PUBKEY, "test-repo", None, None, false, config).ok()
    }

    fn create_test_helper_with_config(config: Config) -> Option<RemoteHelper> {
        RemoteHelper::new(TEST_PUBKEY, "test-repo", None, None, false, config).ok()
    }

    fn write_test_config(home: &std::path::Path, blossom_url: &str, force_upload: bool) {
        let config_dir = home.join(".hashtree");
        std::fs::create_dir_all(&config_dir).expect("create config dir");
        let config = format!(
            r#"
[server]
enable_auth = false
stun_port = 0

[nostr]
relays = []
social_graph_crawl_depth = 0

[blossom]
read_servers = ["{blossom_url}"]
write_servers = ["{blossom_url}"]
force_upload = {force_upload}
"#
        );
        std::fs::write(config_dir.join("config.toml"), config).expect("write config");
    }

    #[test]
    fn test_build_repo_viewer_url_uses_git_host() {
        assert_eq!(
            build_repo_viewer_url("npub1example/repo", None),
            "https://git.iris.to/#/npub1example/repo"
        );
    }

    #[test]
    fn test_build_repo_viewer_url_preserves_link_key() {
        let url_secret = [0xab; 32];
        assert_eq!(
            build_repo_viewer_url("npub1example/repo", Some(&url_secret)),
            format!(
                "https://git.iris.to/#/npub1example/repo?k={}",
                "ab".repeat(32)
            )
        );
    }

    #[test]
    fn test_capabilities() {
        let Some(helper) = create_test_helper() else {
            return; // Skip if storage can't be created
        };

        let caps = helper.capabilities();
        assert!(caps.contains(&"fetch".to_string()));
        assert!(caps.contains(&"push".to_string()));
        assert!(caps.contains(&"option".to_string()));
        // Should end with empty line
        assert_eq!(caps.last(), Some(&String::new()));
    }

    #[test]
    fn test_handle_capabilities_command() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        let result = helper.handle_command("capabilities").unwrap();
        assert!(result.is_some());
        let caps = result.unwrap();
        assert!(caps.contains(&"fetch".to_string()));
        assert!(caps.contains(&"push".to_string()));
    }

    #[test]
    fn test_handle_list_command() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        // list may fail in tests without a published repo; ensure it doesn't panic
        // and validates successful output shape when available.
        match helper.handle_command("list") {
            Ok(Some(lines)) => {
                assert_eq!(lines.last(), Some(&String::new()));
            }
            Ok(None) => panic!("list should return output lines"),
            Err(err) => {
                assert!(
                    err.to_string().contains("not found"),
                    "unexpected list error: {}",
                    err
                );
            }
        }
    }

    #[test]
    fn test_handle_list_for_push_command() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        let result = helper.handle_command("list for-push").unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn test_handle_option_command() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        let result = helper.handle_command("option verbosity 1").unwrap();
        assert!(result.is_some());
        let lines = result.unwrap();
        assert!(lines.contains(&"ok".to_string()));
    }

    #[test]
    fn test_handle_unknown_command() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        let result = helper.handle_command("unknown-command").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_handle_empty_line_exits() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        assert!(!helper.should_exit());
        let _ = helper.handle_command("").unwrap();
        assert!(helper.should_exit());
    }

    #[test]
    fn test_queue_fetch() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        // Queue a fetch
        let result = helper
            .handle_command("fetch abc123def456 refs/heads/main")
            .unwrap();
        assert!(result.is_none()); // fetch queues, doesn't respond immediately

        assert_eq!(helper.fetch_specs.len(), 1);
        assert_eq!(helper.fetch_specs[0].sha, "abc123def456");
        assert_eq!(helper.fetch_specs[0].name, "refs/heads/main");
    }

    #[test]
    fn test_queue_multiple_fetches() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        helper
            .handle_command("fetch abc123 refs/heads/main")
            .unwrap();
        helper
            .handle_command("fetch def456 refs/heads/feature")
            .unwrap();

        assert_eq!(helper.fetch_specs.len(), 2);
    }

    #[test]
    fn test_queue_fetch_invalid() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        // Missing name
        let result = helper.handle_command("fetch abc123");
        assert!(result.is_err());
    }

    #[test]
    fn test_queue_push() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        let result = helper
            .handle_command("push refs/heads/main:refs/heads/main")
            .unwrap();
        assert!(result.is_none()); // push queues, doesn't respond immediately

        assert_eq!(helper.push_specs.len(), 1);
        assert_eq!(helper.push_specs[0].src, "refs/heads/main");
        assert_eq!(helper.push_specs[0].dst, "refs/heads/main");
        assert!(!helper.push_specs[0].force);
    }

    #[test]
    fn test_queue_force_push() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        helper
            .handle_command("push +refs/heads/main:refs/heads/main")
            .unwrap();

        assert_eq!(helper.push_specs.len(), 1);
        assert!(helper.push_specs[0].force);
    }

    #[test]
    fn test_queue_delete_push() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        // Empty src means delete
        helper
            .handle_command("push :refs/heads/old-branch")
            .unwrap();

        assert_eq!(helper.push_specs.len(), 1);
        assert_eq!(helper.push_specs[0].src, "");
        assert_eq!(helper.push_specs[0].dst, "refs/heads/old-branch");
    }

    #[test]
    fn test_queue_push_invalid() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        // Missing colon separator
        let result = helper.handle_command("push refs/heads/main");
        assert!(result.is_err());
    }

    #[test]
    fn test_push_spec_parsing() {
        // Test internal PushSpec parsing via queue_push
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        // Normal push
        helper.queue_push("src:dst").unwrap();
        assert_eq!(helper.push_specs[0].src, "src");
        assert_eq!(helper.push_specs[0].dst, "dst");
        assert!(!helper.push_specs[0].force);

        helper.push_specs.clear();

        // Force push
        helper.queue_push("+src:dst").unwrap();
        assert!(helper.push_specs[0].force);
        assert_eq!(helper.push_specs[0].src, "src");

        helper.push_specs.clear();

        // Delete (empty src)
        helper.queue_push(":dst").unwrap();
        assert_eq!(helper.push_specs[0].src, "");
        assert_eq!(helper.push_specs[0].dst, "dst");
    }

    #[test]
    fn test_fetch_spec_parsing() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        helper
            .queue_fetch("abc123def456789 refs/heads/main")
            .unwrap();

        assert_eq!(helper.fetch_specs[0].sha, "abc123def456789");
        assert_eq!(helper.fetch_specs[0].name, "refs/heads/main");
    }

    #[test]
    fn test_fetch_spec_with_tag() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        helper.queue_fetch("abc123 refs/tags/v1.0.0").unwrap();
        assert_eq!(helper.fetch_specs[0].name, "refs/tags/v1.0.0");
    }

    #[test]
    fn test_should_exit_initially_false() {
        let Some(helper) = create_test_helper() else {
            return;
        };

        assert!(!helper.should_exit());
    }

    #[test]
    fn test_get_hashtree_data_dir() {
        let dir = get_hashtree_data_dir();
        assert!(dir.ends_with("data"));
        assert!(dir.to_string_lossy().contains(".hashtree"));
    }

    #[test]
    fn test_command_parsing_with_spaces() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        // Commands are split by first space only
        let result = helper.handle_command("option verbosity 1").unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn test_list_clears_remote_refs() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        // Add some dummy refs
        helper
            .remote_refs
            .insert("refs/heads/old".to_string(), "abc".to_string());

        // list should clear stale refs even if remote lookup fails
        let _ = helper.handle_command("list");
        assert!(helper.remote_refs.is_empty());
    }

    #[test]
    fn test_classify_merge_base_result_code_zero_is_ancestor() {
        let result = RemoteHelper::classify_merge_base_result(Some(0), b"");
        assert_eq!(result, AncestorCheck::Ancestor);
    }

    #[test]
    fn test_classify_merge_base_result_code_one_is_not_ancestor() {
        let result = RemoteHelper::classify_merge_base_result(Some(1), b"");
        assert_eq!(result, AncestorCheck::NotAncestor);
    }

    #[test]
    fn test_classify_merge_base_result_other_code_is_error() {
        let result = RemoteHelper::classify_merge_base_result(Some(2), b"fatal: bad object");
        match result {
            AncestorCheck::Unknown(reason) => {
                assert!(reason.contains("exit code 2"));
                assert!(reason.contains("fatal: bad object"));
            }
            _ => panic!("Expected Unknown result"),
        }
    }

    #[test]
    fn test_classify_merge_base_result_missing_exit_code_is_error() {
        let result = RemoteHelper::classify_merge_base_result(None, b"terminated by signal");
        match result {
            AncestorCheck::Unknown(reason) => {
                assert!(reason.contains("no exit code"));
                assert!(reason.contains("terminated by signal"));
            }
            _ => panic!("Expected Unknown result"),
        }
    }

    #[test]
    fn test_upload_progress_includes_processed_over_total_for_old_tree() {
        assert_eq!(
            format_upload_progress(12, 34, 7, 5, 0, 0, true),
            "  Uploading: 12/34 (7 new, 5 unchanged, 0 exist)"
        );
    }

    #[test]
    fn test_upload_progress_uses_unknown_total_while_discovering_old_tree() {
        assert_eq!(
            format_upload_progress_discovering(12, 34, 7, 5, 0, 0, true),
            "  Uploading: 12/? (34 discovered, 7 new, 5 unchanged, 0 exist)"
        );
    }

    #[test]
    fn test_upload_progress_includes_processed_over_total_for_new_tree_failures() {
        assert_eq!(
            format_upload_progress(12, 34, 7, 0, 5, 2, false),
            "  Uploading: 12/34 (7 new, 5 exist, 2 FAILED)"
        );
    }

    #[test]
    fn test_upload_progress_uses_unknown_total_while_discovering_new_tree_failures() {
        assert_eq!(
            format_upload_progress_discovering(12, 34, 7, 0, 5, 2, false),
            "  Uploading: 12/? (34 discovered, 7 new, 5 exist, 2 FAILED)"
        );
    }

    #[test]
    fn test_queue_hash_if_new_counts_unique_hashes_when_queued() {
        let mut queue = Vec::new();
        let mut queued = HashSet::new();
        let hash_a = [0x11; 32];
        let hash_b = [0x22; 32];

        assert!(queue_hash_if_new(&mut queue, &mut queued, hash_a, None));
        assert!(!queue_hash_if_new(
            &mut queue,
            &mut queued,
            hash_a,
            Some([0x33; 32])
        ));
        assert!(queue_hash_if_new(
            &mut queue,
            &mut queued,
            hash_b,
            Some([0x44; 32])
        ));

        assert_eq!(queue.len(), 2);
        assert_eq!(queued.len(), 2);
        assert_eq!(queue[0], (hash_a, None));
        assert_eq!(queue[1], (hash_b, Some([0x44; 32])));
    }

    #[test]
    fn test_list_objects_for_shas_excludes_shared_history() {
        let _env_lock = ENV_LOCK.lock().expect("env lock");
        let (home, repo, base_sha, master_sha, dev_sha) =
            create_repo_with_diverged_master_and_dev();
        let _home_guard = HomeGuard::set(home.path());
        let _cwd_guard = CwdGuard::set(repo.path());

        let helper = create_test_helper().expect("helper");
        let full = helper
            .list_objects_for_shas(std::slice::from_ref(&dev_sha), &[])
            .expect("list full objects");
        let exclusive = helper
            .list_objects_for_shas(
                std::slice::from_ref(&dev_sha),
                std::slice::from_ref(&master_sha),
            )
            .expect("list exclusive objects");

        assert!(full.contains(&base_sha));
        assert!(full.contains(&dev_sha));
        assert!(exclusive.contains(&dev_sha));
        assert!(
            !exclusive.contains(&base_sha),
            "shared base history should be excluded"
        );
        assert!(
            exclusive.len() < full.len(),
            "excluding pushed history should reduce preserved-object count"
        );
    }

    #[test]
    fn test_import_preserved_remote_objects_from_local_git_uses_exclusive_history() {
        let _env_lock = ENV_LOCK.lock().expect("env lock");
        let (home, repo, _base_sha, master_sha, dev_sha) =
            create_repo_with_diverged_master_and_dev();
        let _home_guard = HomeGuard::set(home.path());
        let _cwd_guard = CwdGuard::set(repo.path());

        let mut helper = create_test_helper().expect("helper");
        helper.push_specs.push(PushSpec {
            src: "master".to_string(),
            dst: "refs/heads/master".to_string(),
            force: false,
        });

        let exclusive = helper
            .list_objects_for_shas(
                std::slice::from_ref(&dev_sha),
                std::slice::from_ref(&master_sha),
            )
            .expect("list exclusive objects");

        let imported = helper
            .import_preserved_remote_objects_from_local_git(&[(
                "refs/heads/dev".to_string(),
                dev_sha.clone(),
            )])
            .expect("import preserved objects");

        assert!(imported, "local git should satisfy preserved ref import");
        assert_eq!(
            helper.storage.object_count().expect("object count"),
            exclusive.len()
        );
    }

    #[test]
    fn test_push_to_file_servers_with_diff_does_not_fetch_old_tree_from_blossom() {
        let _env_lock = ENV_LOCK.lock().expect("env lock");
        let home = TempDir::new().expect("temp home");
        let _home_guard = HomeGuard::set(home.path());
        let fake_blossom = CountingBlossomServer::new();
        write_test_config(home.path(), fake_blossom.base_url(), true);

        let mut config = Config::default();
        config.nostr.relays = vec![];
        config.blossom.read_servers = vec![fake_blossom.base_url().to_string()];
        config.blossom.write_servers = vec![fake_blossom.base_url().to_string()];
        config.blossom.force_upload = true;

        let helper = create_test_helper_with_config(config).expect("helper");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");

        let (old_cid, new_cid) = rt.block_on(async {
            let old_store = Arc::new(MemoryStore::new());
            let old_tree = HashTree::new(HashTreeConfig::new(old_store.clone()).public());
            let (old_cid, _) = old_tree
                .put(b"old tree exists only on blossom")
                .await
                .expect("build old tree");
            let old_bytes = old_store
                .get(&old_cid.hash)
                .await
                .expect("read old root")
                .expect("old root bytes");

            hashtree_blossom::BlossomClient::new_empty(nostr::Keys::generate())
                .with_servers(vec![fake_blossom.base_url().to_string()])
                .upload(&old_bytes)
                .await
                .expect("upload old tree to fake blossom");

            let new_store = helper.storage.store().clone();
            let new_tree = HashTree::new(HashTreeConfig::new(new_store).public());
            let (new_cid, _) = new_tree
                .put(b"new tree exists only locally")
                .await
                .expect("build new tree");

            (old_cid, new_cid)
        });

        let result = helper.push_to_file_servers_with_diff(
            &hex::encode(new_cid.hash),
            None,
            Some(&hex::encode(old_cid.hash)),
            None,
        );

        assert!(
            result.failed.is_empty(),
            "diff upload should succeed without remote old-tree fetches: {:?}",
            result.failed
        );
        assert_eq!(
            fake_blossom.get_request_count(),
            0,
            "diff collection should not fetch the old tree from Blossom when it is missing locally"
        );
    }
}
