//! Permission system for child webviews
//!
//! Tracks which apps have permission to perform sensitive operations.
//! Permissions are scoped per app origin (URL).

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;
use tracing::warn;

/// Permission types for Nostr operations
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PermissionType {
    /// Get public key (always allowed - never exposes nsec)
    GetPublicKey,
    /// Sign an event
    SignEvent,
    /// Encrypt data (NIP-44)
    Encrypt,
    /// Decrypt data (NIP-44)
    Decrypt,
    /// Read events (with optional kind filter)
    ReadEvents { kinds: Option<Vec<u16>> },
    /// Publish events (with optional kind filter)
    PublishEvent { kinds: Option<Vec<u16>> },
}

/// Permission store - manages permission state
#[derive(Clone)]
pub struct PermissionStore {
    /// In-memory cache of permissions: app_origin -> (permission_type -> granted)
    cache: Arc<RwLock<HashMap<String, HashMap<PermissionType, bool>>>>,
    /// Origins that should never prompt again.
    blocked_origins: Arc<RwLock<HashSet<String>>>,
    /// Path to persist permissions (optional)
    storage_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredPermissionEntry {
    origin: String,
    permission_type: PermissionType,
    granted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct StoredPermissionsFile {
    #[serde(default)]
    blocked_origins: Vec<String>,
    #[serde(default)]
    entries: Vec<StoredPermissionEntry>,
}

#[derive(Default)]
struct LoadedPermissionsState {
    cache: HashMap<String, HashMap<PermissionType, bool>>,
    blocked_origins: HashSet<String>,
}

impl PermissionStore {
    /// Create a new permission store
    pub fn new(storage_path: Option<PathBuf>) -> Self {
        let initial_state = storage_path
            .as_deref()
            .map(load_permissions)
            .transpose()
            .unwrap_or_else(|error| {
                warn!("Failed to load persisted permissions: {}", error);
                None
            })
            .unwrap_or_default();
        Self {
            cache: Arc::new(RwLock::new(initial_state.cache)),
            blocked_origins: Arc::new(RwLock::new(initial_state.blocked_origins)),
            storage_path,
        }
    }

    /// Check if a permission is granted
    pub async fn is_granted(
        &self,
        app_origin: &str,
        permission_type: &PermissionType,
    ) -> Option<bool> {
        let cache = self.cache.read().await;
        cache
            .get(app_origin)
            .and_then(|perms| perms.get(permission_type))
            .copied()
    }

    pub async fn is_origin_blocked(&self, app_origin: &str) -> bool {
        self.blocked_origins.read().await.contains(app_origin)
    }

    /// Check if we need to prompt for a permission
    pub async fn needs_prompt(&self, app_origin: &str, permission_type: &PermissionType) -> bool {
        self.is_granted(app_origin, permission_type).await.is_none()
    }

    /// Grant a permission
    pub async fn grant(&self, app_origin: &str, permission_type: PermissionType, persistent: bool) {
        info!(
            "Granting permission {:?} to {}",
            permission_type, app_origin
        );
        let mut cache = self.cache.write().await;
        cache
            .entry(app_origin.to_string())
            .or_default()
            .insert(permission_type, true);
        if persistent {
            if let Some(storage_path) = self.storage_path.as_deref() {
                let blocked_origins = self.blocked_origins.read().await;
                if let Err(error) = persist_permissions(storage_path, &cache, &blocked_origins) {
                    warn!("Failed to persist granted permission: {}", error);
                }
            }
        }
    }

    /// Deny a permission
    pub async fn deny(&self, app_origin: &str, permission_type: PermissionType, persistent: bool) {
        info!("Denying permission {:?} to {}", permission_type, app_origin);
        let mut cache = self.cache.write().await;
        cache
            .entry(app_origin.to_string())
            .or_default()
            .insert(permission_type, false);
        if persistent {
            if let Some(storage_path) = self.storage_path.as_deref() {
                let blocked_origins = self.blocked_origins.read().await;
                if let Err(error) = persist_permissions(storage_path, &cache, &blocked_origins) {
                    warn!("Failed to persist denied permission: {}", error);
                }
            }
        }
    }

    /// Block all NIP-07 prompts for an origin.
    pub async fn block_origin(&self, app_origin: &str) {
        info!("Blocking all permissions for {}", app_origin);
        self.cache.write().await.remove(app_origin);
        self.blocked_origins
            .write()
            .await
            .insert(app_origin.to_string());
        if let Some(storage_path) = self.storage_path.as_deref() {
            let cache = self.cache.read().await;
            let blocked_origins = self.blocked_origins.read().await;
            if let Err(error) = persist_permissions(storage_path, &cache, &blocked_origins) {
                warn!("Failed to persist blocked origin: {}", error);
            }
        }
    }

    /// Revoke all permissions for an app
    pub async fn revoke_all(&self, app_origin: &str) {
        info!("Revoking all permissions for {}", app_origin);
        let mut cache = self.cache.write().await;
        cache.remove(app_origin);
        self.blocked_origins.write().await.remove(app_origin);
        if let Some(storage_path) = self.storage_path.as_deref() {
            let blocked_origins = self.blocked_origins.read().await;
            if let Err(error) = persist_permissions(storage_path, &cache, &blocked_origins) {
                warn!("Failed to persist revoked permissions: {}", error);
            }
        }
    }

    /// Get all permissions for an app
    pub async fn get_permissions(&self, app_origin: &str) -> HashMap<PermissionType, bool> {
        let cache = self.cache.read().await;
        cache.get(app_origin).cloned().unwrap_or_default()
    }
}

fn load_permissions(storage_path: &Path) -> Result<LoadedPermissionsState, String> {
    if !storage_path.exists() {
        return Ok(LoadedPermissionsState::default());
    }

    let raw = std::fs::read_to_string(storage_path)
        .map_err(|error| format!("failed to read permission storage: {error}"))?;
    let stored = serde_json::from_str::<StoredPermissionsFile>(&raw)
        .or_else(|_| {
            serde_json::from_str::<Vec<StoredPermissionEntry>>(&raw).map(|entries| {
                StoredPermissionsFile {
                    blocked_origins: Vec::new(),
                    entries,
                }
            })
        })
        .map_err(|error| format!("failed to parse permission storage: {error}"))?;

    let mut cache: HashMap<String, HashMap<PermissionType, bool>> = HashMap::new();
    for entry in stored.entries {
        cache
            .entry(entry.origin)
            .or_default()
            .insert(entry.permission_type, entry.granted);
    }
    Ok(LoadedPermissionsState {
        cache,
        blocked_origins: stored.blocked_origins.into_iter().collect(),
    })
}

fn persist_permissions(
    storage_path: &Path,
    cache: &HashMap<String, HashMap<PermissionType, bool>>,
    blocked_origins: &HashSet<String>,
) -> Result<(), String> {
    if let Some(parent) = storage_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create permission directory: {error}"))?;
    }

    let mut entries = Vec::new();
    for (origin, permissions) in cache {
        for (permission_type, granted) in permissions {
            entries.push(StoredPermissionEntry {
                origin: origin.clone(),
                permission_type: permission_type.clone(),
                granted: *granted,
            });
        }
    }
    entries.sort_by(|left, right| {
        left.origin.cmp(&right.origin).then_with(|| {
            format!("{:?}", left.permission_type).cmp(&format!("{:?}", right.permission_type))
        })
    });

    let mut blocked_origins = blocked_origins.iter().cloned().collect::<Vec<_>>();
    blocked_origins.sort();

    let serialized = serde_json::to_vec_pretty(&StoredPermissionsFile {
        blocked_origins,
        entries,
    })
    .map_err(|error| format!("failed to serialize permissions: {error}"))?;
    std::fs::write(storage_path, serialized)
        .map_err(|error| format!("failed to write permission storage: {error}"))
}

impl Default for PermissionStore {
    fn default() -> Self {
        Self::new(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_get_public_key_needs_prompt_until_granted() {
        let store = PermissionStore::new(None);
        let app = "http://example.com";
        assert_eq!(
            store.is_granted(app, &PermissionType::GetPublicKey).await,
            None
        );
        assert!(store.needs_prompt(app, &PermissionType::GetPublicKey).await);

        store.grant(app, PermissionType::GetPublicKey, false).await;
        assert_eq!(
            store.is_granted(app, &PermissionType::GetPublicKey).await,
            Some(true)
        );
    }

    #[tokio::test]
    async fn test_sign_event_needs_prompt() {
        let store = PermissionStore::new(None);
        let app = "http://example.com";
        assert!(store
            .is_granted(app, &PermissionType::SignEvent)
            .await
            .is_none());
        assert!(store.needs_prompt(app, &PermissionType::SignEvent).await);
    }

    #[tokio::test]
    async fn test_grant_permission() {
        let store = PermissionStore::new(None);
        let app = "http://example.com";
        store.grant(app, PermissionType::SignEvent, false).await;
        assert_eq!(
            store.is_granted(app, &PermissionType::SignEvent).await,
            Some(true)
        );
    }

    #[tokio::test]
    async fn test_permissions_scoped_by_app() {
        let store = PermissionStore::new(None);
        let app1 = "http://app1.com";
        let app2 = "http://app2.com";
        store.grant(app1, PermissionType::SignEvent, false).await;
        assert_eq!(
            store.is_granted(app1, &PermissionType::SignEvent).await,
            Some(true)
        );
        assert!(store
            .is_granted(app2, &PermissionType::SignEvent)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn test_revoke_all() {
        let store = PermissionStore::new(None);
        let app = "http://example.com";
        store.grant(app, PermissionType::SignEvent, false).await;
        store.grant(app, PermissionType::Encrypt, false).await;
        store.revoke_all(app).await;
        assert!(store.needs_prompt(app, &PermissionType::SignEvent).await);
        assert!(store.needs_prompt(app, &PermissionType::Encrypt).await);
    }
}
