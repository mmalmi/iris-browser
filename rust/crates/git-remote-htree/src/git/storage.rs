//! Hashtree-backed git object and ref storage with configurable persistence
//!
//! Stores git objects and refs in a hashtree merkle tree:
//!   root/
//!     .git/
//!       HEAD -> "ref: refs/heads/main"
//!       refs/heads/main -> <commit-sha1>
//!       info/refs -> dumb-HTTP ref advertisement
//!       objects/XX/YYYY... -> zlib-compressed loose object (standard git layout)
//!       objects/info/packs -> dumb-HTTP pack advertisement
//!
//! The root hash (SHA-256) is the content-addressed identifier for the entire repo state.

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use hashtree_config::{Config, StorageBackend};
use hashtree_core::store::{Store, StoreError, StoreStats};
use hashtree_core::types::Hash;
use hashtree_core::{Cid, DirEntry, HashTree, HashTreeConfig, LinkType};
use hashtree_fs::FsBlobStore;
#[cfg(feature = "lmdb")]
use hashtree_lmdb::LmdbBlobStore;
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use tokio::runtime::{Handle, Runtime};
use tracing::{debug, info};

use super::object::{parse_tree, GitObject, ObjectId, ObjectType};
use super::refs::{validate_ref_name, Ref};
use super::{Error, Result};

/// Box type for async recursion
type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Runtime executor - either owns a runtime or reuses an existing one
enum RuntimeExecutor {
    Owned(Runtime),
    Handle(Handle),
}

impl RuntimeExecutor {
    fn block_on<F: std::future::Future>(&self, f: F) -> F::Output {
        match self {
            RuntimeExecutor::Owned(rt) => rt.block_on(f),
            RuntimeExecutor::Handle(handle) => tokio::task::block_in_place(|| handle.block_on(f)),
        }
    }
}

/// Local blob store - wraps either FsBlobStore or LmdbBlobStore
pub enum LocalStore {
    Fs(FsBlobStore),
    #[cfg(feature = "lmdb")]
    Lmdb(LmdbBlobStore),
}

impl LocalStore {
    fn new_for_backend<P: AsRef<Path>>(
        path: P,
        backend: StorageBackend,
        max_bytes: u64,
    ) -> std::result::Result<Self, StoreError> {
        match backend {
            StorageBackend::Fs => {
                if max_bytes > 0 {
                    Ok(LocalStore::Fs(FsBlobStore::with_max_bytes(
                        path, max_bytes,
                    )?))
                } else {
                    Ok(LocalStore::Fs(FsBlobStore::new(path)?))
                }
            }
            #[cfg(feature = "lmdb")]
            StorageBackend::Lmdb => {
                if max_bytes > 0 {
                    Ok(LocalStore::Lmdb(LmdbBlobStore::with_max_bytes(
                        path, max_bytes,
                    )?))
                } else {
                    Ok(LocalStore::Lmdb(LmdbBlobStore::new(path)?))
                }
            }
            #[cfg(not(feature = "lmdb"))]
            StorageBackend::Lmdb => {
                warn!(
                    "LMDB backend requested but lmdb feature not enabled, using filesystem storage"
                );
                if max_bytes > 0 {
                    Ok(LocalStore::Fs(FsBlobStore::with_max_bytes(
                        path, max_bytes,
                    )?))
                } else {
                    Ok(LocalStore::Fs(FsBlobStore::new(path)?))
                }
            }
        }
    }

    /// Create a new local store based on config
    pub fn new<P: AsRef<Path>>(path: P) -> std::result::Result<Self, StoreError> {
        Self::new_with_max_bytes(path, 0)
    }

    /// Create a new local store based on config with an optional byte limit.
    pub fn new_with_max_bytes<P: AsRef<Path>>(
        path: P,
        max_bytes: u64,
    ) -> std::result::Result<Self, StoreError> {
        let config = Config::load_or_default();
        Self::new_for_backend(path, config.storage.backend, max_bytes)
    }

    /// List all hashes in the store
    pub fn list(&self) -> std::result::Result<Vec<Hash>, StoreError> {
        match self {
            LocalStore::Fs(store) => store.list(),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.list(),
        }
    }

    /// Sync get operation
    pub fn get_sync(&self, hash: &Hash) -> std::result::Result<Option<Vec<u8>>, StoreError> {
        match self {
            LocalStore::Fs(store) => store.get_sync(hash),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.get_sync(hash),
        }
    }
}

#[async_trait::async_trait]
impl Store for LocalStore {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> std::result::Result<bool, StoreError> {
        match self {
            LocalStore::Fs(store) => store.put(hash, data).await,
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.put(hash, data).await,
        }
    }

    async fn get(&self, hash: &Hash) -> std::result::Result<Option<Vec<u8>>, StoreError> {
        match self {
            LocalStore::Fs(store) => store.get(hash).await,
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.get(hash).await,
        }
    }

    async fn has(&self, hash: &Hash) -> std::result::Result<bool, StoreError> {
        match self {
            LocalStore::Fs(store) => store.has(hash).await,
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.has(hash).await,
        }
    }

    async fn delete(&self, hash: &Hash) -> std::result::Result<bool, StoreError> {
        match self {
            LocalStore::Fs(store) => store.delete(hash).await,
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.delete(hash).await,
        }
    }

    fn set_max_bytes(&self, max: u64) {
        match self {
            LocalStore::Fs(store) => store.set_max_bytes(max),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.set_max_bytes(max),
        }
    }

    fn max_bytes(&self) -> Option<u64> {
        match self {
            LocalStore::Fs(store) => store.max_bytes(),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.max_bytes(),
        }
    }

    async fn stats(&self) -> StoreStats {
        match self {
            LocalStore::Fs(store) => match store.stats() {
                Ok(stats) => StoreStats {
                    count: stats.count as u64,
                    bytes: stats.total_bytes,
                    pinned_count: stats.pinned_count as u64,
                    pinned_bytes: stats.pinned_bytes,
                },
                Err(_) => StoreStats::default(),
            },
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => match store.stats() {
                Ok(stats) => StoreStats {
                    count: stats.count as u64,
                    bytes: stats.total_bytes,
                    pinned_count: 0,
                    pinned_bytes: 0,
                },
                Err(_) => StoreStats::default(),
            },
        }
    }

    async fn evict_if_needed(&self) -> std::result::Result<u64, StoreError> {
        match self {
            LocalStore::Fs(store) => store.evict_if_needed().await,
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.evict_if_needed().await,
        }
    }
}

/// Git storage backed by HashTree with configurable persistence
pub struct GitStorage {
    store: Arc<LocalStore>,
    tree: HashTree<LocalStore>,
    runtime: RuntimeExecutor,
    /// In-memory state for the current session
    objects: std::sync::RwLock<HashMap<String, Vec<u8>>>,
    refs: std::sync::RwLock<HashMap<String, String>>,
    /// Cached root CID (hash + encryption key)
    root_cid: std::sync::RwLock<Option<Cid>>,
}

impl GitStorage {
    /// Open or create a git storage at the given path
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let config = Config::load_or_default();
        let max_size_bytes = config
            .storage
            .max_size_gb
            .saturating_mul(1024 * 1024 * 1024);
        Self::open_with_max_bytes(path, max_size_bytes)
    }

    /// Open or create a git storage at the given path with an explicit byte limit.
    pub fn open_with_max_bytes(path: impl AsRef<Path>, max_size_bytes: u64) -> Result<Self> {
        let config = Config::load_or_default();
        Self::open_with_backend_and_max_bytes(path, config.storage.backend, max_size_bytes)
    }

    pub fn open_with_backend_and_max_bytes(
        path: impl AsRef<Path>,
        backend: StorageBackend,
        max_size_bytes: u64,
    ) -> Result<Self> {
        let runtime = match Handle::try_current() {
            Ok(handle) => RuntimeExecutor::Handle(handle),
            Err(_) => {
                let rt = Runtime::new()
                    .map_err(|e| Error::StorageError(format!("tokio runtime: {}", e)))?;
                RuntimeExecutor::Owned(rt)
            }
        };

        let store_path = path.as_ref().join("blobs");
        let store = Arc::new(
            LocalStore::new_for_backend(&store_path, backend, max_size_bytes)
                .map_err(|e| Error::StorageError(format!("local store: {}", e)))?,
        );

        // Use encrypted mode (default) - blossom servers require encrypted data
        let tree = HashTree::new(HashTreeConfig::new(store.clone()));

        Ok(Self {
            store,
            tree,
            runtime,
            objects: std::sync::RwLock::new(HashMap::new()),
            refs: std::sync::RwLock::new(HashMap::new()),
            root_cid: std::sync::RwLock::new(None),
        })
    }

    /// Evict old local blobs if storage is over the configured limit.
    pub fn evict_if_needed(&self) -> Result<u64> {
        self.runtime
            .block_on(self.store.evict_if_needed())
            .map_err(|e| Error::StorageError(format!("evict: {}", e)))
    }

    /// Write an object, returning its ID
    fn write_object(&self, obj: &GitObject) -> Result<ObjectId> {
        let oid = obj.id();
        let key = oid.to_hex();

        let loose = obj.to_loose_format();
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&loose)?;
        let compressed = encoder.finish()?;

        let mut objects = self
            .objects
            .write()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
        objects.insert(key, compressed);

        // Invalidate cached root
        if let Ok(mut root) = self.root_cid.write() {
            *root = None;
        }

        Ok(oid)
    }

    /// Write raw object data (type + content already parsed)
    pub fn write_raw_object(&self, obj_type: ObjectType, content: &[u8]) -> Result<ObjectId> {
        let obj = GitObject::new(obj_type, content.to_vec());
        self.write_object(&obj)
    }

    /// Read an object by ID from in-memory cache
    #[allow(dead_code)]
    fn read_object(&self, oid: &ObjectId) -> Result<GitObject> {
        let key = oid.to_hex();
        let objects = self
            .objects
            .read()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
        let compressed = objects
            .get(&key)
            .ok_or_else(|| Error::ObjectNotFound(key.clone()))?;

        let mut decoder = ZlibDecoder::new(compressed.as_slice());
        let mut data = Vec::new();
        decoder.read_to_end(&mut data)?;

        GitObject::from_loose_format(&data)
    }

    /// Write a ref
    pub fn write_ref(&self, name: &str, target: &Ref) -> Result<()> {
        validate_ref_name(name)?;

        let value = match target {
            Ref::Direct(oid) => oid.to_hex(),
            Ref::Symbolic(target) => format!("ref: {}", target),
        };

        let mut refs = self
            .refs
            .write()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
        refs.insert(name.to_string(), value);

        // Invalidate cached root
        if let Ok(mut root) = self.root_cid.write() {
            *root = None;
        }

        Ok(())
    }

    /// Read a ref
    #[allow(dead_code)]
    pub fn read_ref(&self, name: &str) -> Result<Option<Ref>> {
        let refs = self
            .refs
            .read()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;

        match refs.get(name) {
            Some(value) => {
                if let Some(target) = value.strip_prefix("ref: ") {
                    Ok(Some(Ref::Symbolic(target.to_string())))
                } else {
                    let oid = ObjectId::from_hex(value)
                        .ok_or_else(|| Error::StorageError(format!("invalid ref: {}", value)))?;
                    Ok(Some(Ref::Direct(oid)))
                }
            }
            None => Ok(None),
        }
    }

    /// List all refs
    #[allow(dead_code)]
    pub fn list_refs(&self) -> Result<HashMap<String, String>> {
        let refs = self
            .refs
            .read()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
        Ok(refs.clone())
    }

    /// Delete a ref
    pub fn delete_ref(&self, name: &str) -> Result<bool> {
        let mut refs = self
            .refs
            .write()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
        let existed = refs.remove(name).is_some();

        // Invalidate cached root
        if let Ok(mut root) = self.root_cid.write() {
            *root = None;
        }

        Ok(existed)
    }

    /// Import a raw git object (already in loose format, zlib compressed)
    /// Used when fetching existing objects from remote before push
    pub fn import_compressed_object(&self, oid: &str, compressed_data: Vec<u8>) -> Result<()> {
        let mut objects = self
            .objects
            .write()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
        objects.insert(oid.to_string(), compressed_data);

        // Invalidate cached root
        if let Ok(mut root) = self.root_cid.write() {
            *root = None;
        }

        Ok(())
    }

    /// Import a ref directly (used when loading existing refs from remote)
    pub fn import_ref(&self, name: &str, value: &str) -> Result<()> {
        let mut refs = self
            .refs
            .write()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
        refs.insert(name.to_string(), value.to_string());

        // Invalidate cached root
        if let Ok(mut root) = self.root_cid.write() {
            *root = None;
        }

        Ok(())
    }

    /// Check if a ref exists
    #[cfg(test)]
    pub fn has_ref(&self, name: &str) -> Result<bool> {
        let refs = self
            .refs
            .read()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
        Ok(refs.contains_key(name))
    }

    /// Get count of objects in storage
    #[cfg(test)]
    pub fn object_count(&self) -> Result<usize> {
        let objects = self
            .objects
            .read()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
        Ok(objects.len())
    }

    /// Get the cached root CID (returns None if tree hasn't been built)
    #[allow(dead_code)]
    pub fn get_root_cid(&self) -> Result<Option<Cid>> {
        let root = self
            .root_cid
            .read()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
        Ok(root.clone())
    }

    /// Get the default branch name
    #[allow(dead_code)]
    pub fn default_branch(&self) -> Result<Option<String>> {
        let refs = self
            .refs
            .read()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;

        if let Some(head) = refs.get("HEAD") {
            if let Some(target) = head.strip_prefix("ref: ") {
                return Ok(Some(target.to_string()));
            }
        }
        Ok(None)
    }

    /// Get the tree SHA from a commit object
    fn get_commit_tree(
        &self,
        commit_oid: &str,
        objects: &HashMap<String, Vec<u8>>,
    ) -> Option<String> {
        let compressed = objects.get(commit_oid)?;

        // Decompress the object
        let mut decoder = ZlibDecoder::new(&compressed[..]);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed).ok()?;

        // Parse git object format: "type size\0content"
        let null_pos = decompressed.iter().position(|&b| b == 0)?;
        let content = &decompressed[null_pos + 1..];

        // Parse commit content - first line is "tree <sha>"
        let content_str = std::str::from_utf8(content).ok()?;
        let first_line = content_str.lines().next()?;
        first_line
            .strip_prefix("tree ")
            .map(|tree_hash| tree_hash.to_string())
    }

    /// Get git object content (decompressed, without header)
    fn get_object_content(
        &self,
        oid: &str,
        objects: &HashMap<String, Vec<u8>>,
    ) -> Option<(ObjectType, Vec<u8>)> {
        let compressed = objects.get(oid)?;

        // Decompress the object
        let mut decoder = ZlibDecoder::new(&compressed[..]);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed).ok()?;

        // Parse git object format: "type size\0content"
        let null_pos = decompressed.iter().position(|&b| b == 0)?;
        let header = std::str::from_utf8(&decompressed[..null_pos]).ok()?;
        let obj_type = if header.starts_with("blob") {
            ObjectType::Blob
        } else if header.starts_with("tree") {
            ObjectType::Tree
        } else if header.starts_with("commit") {
            ObjectType::Commit
        } else if header.starts_with("tag") {
            ObjectType::Tag
        } else {
            return None;
        };
        let content = decompressed[null_pos + 1..].to_vec();
        Some((obj_type, content))
    }

    fn peel_tag_target(&self, oid: &str, objects: &HashMap<String, Vec<u8>>) -> Option<String> {
        let (obj_type, content) = self.get_object_content(oid, objects)?;
        if obj_type != ObjectType::Tag {
            return Some(oid.to_string());
        }

        let target = std::str::from_utf8(&content)
            .ok()?
            .lines()
            .find_map(|line| line.strip_prefix("object "))
            .map(str::trim)?
            .to_string();

        match self.get_object_content(&target, objects)?.0 {
            ObjectType::Tag => self.peel_tag_target(&target, objects),
            _ => Some(target),
        }
    }

    fn build_info_refs_content(
        &self,
        refs: &HashMap<String, String>,
        objects: &HashMap<String, Vec<u8>>,
    ) -> String {
        let mut lines = Vec::new();

        for (name, value) in refs {
            if name == "HEAD" {
                continue;
            }

            let oid = value.trim().to_string();
            lines.push((name.clone(), oid.clone()));

            if name.starts_with("refs/tags/") {
                if let Some(peeled) = self.peel_tag_target(&oid, objects) {
                    if peeled != oid {
                        lines.push((format!("{}^{{}}", name), peeled));
                    }
                }
            }
        }

        lines.sort_by(|a, b| a.0.cmp(&b.0));

        let mut content = String::new();
        for (name, oid) in lines {
            content.push_str(&oid);
            content.push('\t');
            content.push_str(&name);
            content.push('\n');
        }
        content
    }

    async fn build_info_dir(
        &self,
        refs: &HashMap<String, String>,
        objects: &HashMap<String, Vec<u8>>,
    ) -> Result<Cid> {
        let info_refs = self.build_info_refs_content(refs, objects);
        let (info_refs_cid, info_refs_size) = self
            .tree
            .put(info_refs.as_bytes())
            .await
            .map_err(|e| Error::StorageError(format!("put info/refs: {}", e)))?;

        self.tree
            .put_directory(vec![
                DirEntry::from_cid("refs", &info_refs_cid).with_size(info_refs_size)
            ])
            .await
            .map_err(|e| Error::StorageError(format!("put info dir: {}", e)))
    }

    /// Build the hashtree and return the root CID (hash + encryption key)
    pub fn build_tree(&self) -> Result<Cid> {
        // Check if we have a cached root
        if let Ok(root) = self.root_cid.read() {
            if let Some(ref cid) = *root {
                return Ok(cid.clone());
            }
        }

        let objects = self
            .objects
            .read()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
        let refs = self
            .refs
            .read()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;

        // Get default branch from HEAD or find first branch ref
        let (default_branch, commit_sha) = if let Some(head) = refs.get("HEAD") {
            let branch = head.strip_prefix("ref: ").map(String::from);
            let sha = branch.as_ref().and_then(|b| refs.get(b)).cloned();
            (branch, sha)
        } else {
            // No HEAD ref - find first refs/heads/* ref directly
            let mut branch_info: Option<(String, String)> = None;
            for (ref_name, sha) in refs.iter() {
                if ref_name.starts_with("refs/heads/") {
                    branch_info = Some((ref_name.clone(), sha.clone()));
                    break;
                }
            }
            match branch_info {
                Some((branch, sha)) => (Some(branch), Some(sha)),
                None => (None, None),
            }
        };

        // Get tree SHA from commit
        let tree_sha = commit_sha
            .as_ref()
            .and_then(|sha| self.get_commit_tree(sha, &objects));

        // Clone objects for async block
        let objects_clone = objects.clone();

        let root_cid = self.runtime.block_on(async {
            // Build objects directory
            let objects_cid = self.build_objects_dir(&objects).await?;

            // Build refs directory
            let refs_cid = self.build_refs_dir(&refs).await?;

            // Build dumb-HTTP info directory
            let info_cid = self.build_info_dir(&refs, &objects_clone).await?;

            // Build HEAD file - use default_branch if no explicit HEAD
            // Git expects HEAD to end with newline, so add it if missing
            let head_content = refs.get("HEAD")
                .map(|h| if h.ends_with('\n') { h.clone() } else { format!("{}\n", h) })
                .or_else(|| default_branch.as_ref().map(|b| format!("ref: {}\n", b)))
                .unwrap_or_else(|| "ref: refs/heads/main\n".to_string());
            debug!("HEAD content: {:?}", head_content);
            let (head_cid, head_size) = self.tree.put(head_content.as_bytes()).await
                .map_err(|e| Error::StorageError(format!("put HEAD: {}", e)))?;
            debug!("HEAD hash: {}", hex::encode(head_cid.hash));

            // Build .git directory - use from_cid to preserve encryption keys
            let mut git_entries = vec![
                DirEntry::from_cid("HEAD", &head_cid).with_size(head_size),
                DirEntry::from_cid("info", &info_cid).with_link_type(LinkType::Dir),
                DirEntry::from_cid("objects", &objects_cid).with_link_type(LinkType::Dir),
                DirEntry::from_cid("refs", &refs_cid).with_link_type(LinkType::Dir),
            ];

            // Add config if we have a default branch
            if let Some(ref branch) = default_branch {
                let config = format!(
                    "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = true\n[init]\n\tdefaultBranch = {}\n",
                    branch.trim_start_matches("refs/heads/")
                );
                let (config_cid, config_size) = self.tree.put(config.as_bytes()).await
                    .map_err(|e| Error::StorageError(format!("put config: {}", e)))?;
                git_entries.push(DirEntry::from_cid("config", &config_cid).with_size(config_size));
            }

            // Build and add index file if we have a tree SHA
            if let Some(ref tree_oid) = tree_sha {
                match self.build_index_file(tree_oid, &objects_clone) {
                    Ok(index_data) => {
                        let (index_cid, index_size) = self.tree.put(&index_data).await
                            .map_err(|e| Error::StorageError(format!("put index: {}", e)))?;
                        git_entries.push(DirEntry::from_cid("index", &index_cid).with_size(index_size));
                        info!("Added git index file ({} bytes)", index_data.len());
                    }
                    Err(e) => {
                        debug!("Failed to build git index file: {} - continuing without index", e);
                    }
                }
            }

            let git_cid = self.tree.put_directory(git_entries).await
                .map_err(|e| Error::StorageError(format!("put .git: {}", e)))?;

            // Build root entries starting with .git
            // Use from_cid to preserve the encryption key
            let mut root_entries = vec![DirEntry::from_cid(".git", &git_cid).with_link_type(LinkType::Dir)];

            // Add working tree files if we have a tree SHA
            if let Some(ref tree_oid) = tree_sha {
                let working_tree_entries = self.build_working_tree_entries(tree_oid, &objects_clone).await?;
                root_entries.extend(working_tree_entries);
                info!("Added {} working tree entries to root", root_entries.len() - 1);
            }

            // Sort entries for deterministic ordering
            root_entries.sort_by(|a, b| a.name.cmp(&b.name));

            let root_cid = self.tree.put_directory(root_entries).await
                .map_err(|e| Error::StorageError(format!("put root: {}", e)))?;

            info!("Built hashtree root: {} (encrypted: {}) (.git dir: {})",
                hex::encode(root_cid.hash),
                root_cid.key.is_some(),
                hex::encode(git_cid.hash));

            Ok::<Cid, Error>(root_cid)
        })?;

        // Cache the root CID
        if let Ok(mut root) = self.root_cid.write() {
            *root = Some(root_cid.clone());
        }

        Ok(root_cid)
    }

    /// Build working tree entries from a git tree object
    async fn build_working_tree_entries(
        &self,
        tree_oid: &str,
        objects: &HashMap<String, Vec<u8>>,
    ) -> Result<Vec<DirEntry>> {
        let mut entries = Vec::new();

        // Get tree content
        let (obj_type, content) = self
            .get_object_content(tree_oid, objects)
            .ok_or_else(|| Error::ObjectNotFound(tree_oid.to_string()))?;

        if obj_type != ObjectType::Tree {
            return Err(Error::InvalidObjectType(format!(
                "expected tree, got {:?}",
                obj_type
            )));
        }

        // Parse tree entries
        let tree_entries = parse_tree(&content)?;

        for entry in tree_entries {
            let oid_hex = entry.oid.to_hex();

            if entry.is_tree() {
                // Recursively build subdirectory
                let sub_entries = self
                    .build_working_tree_entries_boxed(&oid_hex, objects)
                    .await?;

                // Create subdirectory in hashtree
                let dir_cid =
                    self.tree.put_directory(sub_entries).await.map_err(|e| {
                        Error::StorageError(format!("put dir {}: {}", entry.name, e))
                    })?;

                // Use from_cid to preserve encryption key
                entries
                    .push(DirEntry::from_cid(&entry.name, &dir_cid).with_link_type(LinkType::Dir));
            } else {
                // Get blob content
                if let Some((ObjectType::Blob, blob_content)) =
                    self.get_object_content(&oid_hex, objects)
                {
                    // Use put() instead of put_blob() to chunk large files
                    let (cid, size) = self.tree.put(&blob_content).await.map_err(|e| {
                        Error::StorageError(format!("put blob {}: {}", entry.name, e))
                    })?;

                    // Use from_cid to preserve encryption key
                    entries.push(DirEntry::from_cid(&entry.name, &cid).with_size(size));
                }
            }
        }

        // Sort for deterministic ordering
        entries.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(entries)
    }

    /// Boxed version for async recursion
    fn build_working_tree_entries_boxed<'a>(
        &'a self,
        tree_oid: &'a str,
        objects: &'a HashMap<String, Vec<u8>>,
    ) -> BoxFuture<'a, Result<Vec<DirEntry>>> {
        Box::pin(self.build_working_tree_entries(tree_oid, objects))
    }

    /// Build the objects directory using HashTree
    async fn build_objects_dir(&self, objects: &HashMap<String, Vec<u8>>) -> Result<Cid> {
        let mut top_entries = Vec::new();

        if !objects.is_empty() {
            // Group objects by first 2 characters of SHA (git loose object structure)
            // Git expects objects/XX/YYYYYY... where XX is first 2 hex chars
            let mut buckets: HashMap<String, Vec<(String, Vec<u8>)>> = HashMap::new();
            for (oid, data) in objects {
                let prefix = &oid[..2];
                let suffix = &oid[2..];
                buckets
                    .entry(prefix.to_string())
                    .or_default()
                    .push((suffix.to_string(), data.clone()));
            }

            // Build subdirectories for each prefix
            for (prefix, objs) in buckets {
                let mut sub_entries = Vec::new();
                for (suffix, data) in objs {
                    // Use put() instead of put_blob() to chunk large objects
                    // Git blobs can be >5MB which exceeds blossom server limits
                    let (cid, size) = self.tree.put(&data).await.map_err(|e| {
                        Error::StorageError(format!("put object {}{}: {}", prefix, suffix, e))
                    })?;
                    // Use from_cid to preserve encryption key
                    sub_entries.push(DirEntry::from_cid(suffix, &cid).with_size(size));
                }
                // Sort for deterministic ordering
                sub_entries.sort_by(|a, b| a.name.cmp(&b.name));

                let sub_cid =
                    self.tree.put_directory(sub_entries).await.map_err(|e| {
                        Error::StorageError(format!("put objects/{}: {}", prefix, e))
                    })?;
                top_entries
                    .push(DirEntry::from_cid(prefix, &sub_cid).with_link_type(LinkType::Dir));
            }
        }

        let (packs_cid, packs_size) = self
            .tree
            .put(b"")
            .await
            .map_err(|e| Error::StorageError(format!("put objects/info/packs: {}", e)))?;
        let info_cid = self
            .tree
            .put_directory(vec![
                DirEntry::from_cid("packs", &packs_cid).with_size(packs_size)
            ])
            .await
            .map_err(|e| Error::StorageError(format!("put objects/info: {}", e)))?;
        top_entries.push(DirEntry::from_cid("info", &info_cid).with_link_type(LinkType::Dir));

        // Sort for deterministic ordering
        top_entries.sort_by(|a, b| a.name.cmp(&b.name));

        let entry_count = top_entries.len();
        let cid = self
            .tree
            .put_directory(top_entries)
            .await
            .map_err(|e| Error::StorageError(format!("put objects dir: {}", e)))?;

        debug!(
            "Built objects dir with {} entries: {}",
            entry_count,
            hex::encode(cid.hash)
        );
        Ok(cid)
    }

    /// Build the refs directory using HashTree
    async fn build_refs_dir(&self, refs: &HashMap<String, String>) -> Result<Cid> {
        // Group refs by category (heads, tags, etc.)
        let mut groups: HashMap<String, Vec<(String, String)>> = HashMap::new();

        for (ref_name, value) in refs {
            let parts: Vec<&str> = ref_name.split('/').collect();
            if parts.len() >= 3 && parts[0] == "refs" {
                let category = parts[1].to_string();
                let name = parts[2..].join("/");
                groups
                    .entry(category)
                    .or_default()
                    .push((name, value.clone()));
            }
        }

        let mut ref_entries = Vec::new();

        for (category, refs_in_category) in groups {
            let mut cat_entries = Vec::new();
            for (name, value) in refs_in_category {
                // Use put() to get Cid with encryption key
                let (cid, _size) = self
                    .tree
                    .put(value.as_bytes())
                    .await
                    .map_err(|e| Error::StorageError(format!("put ref: {}", e)))?;
                debug!(
                    "refs/{}/{} -> blob {}",
                    category,
                    name,
                    hex::encode(cid.hash)
                );
                cat_entries.push(DirEntry::from_cid(name, &cid));
            }

            cat_entries.sort_by(|a, b| a.name.cmp(&b.name));

            let cat_cid = self
                .tree
                .put_directory(cat_entries)
                .await
                .map_err(|e| Error::StorageError(format!("put {} dir: {}", category, e)))?;
            debug!("refs/{} dir -> {}", category, hex::encode(cat_cid.hash));
            ref_entries.push(DirEntry::from_cid(category, &cat_cid).with_link_type(LinkType::Dir));
        }

        if ref_entries.is_empty() {
            // Return empty directory Cid
            let empty_cid = self
                .tree
                .put_directory(vec![])
                .await
                .map_err(|e| Error::StorageError(format!("put empty refs: {}", e)))?;
            return Ok(empty_cid);
        }

        ref_entries.sort_by(|a, b| a.name.cmp(&b.name));

        let refs_cid = self
            .tree
            .put_directory(ref_entries)
            .await
            .map_err(|e| Error::StorageError(format!("put refs dir: {}", e)))?;
        debug!("refs dir -> {}", hex::encode(refs_cid.hash));
        Ok(refs_cid)
    }

    /// Build git index file from tree entries
    /// Returns the raw binary content of the index file
    fn build_index_file(
        &self,
        tree_oid: &str,
        objects: &HashMap<String, Vec<u8>>,
    ) -> Result<Vec<u8>> {
        // Collect all file entries from the tree (recursively)
        let mut entries: Vec<(String, [u8; 20], u32, u32)> = Vec::new(); // (path, sha1, mode, size)
        self.collect_tree_entries_for_index(tree_oid, objects, "", &mut entries)?;

        // Sort entries by path (git index requirement)
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let entry_count = entries.len() as u32;
        debug!("Building git index with {} entries", entry_count);

        // Build index content
        let mut index_data = Vec::new();

        // Header: DIRC + version 2 + entry count
        index_data.extend_from_slice(b"DIRC");
        index_data.extend_from_slice(&2u32.to_be_bytes()); // version 2
        index_data.extend_from_slice(&entry_count.to_be_bytes());

        // Current time for ctime/mtime (doesn't matter much for our use case)
        let now_sec = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;

        for (path, sha1, mode, size) in &entries {
            let entry_start = index_data.len();

            // ctime sec, nsec
            index_data.extend_from_slice(&now_sec.to_be_bytes());
            index_data.extend_from_slice(&0u32.to_be_bytes());
            // mtime sec, nsec
            index_data.extend_from_slice(&now_sec.to_be_bytes());
            index_data.extend_from_slice(&0u32.to_be_bytes());
            // dev, ino (use 0)
            index_data.extend_from_slice(&0u32.to_be_bytes());
            index_data.extend_from_slice(&0u32.to_be_bytes());
            // mode
            index_data.extend_from_slice(&mode.to_be_bytes());
            // uid, gid (use 0)
            index_data.extend_from_slice(&0u32.to_be_bytes());
            index_data.extend_from_slice(&0u32.to_be_bytes());
            // file size
            index_data.extend_from_slice(&size.to_be_bytes());
            // SHA-1
            index_data.extend_from_slice(sha1);
            // flags: path length (max 0xFFF) in low 12 bits
            let path_len = std::cmp::min(path.len(), 0xFFF) as u16;
            index_data.extend_from_slice(&path_len.to_be_bytes());
            // path (NUL-terminated)
            index_data.extend_from_slice(path.as_bytes());
            index_data.push(0); // NUL terminator

            // Pad to 8-byte boundary relative to entry start
            let entry_len = index_data.len() - entry_start;
            let padding = (8 - (entry_len % 8)) % 8;
            index_data.extend(std::iter::repeat_n(0, padding));
        }

        // Calculate SHA-1 checksum of everything and append
        let mut hasher = Sha1::new();
        hasher.update(&index_data);
        let checksum = hasher.finalize();
        index_data.extend_from_slice(&checksum);

        debug!(
            "Built git index: {} bytes, {} entries",
            index_data.len(),
            entry_count
        );
        Ok(index_data)
    }

    /// Collect file entries from a git tree for building the index
    fn collect_tree_entries_for_index(
        &self,
        tree_oid: &str,
        objects: &HashMap<String, Vec<u8>>,
        prefix: &str,
        entries: &mut Vec<(String, [u8; 20], u32, u32)>,
    ) -> Result<()> {
        let (obj_type, content) = self
            .get_object_content(tree_oid, objects)
            .ok_or_else(|| Error::ObjectNotFound(tree_oid.to_string()))?;

        if obj_type != ObjectType::Tree {
            return Err(Error::InvalidObjectType(format!(
                "expected tree, got {:?}",
                obj_type
            )));
        }

        let tree_entries = parse_tree(&content)?;

        for entry in tree_entries {
            let path = if prefix.is_empty() {
                entry.name.clone()
            } else {
                format!("{}/{}", prefix, entry.name)
            };

            let oid_hex = entry.oid.to_hex();

            if entry.is_tree() {
                // Recursively process subdirectory
                self.collect_tree_entries_for_index(&oid_hex, objects, &path, entries)?;
            } else {
                // Get blob content for size and SHA-1
                if let Some((ObjectType::Blob, blob_content)) =
                    self.get_object_content(&oid_hex, objects)
                {
                    // Convert hex SHA to bytes
                    let mut sha1_bytes = [0u8; 20];
                    if let Ok(bytes) = hex::decode(&oid_hex) {
                        if bytes.len() == 20 {
                            sha1_bytes.copy_from_slice(&bytes);
                        }
                    }

                    // Mode: use entry.mode or default to regular file
                    let mode = entry.mode;
                    let size = blob_content.len() as u32;

                    entries.push((path, sha1_bytes, mode, size));
                }
            }
        }

        Ok(())
    }

    /// Get the underlying store
    pub fn store(&self) -> &Arc<LocalStore> {
        &self.store
    }

    /// Get the HashTree for direct access
    #[allow(dead_code)]
    pub fn hashtree(&self) -> &HashTree<LocalStore> {
        &self.tree
    }

    /// Push all blobs to file servers
    #[allow(dead_code)]
    pub fn push_to_file_servers(
        &self,
        blossom: &hashtree_blossom::BlossomClient,
    ) -> Result<(usize, usize)> {
        let hashes = self
            .store
            .list()
            .map_err(|e| Error::StorageError(format!("list hashes: {}", e)))?;

        info!("Pushing {} blobs to file servers", hashes.len());

        let mut uploaded = 0;
        let mut existed = 0;

        self.runtime.block_on(async {
            for hash in &hashes {
                let hex_hash = hex::encode(hash);
                let data = match self.store.get_sync(hash) {
                    Ok(Some(d)) => d,
                    _ => continue,
                };

                match blossom.upload_if_missing(&data).await {
                    Ok((_, true)) => {
                        debug!("Uploaded {}", &hex_hash[..12]);
                        uploaded += 1;
                    }
                    Ok((_, false)) => {
                        existed += 1;
                    }
                    Err(e) => {
                        debug!("Failed to upload {}: {}", &hex_hash[..12], e);
                    }
                }
            }
        });

        info!(
            "Upload complete: {} new, {} already existed",
            uploaded, existed
        );
        Ok((uploaded, existed))
    }

    /// Clear all state (for testing or re-initialization)
    #[allow(dead_code)]
    pub fn clear(&self) -> Result<()> {
        let mut objects = self
            .objects
            .write()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
        let mut refs = self
            .refs
            .write()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
        let mut root = self
            .root_cid
            .write()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;

        objects.clear();
        refs.clear();
        *root = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashtree_core::store::Store;
    use hashtree_core::LinkType;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    fn create_test_storage() -> (GitStorage, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let storage = GitStorage::open(temp_dir.path()).unwrap();
        (storage, temp_dir)
    }

    fn create_test_storage_with_limit(max_size_bytes: u64) -> (GitStorage, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let storage = GitStorage::open_with_backend_and_max_bytes(
            temp_dir.path(),
            StorageBackend::Fs,
            max_size_bytes,
        )
        .unwrap();
        (storage, temp_dir)
    }

    fn local_total_bytes(storage: &GitStorage) -> u64 {
        match storage.store().as_ref() {
            LocalStore::Fs(store) => store.stats().unwrap().total_bytes,
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.stats().unwrap().total_bytes,
        }
    }

    fn write_test_commit(storage: &GitStorage) -> ObjectId {
        let blob_oid = storage
            .write_raw_object(ObjectType::Blob, b"hello from hashtree\n")
            .unwrap();

        let mut tree_content = Vec::new();
        tree_content.extend_from_slice(b"100644 README.md\0");
        tree_content.extend_from_slice(&hex::decode(blob_oid.to_hex()).unwrap());
        let tree_oid = storage
            .write_raw_object(ObjectType::Tree, &tree_content)
            .unwrap();

        let commit_content = format!(
            "tree {}\nauthor Test User <test@example.com> 0 +0000\ncommitter Test User <test@example.com> 0 +0000\n\nInitial commit\n",
            tree_oid.to_hex()
        );
        storage
            .write_raw_object(ObjectType::Commit, commit_content.as_bytes())
            .unwrap()
    }

    fn export_tree_to_fs<S: Store>(
        runtime: &RuntimeExecutor,
        tree: &HashTree<S>,
        cid: &Cid,
        dst: &Path,
    ) {
        std::fs::create_dir_all(dst).unwrap();
        let entries = runtime.block_on(tree.list_directory(cid)).unwrap();
        for entry in entries {
            let entry_cid = Cid {
                hash: entry.hash,
                key: entry.key,
            };
            let path = dst.join(&entry.name);
            match entry.link_type {
                LinkType::Dir => export_tree_to_fs(runtime, tree, &entry_cid, &path),
                LinkType::Blob | LinkType::File => {
                    let data = runtime
                        .block_on(tree.get(&entry_cid, None))
                        .unwrap()
                        .unwrap();
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).unwrap();
                    }
                    std::fs::write(path, data).unwrap();
                }
            }
        }
    }

    fn spawn_http_server(root: &Path, port: u16) -> Child {
        Command::new("python3")
            .args([
                "-m",
                "http.server",
                &port.to_string(),
                "--bind",
                "127.0.0.1",
            ])
            .current_dir(root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn python http server")
    }

    fn wait_for_http_server(server: &mut Child, port: u16, path: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);

        loop {
            if let Some(status) = server.try_wait().expect("check http server status") {
                panic!("python http server exited before becoming ready: {status}");
            }

            if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
                stream
                    .set_read_timeout(Some(Duration::from_millis(200)))
                    .expect("set read timeout");
                stream
                    .set_write_timeout(Some(Duration::from_millis(200)))
                    .expect("set write timeout");
                let request =
                    format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
                if stream.write_all(request.as_bytes()).is_ok() {
                    let mut response = String::new();
                    if stream.read_to_string(&mut response).is_ok()
                        && response.starts_with("HTTP/1.0 200")
                    {
                        return;
                    }
                }
            }

            if Instant::now() >= deadline {
                panic!("python http server did not become ready on port {port}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn test_import_ref() {
        let (storage, _temp) = create_test_storage();

        // Import a ref
        storage
            .import_ref("refs/heads/main", "abc123def456")
            .unwrap();

        // Check it exists
        assert!(storage.has_ref("refs/heads/main").unwrap());

        // Check value via list_refs
        let refs = storage.list_refs().unwrap();
        assert_eq!(
            refs.get("refs/heads/main"),
            Some(&"abc123def456".to_string())
        );
    }

    #[test]
    fn test_import_multiple_refs_preserves_all() {
        let (storage, _temp) = create_test_storage();

        // Import multiple refs (simulating loading from remote)
        storage.import_ref("refs/heads/main", "sha_main").unwrap();
        storage.import_ref("refs/heads/dev", "sha_dev").unwrap();
        storage
            .import_ref("refs/heads/feature", "sha_feature")
            .unwrap();

        // All should exist
        assert!(storage.has_ref("refs/heads/main").unwrap());
        assert!(storage.has_ref("refs/heads/dev").unwrap());
        assert!(storage.has_ref("refs/heads/feature").unwrap());

        // Now write a new ref (simulating push)
        storage
            .write_ref(
                "refs/heads/new-branch",
                &Ref::Direct(
                    ObjectId::from_hex("0123456789abcdef0123456789abcdef01234567").unwrap(),
                ),
            )
            .unwrap();

        // Original refs should still exist
        let refs = storage.list_refs().unwrap();
        assert_eq!(refs.len(), 4);
        assert!(refs.contains_key("refs/heads/main"));
        assert!(refs.contains_key("refs/heads/dev"));
        assert!(refs.contains_key("refs/heads/feature"));
        assert!(refs.contains_key("refs/heads/new-branch"));
    }

    #[test]
    fn test_import_compressed_object() {
        let (storage, _temp) = create_test_storage();

        // Create a fake compressed object
        let fake_compressed = vec![0x78, 0x9c, 0x01, 0x02, 0x03]; // fake zlib data

        storage
            .import_compressed_object("abc123def456", fake_compressed.clone())
            .unwrap();

        // Check object count
        assert_eq!(storage.object_count().unwrap(), 1);
    }

    #[test]
    fn test_write_ref_overwrites_imported() {
        let (storage, _temp) = create_test_storage();

        // Import a ref
        storage.import_ref("refs/heads/main", "old_sha").unwrap();

        // Write same ref with new value
        storage
            .write_ref(
                "refs/heads/main",
                &Ref::Direct(
                    ObjectId::from_hex("0123456789abcdef0123456789abcdef01234567").unwrap(),
                ),
            )
            .unwrap();

        // Should have new value
        let refs = storage.list_refs().unwrap();
        assert_eq!(
            refs.get("refs/heads/main"),
            Some(&"0123456789abcdef0123456789abcdef01234567".to_string())
        );
    }

    #[test]
    fn test_delete_ref_preserves_others() {
        let (storage, _temp) = create_test_storage();

        // Import multiple refs
        storage.import_ref("refs/heads/main", "sha_main").unwrap();
        storage.import_ref("refs/heads/dev", "sha_dev").unwrap();

        // Delete one
        storage.delete_ref("refs/heads/dev").unwrap();

        // Other should still exist
        assert!(storage.has_ref("refs/heads/main").unwrap());
        assert!(!storage.has_ref("refs/heads/dev").unwrap());
    }

    #[test]
    fn test_clear_removes_all() {
        let (storage, _temp) = create_test_storage();

        // Import refs and objects
        storage.import_ref("refs/heads/main", "sha_main").unwrap();
        storage
            .import_compressed_object("obj1", vec![1, 2, 3])
            .unwrap();

        // Clear
        storage.clear().unwrap();

        // All gone
        assert!(!storage.has_ref("refs/heads/main").unwrap());
        assert_eq!(storage.object_count().unwrap(), 0);
    }

    #[test]
    fn test_evict_if_needed_respects_configured_limit() {
        let (storage, _temp) = create_test_storage_with_limit(1_024);

        storage
            .write_raw_object(ObjectType::Blob, &vec![b'a'; 900])
            .unwrap();
        storage
            .write_raw_object(ObjectType::Blob, &vec![b'b'; 900])
            .unwrap();
        storage
            .write_ref(
                "refs/heads/main",
                &Ref::Direct(
                    ObjectId::from_hex("0123456789abcdef0123456789abcdef01234567").unwrap(),
                ),
            )
            .unwrap();

        storage.build_tree().unwrap();

        let before = local_total_bytes(&storage);
        assert!(before > 1_024);

        let freed = storage.evict_if_needed().unwrap();
        assert!(freed > 0);

        let after = local_total_bytes(&storage);
        assert!(after <= 1_024);
    }

    #[test]
    fn test_build_tree_adds_dumb_http_metadata() {
        let (storage, _temp) = create_test_storage();
        let commit_oid = write_test_commit(&storage);
        let tag_content = format!(
            "object {}\ntype commit\ntag v1.0.0\ntagger Test User <test@example.com> 0 +0000\n\nrelease\n",
            commit_oid.to_hex()
        );
        let tag_oid = storage
            .write_raw_object(ObjectType::Tag, tag_content.as_bytes())
            .unwrap();

        storage
            .write_ref("refs/heads/main", &Ref::Direct(commit_oid))
            .unwrap();
        storage
            .write_ref("refs/tags/v1.0.0", &Ref::Direct(tag_oid))
            .unwrap();
        storage
            .write_ref("HEAD", &Ref::Symbolic("refs/heads/main".to_string()))
            .unwrap();

        let root_cid = storage.build_tree().unwrap();

        let info_refs_cid = storage
            .runtime
            .block_on(storage.tree.resolve_path(&root_cid, ".git/info/refs"))
            .unwrap()
            .expect("info/refs exists");
        let info_refs = storage
            .runtime
            .block_on(storage.tree.get(&info_refs_cid, None))
            .unwrap()
            .unwrap();
        let info_refs = String::from_utf8(info_refs).unwrap();

        assert_eq!(
            info_refs,
            format!(
                "{commit}\trefs/heads/main\n{tag}\trefs/tags/v1.0.0\n{commit}\trefs/tags/v1.0.0^{{}}\n",
                commit = commit_oid.to_hex(),
                tag = tag_oid.to_hex()
            )
        );

        let packs_cid = storage
            .runtime
            .block_on(
                storage
                    .tree
                    .resolve_path(&root_cid, ".git/objects/info/packs"),
            )
            .unwrap()
            .expect("objects/info/packs exists");
        let packs = storage
            .runtime
            .block_on(storage.tree.get(&packs_cid, None))
            .unwrap()
            .unwrap();
        assert!(packs.is_empty(), "objects/info/packs should be empty");
    }

    #[test]
    fn test_materialized_tree_supports_static_http_clone_from_git_dir() {
        let (storage, _temp) = create_test_storage();
        let commit_oid = write_test_commit(&storage);
        storage
            .write_ref("refs/heads/main", &Ref::Direct(commit_oid))
            .unwrap();
        storage
            .write_ref("HEAD", &Ref::Symbolic("refs/heads/main".to_string()))
            .unwrap();

        let root_cid = storage.build_tree().unwrap();
        let export_dir = TempDir::new().unwrap();
        let repo_dir = export_dir.path().join("repo");
        export_tree_to_fs(&storage.runtime, &storage.tree, &root_cid, &repo_dir);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let mut server = spawn_http_server(export_dir.path(), port);
        wait_for_http_server(&mut server, port, "/repo/.git/HEAD");

        let clone_dir = TempDir::new().unwrap();
        let clone_path = clone_dir.path().join("clone");
        let output = Command::new("git")
            .args([
                "clone",
                &format!("http://127.0.0.1:{port}/repo/.git", port = port),
                clone_path.to_str().unwrap(),
            ])
            .output()
            .unwrap();

        let _ = server.kill();
        let _ = server.wait();

        assert!(
            output.status.success(),
            "git clone failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(clone_path.join("README.md")).unwrap(),
            "hello from hashtree\n"
        );
    }
}
