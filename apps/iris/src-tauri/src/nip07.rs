//! NIP-07 webview support for child webviews
//!
//! Provides window.nostr capability for child webviews.
//! The shell exposes a native account-backed NIP-07 signer to the main
//! window and to child webviews.
#![cfg_attr(any(target_os = "android", target_os = "ios"), allow(dead_code))]

use crate::permissions::{PermissionStore, PermissionType};
use axum::body::{Body, Bytes};
use axum::extract::{ws::WebSocketUpgrade, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::response::Response as AxumResponse;
#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
use hashtree_cli::server::register_virtual_tree_host;
use nostr_sdk::{
    nips::{nip04, nip44},
    Keys, PublicKey, ToBech32, UnsignedEvent,
};
use once_cell::sync::OnceCell;
use parking_lot::RwLock;
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager, Runtime, Theme, WebviewUrl};
#[cfg(any(target_os = "macos", windows))]
use tauri::{LogicalPosition, LogicalSize};
#[cfg(target_os = "linux")]
use tauri::{PhysicalPosition, PhysicalSize};
#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
use tauri::{Rect, WebviewBuilder};
#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
use tauri_plugin_dialog::{
    DialogExt, MessageDialogButtons, MessageDialogKind, MessageDialogResult,
};
#[cfg(any(target_os = "android", target_os = "ios"))]
use tauri_plugin_iris_mobile_browser::{
    BrowserBoundsRequest, BrowserCreateRequest, MobileBrowserExt, ShellOverlayRequest,
};
use tauri_plugin_secure_storage::{OptionsRequest, SecureStorageExt};
use tokio::sync::{oneshot, Mutex};
use tracing::{debug, error, info, warn};

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
const MOBILE_CHILD_WEBVIEWS_UNSUPPORTED: &str = "Mobile child webviews are not supported yet";
const IRIS_CONFIDENTIAL_STORAGE_KEY: &str = "iris-confidential";
#[cfg(debug_assertions)]
const IRIS_INSECURE_DEV_CONFIDENTIAL_ENV: &str = "IRIS_INSECURE_DEV_CONFIDENTIAL";

// ============================================
// htree:// URL helpers for origin isolation
// ============================================

pub fn htree_origin_from_nhash(nhash: &str) -> String {
    htree_url_from_nhash(nhash, "/")
        .trim_end_matches('/')
        .to_string()
}

pub fn htree_origin_from_tree_host(host: &str, treename: &str) -> String {
    htree_url_from_tree_host(host, treename, "/")
        .trim_end_matches('/')
        .to_string()
}

#[cfg(any(target_os = "linux", test))]
fn normalized_child_webview_scale(scale: Option<f64>) -> f64 {
    scale
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(1.0)
}

#[cfg(any(target_os = "linux", test))]
fn scaled_child_webview_dimensions(
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    scale: Option<f64>,
) -> (f64, f64, f64, f64) {
    let scale = normalized_child_webview_scale(scale);
    (
        (x * scale).round(),
        (y * scale).round(),
        (width.max(0.0) * scale).round(),
        (height.max(0.0) * scale).round(),
    )
}

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
fn desktop_child_webview_bounds(
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    scale: Option<f64>,
) -> Rect {
    #[cfg(target_os = "linux")]
    {
        let (x, y, width, height) = scaled_child_webview_dimensions(x, y, width, height, scale);
        return Rect {
            position: PhysicalPosition::new(x, y).into(),
            size: PhysicalSize::new(width, height).into(),
        };
    }

    #[cfg(any(target_os = "macos", windows))]
    {
        let _ = scale;
        Rect {
            position: LogicalPosition::new(x, y).into(),
            size: LogicalSize::new(width.max(0.0), height.max(0.0)).into(),
        }
    }
}

fn decode_url_component(value: &str) -> String {
    percent_decode_str(value).decode_utf8_lossy().into_owned()
}

fn child_webview_placeholder_background_color(theme: Theme) -> tauri::utils::config::Color {
    match theme {
        Theme::Dark => tauri::utils::config::Color(24, 24, 24, 255),
        Theme::Light => tauri::utils::config::Color(235, 235, 235, 255),
        #[allow(unreachable_patterns)]
        _ => tauri::utils::config::Color(235, 235, 235, 255),
    }
}

fn decode_path_segments(path: &str) -> Vec<String> {
    path.trim_start_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(decode_url_component)
        .collect()
}

fn htree_url_with_segments(host: &str, segments: &[String]) -> String {
    let mut url = tauri::Url::parse(&format!("htree://{}/", host)).expect("valid htree base URL");
    {
        let mut path_segments = url
            .path_segments_mut()
            .expect("htree URL should support path segments");
        path_segments.pop_if_empty();
        for segment in segments {
            path_segments.push(segment);
        }
    }

    if segments.is_empty() {
        url.as_str().trim_end_matches('/').to_string()
    } else {
        url.into()
    }
}

fn htree_url_from_nhash(nhash: &str, path: &str) -> String {
    let segments = decode_path_segments(path);
    htree_url_with_segments(nhash, &segments)
}

fn htree_url_from_tree_host(host: &str, treename: &str, path: &str) -> String {
    let mut segments = vec![decode_url_component(treename)];
    let path_segments = decode_path_segments(path);
    let is_tree_root = path_segments.is_empty();
    segments.extend(path_segments);
    let url = htree_url_with_segments(host, &segments);
    if is_tree_root {
        format!("{}/", url)
    } else {
        url
    }
}

fn http_url_with_segments(base: &str, segments: &[String]) -> Result<String, String> {
    let mut url = tauri::Url::parse(base).map_err(|e| format!("Invalid base URL: {}", e))?;
    {
        let mut path_segments = url
            .path_segments_mut()
            .map_err(|_| "Base URL does not support path segments".to_string())?;
        path_segments.pop_if_empty();
        for segment in segments {
            path_segments.push(segment);
        }
    }
    Ok(url.into())
}

fn isolated_loopback_scope_label(canonical_root: &str) -> String {
    let digest = hashtree_core::sha256(canonical_root.as_bytes());
    format!("tree-{}", hex::encode(&digest[..16]))
}

fn use_origin_isolated_loopback_hosts() -> bool {
    !cfg!(target_os = "linux")
}

fn loopback_server_url(
    server_url: &str,
    canonical_root: &str,
    use_origin_isolated_hosts: bool,
) -> Result<String, String> {
    let mut url = tauri::Url::parse(server_url).map_err(|e| format!("Invalid base URL: {}", e))?;
    if use_origin_isolated_hosts {
        let isolated_host = format!(
            "{}.htree.localhost",
            isolated_loopback_scope_label(canonical_root)
        );
        url.set_host(Some(&isolated_host))
            .map_err(|e| format!("Failed to set isolated host: {}", e))?;
    }
    Ok(url.into())
}

fn daemon_proxy_url_from_nhash(
    server_url: &str,
    nhash: &str,
    path: &str,
    use_origin_isolated_hosts: bool,
) -> Result<String, String> {
    let canonical_root = htree_origin_from_nhash(nhash);
    let loopback_server_url =
        loopback_server_url(server_url, &canonical_root, use_origin_isolated_hosts)?;
    let mut segments = vec!["htree".to_string(), decode_url_component(nhash)];
    let path_segments = decode_path_segments(path);
    let is_tree_root = path_segments.is_empty();
    segments.extend(path_segments);
    let url = http_url_with_segments(&loopback_server_url, &segments)?;
    if is_tree_root {
        Ok(format!("{}/", url.trim_end_matches('/')))
    } else {
        Ok(url)
    }
}

fn daemon_proxy_url_from_tree_host(
    server_url: &str,
    host: &str,
    treename: &str,
    path: &str,
    use_origin_isolated_hosts: bool,
) -> Result<String, String> {
    let canonical_root = htree_origin_from_tree_host(host, treename);
    let loopback_server_url =
        loopback_server_url(server_url, &canonical_root, use_origin_isolated_hosts)?;
    let mut segments = vec![
        "htree".to_string(),
        decode_url_component(host),
        decode_url_component(treename),
    ];
    let path_segments = decode_path_segments(path);
    let is_tree_root = path_segments.is_empty();
    segments.extend(path_segments);
    let url = http_url_with_segments(&loopback_server_url, &segments)?;
    if is_tree_root {
        Ok(format!("{}/", url.trim_end_matches('/')))
    } else {
        Ok(url)
    }
}

fn append_query(mut url: String, query: Option<&str>) -> String {
    if let Some(query) = query.map(str::trim).filter(|value| !value.is_empty()) {
        url.push('?');
        url.push_str(query);
    }
    url
}

fn append_fragment(mut url: String, fragment: Option<&str>) -> String {
    if let Some(fragment) = fragment
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.trim_start_matches('#'))
    {
        url.push('#');
        url.push_str(fragment);
    }
    url
}

fn append_query_params(url: &str, params: &[(&str, &str)]) -> Result<String, String> {
    let mut parsed = tauri::Url::parse(url).map_err(|e| format!("Invalid URL: {}", e))?;
    {
        let mut query_pairs = parsed.query_pairs_mut();
        for (key, value) in params {
            query_pairs.append_pair(key, value);
        }
    }
    Ok(parsed.into())
}

fn append_internal_htree_query_params(
    url: &str,
    server_url: &str,
    canonical_url: &str,
    session_token: &str,
    cache_bust: Option<&str>,
) -> Result<String, String> {
    let mut params = vec![
        ("iris_htree_server", server_url),
        ("iris_htree_canonical", canonical_url),
        ("iris_htree_session", session_token),
    ];
    if let Some(cache_bust) = cache_bust.map(str::trim).filter(|value| !value.is_empty()) {
        params.push(("iris_htree_root", cache_bust));
    }
    append_query_params(url, &params)
}

fn resolve_tree_request_host<'a>(
    request_host: &'a str,
    self_npub: Option<&'a str>,
) -> Result<&'a str, String> {
    if request_host == "self" {
        self_npub.ok_or_else(|| "self identity is not available".to_string())
    } else {
        Ok(request_host)
    }
}

fn webview_url_for_parsed_url(url: &tauri::Url) -> WebviewUrl {
    match url.scheme() {
        "http" | "https" => WebviewUrl::External(url.clone()),
        _ => WebviewUrl::CustomProtocol(url.clone()),
    }
}

#[cfg(any(target_os = "android", target_os = "ios"))]
fn url_origin(url: &tauri::Url) -> Option<String> {
    let scheme = url.scheme();
    let host = url.host_str()?;
    let port = url.port();

    Some(if let Some(port) = port {
        format!("{scheme}://{host}:{port}")
    } else {
        format!("{scheme}://{host}")
    })
}

fn inject_child_init_script<R: Runtime>(
    app: &AppHandle<R>,
    label: &str,
    script: &str,
    context: &str,
) {
    let Some(webview) = app.get_webview(label) else {
        warn!(
            "[child-webview:{}] Missing webview while injecting bridge script during {}",
            label, context
        );
        return;
    };

    match webview.eval(script) {
        Ok(()) => {
            debug!(
                "[child-webview:{}] Injected bridge script during {}",
                label, context
            );
        }
        Err(error) => {
            warn!(
                "[child-webview:{}] Failed to inject bridge script during {}: {}",
                label, context, error
            );
        }
    }
}

fn schedule_child_init_script_retry<R: Runtime + 'static>(
    app: AppHandle<R>,
    label: String,
    script: String,
    delay: Duration,
    context: String,
) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(delay).await;
        inject_child_init_script(&app, &label, &script, &context);
    });
}

fn tauri_response_to_axum(response: tauri::http::Response<Vec<u8>>) -> AxumResponse<Body> {
    let status = StatusCode::from_u16(response.status().as_u16())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut builder = AxumResponse::builder().status(status);
    for (name, value) in response.headers() {
        builder = builder.header(name, value);
    }
    builder
        .body(Body::from(response.into_body()))
        .unwrap_or_else(|_| AxumResponse::new(Body::from("bridge response build failed")))
}

// ============================================
// Global state
// ============================================

static GLOBAL_NIP07_STATE: OnceCell<Arc<Nip07State>> = OnceCell::new();

pub fn init_global_state(nip07: Arc<Nip07State>) {
    let _ = GLOBAL_NIP07_STATE.set(nip07);
}

pub fn get_nip07_state() -> Option<Arc<Nip07State>> {
    GLOBAL_NIP07_STATE.get().cloned()
}

// ============================================
// State types
// ============================================

pub(crate) trait ConfidentialStore: Send + Sync {
    fn load_blob(&self) -> Result<Option<ConfidentialBlob>, String>;
    fn save_blob(&self, blob: &ConfidentialBlob) -> Result<(), String>;
    fn clear_blob(&self) -> Result<(), String>;
}

pub(crate) fn confidential_store(
    app: AppHandle,
    insecure_dev_path: Option<PathBuf>,
) -> Arc<dyn ConfidentialStore> {
    #[cfg(not(debug_assertions))]
    let _ = insecure_dev_path;

    #[cfg(debug_assertions)]
    if insecure_dev_confidential_store_enabled() {
        if let Some(path) = insecure_dev_path {
            warn!(
                "Using insecure plaintext confidential storage at {:?} because {} is enabled",
                path, IRIS_INSECURE_DEV_CONFIDENTIAL_ENV
            );
            return Arc::new(FileConfidentialStore::new(app, path));
        }
    }

    Arc::new(SystemConfidentialStore::new(app))
}

struct SystemConfidentialStore {
    app: AppHandle,
    cache: RwLock<ConfidentialBlobCache>,
}

#[derive(Debug, Clone)]
enum ConfidentialBlobCache {
    Unknown,
    Missing,
    Loaded(ConfidentialBlob),
}

impl SystemConfidentialStore {
    fn new(app: AppHandle) -> Self {
        Self {
            app,
            cache: RwLock::new(ConfidentialBlobCache::Unknown),
        }
    }

    fn blob_request(&self, data: Option<String>) -> OptionsRequest {
        OptionsRequest {
            prefixed_key: Some(IRIS_CONFIDENTIAL_STORAGE_KEY.to_string()),
            data,
            sync: Some(false),
            keychain_access: None,
        }
    }
}

impl ConfidentialStore for SystemConfidentialStore {
    fn load_blob(&self) -> Result<Option<ConfidentialBlob>, String> {
        match &*self.cache.read() {
            ConfidentialBlobCache::Loaded(blob) => return Ok(Some(blob.clone())),
            ConfidentialBlobCache::Missing => return Ok(None),
            ConfidentialBlobCache::Unknown => {}
        }

        let payload = self
            .app
            .secure_storage()
            .get_item(self.app.clone(), self.blob_request(None))
            .map_err(|error| format!("Failed to read confidential storage: {error}"))?;
        let blob = payload
            .data
            .filter(|value| !value.trim().is_empty())
            .map(|value| {
                serde_json::from_str::<ConfidentialBlob>(&value).map_err(|error| {
                    format!("Failed to parse confidential storage payload: {error}")
                })
            })
            .transpose()?;
        *self.cache.write() = match &blob {
            Some(blob) => ConfidentialBlobCache::Loaded(blob.clone()),
            None => ConfidentialBlobCache::Missing,
        };
        Ok(blob)
    }

    fn save_blob(&self, blob: &ConfidentialBlob) -> Result<(), String> {
        let data = serde_json::to_string(blob).map_err(|error| {
            format!("Failed to serialize confidential storage payload: {error}")
        })?;
        self.app
            .secure_storage()
            .set_item(self.app.clone(), self.blob_request(Some(data)))
            .map(|_| ())
            .map_err(|error| format!("Failed to write confidential storage: {error}"))?;
        *self.cache.write() = ConfidentialBlobCache::Loaded(blob.clone());
        Ok(())
    }

    fn clear_blob(&self) -> Result<(), String> {
        self.app
            .secure_storage()
            .remove_item(self.app.clone(), self.blob_request(None))
            .map(|_| ())
            .map_err(|error| format!("Failed to remove confidential storage: {error}"))?;
        *self.cache.write() = ConfidentialBlobCache::Missing;
        Ok(())
    }
}

#[cfg(debug_assertions)]
fn insecure_dev_confidential_store_enabled() -> bool {
    std::env::var(IRIS_INSECURE_DEV_CONFIDENTIAL_ENV)
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

#[cfg(debug_assertions)]
struct FileConfidentialStore {
    path: PathBuf,
    cache: RwLock<ConfidentialBlobCache>,
}

#[cfg(debug_assertions)]
impl FileConfidentialStore {
    fn new(_app: AppHandle, path: PathBuf) -> Self {
        Self {
            path,
            cache: RwLock::new(ConfidentialBlobCache::Unknown),
        }
    }

    fn read_blob_from_disk(&self) -> Result<Option<ConfidentialBlob>, String> {
        if !self.path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&self.path)
            .map_err(|error| format!("Failed to read insecure confidential storage: {error}"))?;
        if raw.trim().is_empty() {
            return Ok(None);
        }
        serde_json::from_str(&raw)
            .map(Some)
            .map_err(|error| format!("Failed to parse insecure confidential storage: {error}"))
    }

    fn write_blob_to_disk(&self, blob: &ConfidentialBlob) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                format!("Failed to create insecure confidential storage directory: {error}")
            })?;
        }
        let serialized = serde_json::to_vec_pretty(blob).map_err(|error| {
            format!("Failed to serialize insecure confidential storage payload: {error}")
        })?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&self.path)
            .map_err(|error| format!("Failed to open insecure confidential storage: {error}"))?;
        file.write_all(&serialized)
            .and_then(|_| file.flush())
            .map_err(|error| format!("Failed to write insecure confidential storage: {error}"))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let permissions = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&self.path, permissions).map_err(|error| {
                format!("Failed to lock down insecure confidential storage permissions: {error}")
            })?;
        }

        Ok(())
    }
}

#[cfg(debug_assertions)]
impl ConfidentialStore for FileConfidentialStore {
    fn load_blob(&self) -> Result<Option<ConfidentialBlob>, String> {
        match &*self.cache.read() {
            ConfidentialBlobCache::Loaded(blob) => return Ok(Some(blob.clone())),
            ConfidentialBlobCache::Missing => return Ok(None),
            ConfidentialBlobCache::Unknown => {}
        }

        let blob = self.read_blob_from_disk()?;
        *self.cache.write() = match &blob {
            Some(blob) => ConfidentialBlobCache::Loaded(blob.clone()),
            None => ConfidentialBlobCache::Missing,
        };
        Ok(blob)
    }

    fn save_blob(&self, blob: &ConfidentialBlob) -> Result<(), String> {
        self.write_blob_to_disk(blob)?;
        *self.cache.write() = ConfidentialBlobCache::Loaded(blob.clone());
        Ok(())
    }

    fn clear_blob(&self) -> Result<(), String> {
        if self.path.exists() {
            std::fs::remove_file(&self.path).map_err(|error| {
                format!("Failed to remove insecure confidential storage: {error}")
            })?;
        }
        *self.cache.write() = ConfidentialBlobCache::Missing;
        Ok(())
    }
}

pub struct Nip07State {
    pub permissions: Arc<PermissionStore>,
    session_tokens: RwLock<HashMap<String, String>>,
    accounts: RwLock<Vec<ManagedNip07Account>>,
    active_pubkey: RwLock<Option<String>>,
    active_signer: RwLock<Option<Keys>>,
    storage_path: PathBuf,
    confidential_store: Arc<dyn ConfidentialStore>,
    permission_prompt_queue: Mutex<VecDeque<Nip07PermissionPrompt>>,
    pending_permission_prompts: Mutex<HashMap<String, Nip07PermissionPrompt>>,
    permission_prompt_waiters: Mutex<HashMap<String, oneshot::Sender<Nip07PermissionDecision>>>,
}

impl Nip07State {
    pub(crate) fn new(
        permissions: Arc<PermissionStore>,
        storage_path: PathBuf,
        confidential_store: Arc<dyn ConfidentialStore>,
    ) -> Self {
        let (accounts, active_pubkey) =
            load_nip07_accounts(&storage_path).unwrap_or_else(|error| {
                warn!(
                    "Failed to load NIP-07 accounts from {:?}: {}",
                    storage_path, error
                );
                (Vec::new(), None)
            });
        Self {
            permissions,
            session_tokens: RwLock::new(HashMap::new()),
            accounts: RwLock::new(accounts),
            active_pubkey: RwLock::new(active_pubkey),
            active_signer: RwLock::new(None),
            storage_path,
            confidential_store,
            permission_prompt_queue: Mutex::new(VecDeque::new()),
            pending_permission_prompts: Mutex::new(HashMap::new()),
            permission_prompt_waiters: Mutex::new(HashMap::new()),
        }
    }

    pub fn new_session(&self, origin: &str) -> String {
        let token = uuid::Uuid::new_v4().to_string();
        self.session_tokens
            .write()
            .insert(origin.to_string(), token.clone());
        token
    }

    pub fn validate_token(&self, origin: &str, token: &str) -> bool {
        self.session_tokens
            .read()
            .get(origin)
            .map(|t| t == token)
            .unwrap_or(false)
    }

    pub fn validate_any_token(&self, token: &str) -> bool {
        self.session_tokens
            .read()
            .values()
            .any(|value| value == token)
    }

    pub fn current_account(&self) -> Result<Option<Nip07AccountSummary>, String> {
        let accounts = self.accounts.read();
        let active_pubkey = self.active_pubkey.read().clone();
        accounts
            .iter()
            .find(|account| Some(account.pubkey.clone()) == active_pubkey)
            .map(nip07_account_summary_from_metadata)
            .transpose()
    }

    pub fn list_accounts(&self) -> Result<Nip07AccountsSummary, String> {
        let accounts = self
            .accounts
            .read()
            .iter()
            .map(nip07_account_summary_from_metadata)
            .collect::<Result<Vec<_>, _>>()?;
        let active_pubkey = self.active_pubkey.read().clone();
        Ok(Nip07AccountsSummary {
            accounts,
            active_pubkey,
        })
    }

    pub fn login_with_secret<S>(&self, secret: S) -> Result<Nip07AccountSummary, String>
    where
        S: AsRef<str>,
    {
        let keys = Keys::parse(secret.as_ref().trim())
            .map_err(|error| format!("Invalid Nostr secret key: {error}"))?;
        let nsec = keys
            .secret_key()
            .to_bech32()
            .map_err(|error| format!("Failed to encode Nostr secret key: {error}"))?;
        let pubkey_hex = keys.public_key().to_hex();
        let (next_accounts, next_active_pubkey) =
            next_accounts_after_upsert(self.accounts.read().clone(), pubkey_hex.clone());
        self.store_nip07_secret(&pubkey_hex, &nsec)?;
        persist_nip07_accounts(
            &self.storage_path,
            &next_accounts,
            next_active_pubkey.as_deref(),
        )?;
        let summary = next_accounts
            .iter()
            .find(|account| Some(account.pubkey.clone()) == next_active_pubkey)
            .map(nip07_account_summary_from_metadata)
            .transpose()?
            .ok_or_else(|| "Failed to activate Nostr account".to_string())?;
        *self.accounts.write() = next_accounts;
        *self.active_pubkey.write() = next_active_pubkey;
        *self.active_signer.write() = Some(keys);
        Ok(summary)
    }

    pub fn generate_account(&self) -> Result<Nip07AccountSummary, String> {
        let keys = Keys::generate();
        let nsec = keys
            .secret_key()
            .to_bech32()
            .map_err(|error| format!("Failed to encode generated Nostr secret key: {error}"))?;
        let pubkey_hex = keys.public_key().to_hex();
        let (next_accounts, next_active_pubkey) =
            next_accounts_after_upsert(self.accounts.read().clone(), pubkey_hex.clone());
        self.store_nip07_secret(&pubkey_hex, &nsec)?;
        persist_nip07_accounts(
            &self.storage_path,
            &next_accounts,
            next_active_pubkey.as_deref(),
        )?;
        let summary = next_accounts
            .iter()
            .find(|account| Some(account.pubkey.clone()) == next_active_pubkey)
            .map(nip07_account_summary_from_metadata)
            .transpose()?
            .ok_or_else(|| "Failed to activate generated Nostr account".to_string())?;
        *self.accounts.write() = next_accounts;
        *self.active_pubkey.write() = next_active_pubkey;
        *self.active_signer.write() = Some(keys);
        Ok(summary)
    }

    pub fn logout(&self) -> Result<(), String> {
        let current_accounts = self.accounts.read().clone();
        clear_nip07_account(&self.storage_path)?;
        *self.accounts.write() = Vec::new();
        *self.active_pubkey.write() = None;
        *self.active_signer.write() = None;
        self.remove_nip07_secrets(&current_accounts)
    }

    pub fn set_active_account<S>(&self, pubkey: S) -> Result<Nip07AccountSummary, String>
    where
        S: AsRef<str>,
    {
        let requested_pubkey = pubkey.as_ref().trim();
        let accounts = self.accounts.read().clone();
        let next_active_pubkey = accounts
            .iter()
            .find(|account| account.pubkey == requested_pubkey)
            .map(|account| account.pubkey.clone())
            .ok_or_else(|| "Nostr account not found".to_string())?;
        persist_nip07_accounts(
            &self.storage_path,
            &accounts,
            Some(next_active_pubkey.as_str()),
        )?;
        let summary = accounts
            .iter()
            .find(|account| account.pubkey == next_active_pubkey)
            .map(nip07_account_summary_from_metadata)
            .transpose()?
            .ok_or_else(|| "Nostr account not found".to_string())?;
        *self.active_pubkey.write() = Some(next_active_pubkey);
        *self.active_signer.write() = None;
        Ok(summary)
    }

    pub fn remove_account<S>(&self, pubkey: S) -> Result<Nip07AccountsSummary, String>
    where
        S: AsRef<str>,
    {
        let requested_pubkey = pubkey.as_ref().trim();
        let current_accounts = self.accounts.read().clone();
        if !current_accounts
            .iter()
            .any(|account| account.pubkey == requested_pubkey)
        {
            return Err("Nostr account not found".to_string());
        }

        let filtered_accounts: Vec<_> = current_accounts
            .into_iter()
            .filter(|account| account.pubkey != requested_pubkey)
            .collect();
        let requested_active_pubkey = self
            .active_pubkey
            .read()
            .clone()
            .filter(|active_pubkey| active_pubkey != requested_pubkey);
        let (next_accounts, next_active_pubkey) =
            normalize_accounts_state(filtered_accounts, requested_active_pubkey);

        persist_nip07_accounts(
            &self.storage_path,
            &next_accounts,
            next_active_pubkey.as_deref(),
        )?;
        *self.accounts.write() = next_accounts;
        *self.active_pubkey.write() = next_active_pubkey;
        *self.active_signer.write() = None;
        if let Err(error) = self.remove_nip07_secret(requested_pubkey) {
            warn!(
                "Failed to remove secure NIP-07 secret for {}: {}",
                requested_pubkey, error
            );
        }
        self.list_accounts()
    }

    pub fn export_account_secret<S>(&self, pubkey: S) -> Result<String, String>
    where
        S: AsRef<str>,
    {
        let requested_pubkey = pubkey.as_ref().trim();
        if requested_pubkey.is_empty() {
            return Err("Nostr account not found".to_string());
        }
        if !self
            .accounts
            .read()
            .iter()
            .any(|account| account.pubkey == requested_pubkey)
        {
            return Err("Nostr account not found".to_string());
        }
        self.load_nip07_secret(requested_pubkey)?
            .ok_or_else(|| format!("Secure Nostr secret missing for account {requested_pubkey}"))
    }

    fn signer_keys(&self) -> Result<Keys, String> {
        let active_pubkey = self
            .active_pubkey
            .read()
            .clone()
            .ok_or_else(|| "No Nostr account signed in".to_string())?;
        if let Some(keys) = self.active_signer.read().clone() {
            if keys.public_key().to_hex() == active_pubkey {
                return Ok(keys);
            }
        }
        let secret = self.load_nip07_secret(&active_pubkey)?.ok_or_else(|| {
            format!("Secure Nostr secret missing for active account {active_pubkey}")
        })?;
        let keys = Keys::parse(secret.trim())
            .map_err(|error| format!("Secure storage contains an invalid secret key: {error}"))?;
        let derived_pubkey = keys.public_key().to_hex();
        if derived_pubkey != active_pubkey {
            return Err(format!(
                "Secure storage secret for {active_pubkey} belongs to a different pubkey ({derived_pubkey})"
            ));
        }
        *self.active_signer.write() = Some(keys.clone());
        Ok(keys)
    }

    fn load_nip07_secret(&self, pubkey: &str) -> Result<Option<String>, String> {
        Ok(self
            .confidential_store
            .load_blob()?
            .and_then(|blob| confidential_blob_nsec(&blob, pubkey)))
    }

    fn store_nip07_secret(&self, pubkey: &str, nsec: &str) -> Result<(), String> {
        let mut blob = self.confidential_store.load_blob()?.unwrap_or_default();
        upsert_confidential_blob_nsec(&mut blob, pubkey, nsec);
        self.confidential_store.save_blob(&blob)
    }

    fn remove_nip07_secret(&self, pubkey: &str) -> Result<(), String> {
        let Some(mut blob) = self.confidential_store.load_blob()? else {
            return Ok(());
        };
        if !remove_confidential_blob_nsec(&mut blob, pubkey) {
            return Ok(());
        }
        if confidential_blob_is_empty(&blob) {
            self.confidential_store.clear_blob()
        } else {
            self.confidential_store.save_blob(&blob)
        }
    }

    fn remove_nip07_secrets(&self, accounts: &[ManagedNip07Account]) -> Result<(), String> {
        let Some(mut blob) = self.confidential_store.load_blob()? else {
            return Ok(());
        };
        let mut changed = false;
        for account in accounts {
            changed |= remove_confidential_blob_nsec(&mut blob, &account.pubkey);
        }
        if !changed {
            return Ok(());
        }
        if confidential_blob_is_empty(&blob) {
            self.confidential_store.clear_blob()
        } else {
            self.confidential_store.save_blob(&blob)
        }
    }

    pub async fn take_permission_prompt(&self) -> Option<Nip07PermissionPrompt> {
        self.permission_prompt_queue.lock().await.pop_front()
    }

    pub async fn pending_permission_prompts(&self) -> Vec<Nip07PermissionPrompt> {
        let mut prompts = self
            .pending_permission_prompts
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        prompts.sort_by(|left, right| left.request_id.cmp(&right.request_id));
        prompts
    }

    pub async fn resolve_permission_prompt(
        &self,
        request_id: &str,
        decision: Nip07PermissionDecisionAction,
    ) -> Result<(), String> {
        let Some(waiter) = self
            .permission_prompt_waiters
            .lock()
            .await
            .remove(request_id)
        else {
            return Err("Permission prompt was no longer pending".to_string());
        };
        self.pending_permission_prompts
            .lock()
            .await
            .remove(request_id);
        self.permission_prompt_queue
            .lock()
            .await
            .retain(|prompt| prompt.request_id != request_id);

        waiter
            .send(Nip07PermissionDecision { decision })
            .map_err(|_| "Permission prompt receiver dropped".to_string())
    }

    async fn request_permission(
        &self,
        origin: &str,
        permission_type: PermissionType,
        method: &str,
    ) -> Result<bool, String> {
        if self.permissions.is_origin_blocked(origin).await {
            info!(
                "[NIP-07] Permission request for {} from {} blocked by stored site block",
                method, origin
            );
            return Ok(false);
        }

        if let Some(granted) = self.permissions.is_granted(origin, &permission_type).await {
            info!(
                "[NIP-07] Reusing stored permission for {} from {}: {}",
                method,
                origin,
                if granted { "allow" } else { "deny" }
            );
            return Ok(granted);
        }

        let request_id = uuid::Uuid::new_v4().to_string();
        let (sender, receiver) = oneshot::channel();
        self.permission_prompt_waiters
            .lock()
            .await
            .insert(request_id.clone(), sender);
        let prompt = Nip07PermissionPrompt {
            request_id: request_id.clone(),
            origin: origin.to_string(),
            method: method.to_string(),
        };
        self.pending_permission_prompts
            .lock()
            .await
            .insert(request_id.clone(), prompt.clone());
        self.permission_prompt_queue.lock().await.push_back(prompt);
        info!(
            "[NIP-07] Queued permission prompt {} for {} from {}",
            request_id, method, origin
        );

        let decision = match tokio::time::timeout(Duration::from_secs(120), receiver).await {
            Ok(Ok(decision)) => decision,
            Ok(Err(_)) => {
                return Err("Permission prompt receiver dropped".to_string());
            }
            Err(_) => {
                self.permission_prompt_waiters
                    .lock()
                    .await
                    .remove(&request_id);
                self.pending_permission_prompts
                    .lock()
                    .await
                    .remove(&request_id);
                self.permission_prompt_queue
                    .lock()
                    .await
                    .retain(|prompt| prompt.request_id != request_id);
                warn!(
                    "[NIP-07] Permission prompt {} for {} from {} timed out",
                    request_id, method, origin
                );
                return Err("Permission prompt timed out".to_string());
            }
        };

        info!(
            "[NIP-07] Permission prompt {} for {} from {} resolved as {:?}",
            request_id, method, origin, decision.decision
        );
        match decision.decision {
            Nip07PermissionDecisionAction::Deny => Ok(false),
            Nip07PermissionDecisionAction::AllowSession => {
                self.permissions.grant(origin, permission_type, false).await;
                Ok(true)
            }
            Nip07PermissionDecisionAction::AllowAlways => {
                self.permissions.grant(origin, permission_type, true).await;
                Ok(true)
            }
            Nip07PermissionDecisionAction::BlockSite => {
                self.permissions.block_origin(origin).await;
                Ok(false)
            }
        }
    }
}

fn current_unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn confidential_blob_version() -> u32 {
    1
}

fn confidential_blob_nsec(blob: &ConfidentialBlob, pubkey: &str) -> Option<String> {
    blob.nip07.accounts.get(pubkey.trim()).cloned()
}

fn upsert_confidential_blob_nsec(blob: &mut ConfidentialBlob, pubkey: &str, nsec: &str) {
    blob.nip07
        .accounts
        .insert(pubkey.trim().to_string(), nsec.to_string());
}

fn remove_confidential_blob_nsec(blob: &mut ConfidentialBlob, pubkey: &str) -> bool {
    blob.nip07.accounts.remove(pubkey.trim()).is_some()
}

fn confidential_blob_is_empty(blob: &ConfidentialBlob) -> bool {
    blob.nip07.accounts.is_empty() && blob.extra.is_empty()
}

fn nip07_account_summary_from_metadata(
    account: &ManagedNip07Account,
) -> Result<Nip07AccountSummary, String> {
    let public_key = PublicKey::parse(&account.pubkey)
        .map_err(|error| format!("Stored account has an invalid pubkey: {error}"))?;
    let npub = public_key
        .to_bech32()
        .map_err(|error| format!("Failed to encode account npub: {error}"))?;
    Ok(Nip07AccountSummary {
        pubkey: account.pubkey.clone(),
        npub,
        added_at: account.added_at,
    })
}

fn normalize_accounts_state(
    accounts: Vec<ManagedNip07Account>,
    active_pubkey: Option<String>,
) -> (Vec<ManagedNip07Account>, Option<String>) {
    let mut seen_pubkeys = HashSet::new();
    let deduped_accounts: Vec<_> = accounts
        .into_iter()
        .filter(|account| seen_pubkeys.insert(account.pubkey.clone()))
        .collect();
    let normalized_active_pubkey = active_pubkey
        .filter(|candidate| {
            deduped_accounts
                .iter()
                .any(|account| account.pubkey == *candidate)
        })
        .or_else(|| {
            deduped_accounts
                .first()
                .map(|account| account.pubkey.clone())
        });
    (deduped_accounts, normalized_active_pubkey)
}

fn next_accounts_after_upsert(
    mut accounts: Vec<ManagedNip07Account>,
    pubkey: String,
) -> (Vec<ManagedNip07Account>, Option<String>) {
    if !accounts.iter().any(|account| account.pubkey == pubkey) {
        accounts.push(ManagedNip07Account {
            pubkey: pubkey.clone(),
            added_at: current_unix_timestamp_ms(),
        });
    }

    let (accounts, _) = normalize_accounts_state(accounts, Some(pubkey.clone()));
    (accounts, Some(pubkey))
}

fn load_nip07_accounts(
    storage_path: &Path,
) -> Result<(Vec<ManagedNip07Account>, Option<String>), String> {
    if !storage_path.exists() {
        return Ok((Vec::new(), None));
    }

    let raw = std::fs::read_to_string(storage_path)
        .map_err(|error| format!("Failed to read saved account: {error}"))?;
    let stored: StoredNip07Accounts = serde_json::from_str(&raw)
        .map_err(|error| format!("Failed to parse saved account metadata: {error}"))?;
    load_nip07_accounts_from_metadata(storage_path, stored)
}

fn load_nip07_accounts_from_metadata(
    storage_path: &Path,
    stored: StoredNip07Accounts,
) -> Result<(Vec<ManagedNip07Account>, Option<String>), String> {
    let original_account_count = stored.accounts.len();
    let original_active_pubkey = stored.active_pubkey.clone();
    let mut accounts = Vec::with_capacity(stored.accounts.len());

    for stored_account in stored.accounts {
        let pubkey = stored_account.pubkey.trim().to_string();
        if pubkey.is_empty() {
            continue;
        }
        accounts.push(ManagedNip07Account {
            pubkey,
            added_at: stored_account
                .added_at
                .unwrap_or_else(current_unix_timestamp_ms),
        });
    }

    let (accounts, active_pubkey) = normalize_accounts_state(accounts, stored.active_pubkey);
    if accounts.len() != original_account_count || active_pubkey != original_active_pubkey {
        persist_nip07_accounts(storage_path, &accounts, active_pubkey.as_deref())?;
    }

    Ok((accounts, active_pubkey))
}

fn persist_nip07_accounts(
    storage_path: &Path,
    accounts: &[ManagedNip07Account],
    active_pubkey: Option<&str>,
) -> Result<(), String> {
    if accounts.is_empty() {
        return clear_nip07_account(storage_path);
    }

    if let Some(parent) = storage_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("Failed to create account directory: {error}"))?;
    }

    let stored = StoredNip07Accounts {
        accounts: accounts
            .iter()
            .map(|account| StoredNip07Account {
                pubkey: account.pubkey.clone(),
                added_at: Some(account.added_at),
            })
            .collect(),
        active_pubkey: active_pubkey.map(str::to_owned),
    };
    let serialized = serde_json::to_vec_pretty(&stored)
        .map_err(|error| format!("Failed to serialize account: {error}"))?;

    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(storage_path)
        .map_err(|error| format!("Failed to open account storage: {error}"))?;
    file.write_all(&serialized)
        .and_then(|_| file.flush())
        .map_err(|error| format!("Failed to write account storage: {error}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let permissions = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(storage_path, permissions)
            .map_err(|error| format!("Failed to lock down account storage permissions: {error}"))?;
    }

    Ok(())
}

fn clear_nip07_account(storage_path: &Path) -> Result<(), String> {
    if !storage_path.exists() {
        return Ok(());
    }

    std::fs::remove_file(storage_path)
        .map_err(|error| format!("Failed to remove saved account: {error}"))
}

fn unsigned_event_from_nip07_params(
    params: &serde_json::Value,
    public_key: PublicKey,
) -> Result<UnsignedEvent, String> {
    let event = params
        .get("event")
        .ok_or_else(|| "Missing event parameter".to_string())?;
    let created_at = event
        .get("created_at")
        .cloned()
        .ok_or_else(|| "Event is missing created_at".to_string())?;
    let kind = event
        .get("kind")
        .cloned()
        .ok_or_else(|| "Event is missing kind".to_string())?;
    let tags = event
        .get("tags")
        .cloned()
        .ok_or_else(|| "Event is missing tags".to_string())?;
    let content = event
        .get("content")
        .cloned()
        .ok_or_else(|| "Event is missing content".to_string())?;

    serde_json::from_value(serde_json::json!({
        "pubkey": public_key.to_hex(),
        "created_at": created_at,
        "kind": kind,
        "tags": tags,
        "content": content,
    }))
    .map_err(|error| format!("Invalid NIP-07 event payload: {error}"))
}

fn nip07_string_param<'a>(params: &'a serde_json::Value, key: &str) -> Result<&'a str, String> {
    params
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("Missing {key} parameter"))
}

fn nip07_pubkey_param(params: &serde_json::Value) -> Result<PublicKey, String> {
    let pubkey = nip07_string_param(params, "pubkey")?;
    PublicKey::parse(pubkey).map_err(|error| format!("Invalid pubkey parameter: {error}"))
}

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
fn native_permission_dialog_message(origin: &str, method: &str) -> String {
    format!(
        "{} wants to {}.\n\n{}",
        origin,
        permission_method_description(method),
        "Iris is asking on behalf of the site you opened."
    )
}

fn permission_method_description(method: &str) -> &'static str {
    match method {
        "getPublicKey" => "read your public key",
        "signEvent" => "sign Nostr events",
        "nip04.encrypt" | "nip44.encrypt" => "encrypt messages",
        "nip04.decrypt" | "nip44.decrypt" => "decrypt messages",
        _ => "use your Nostr account",
    }
}

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
fn native_permission_decision_from_results(
    primary: &MessageDialogResult,
    secondary: Option<&MessageDialogResult>,
) -> Nip07PermissionDecisionAction {
    let is_allow = matches!(
        primary,
        MessageDialogResult::Custom(label) if label == NATIVE_PERMISSION_ALLOW_LABEL
    );

    if is_allow {
        match secondary {
            Some(MessageDialogResult::Custom(label))
                if label == NATIVE_PERMISSION_ALWAYS_ALLOW_LABEL =>
            {
                Nip07PermissionDecisionAction::AllowAlways
            }
            _ => Nip07PermissionDecisionAction::AllowSession,
        }
    } else {
        match secondary {
            Some(MessageDialogResult::Custom(label))
                if label == NATIVE_PERMISSION_BLOCK_SITE_LABEL =>
            {
                Nip07PermissionDecisionAction::BlockSite
            }
            _ => Nip07PermissionDecisionAction::Deny,
        }
    }
}

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
async fn show_native_message_dialog<R: Runtime>(
    app: &AppHandle<R>,
    title: &str,
    message: &str,
    buttons: MessageDialogButtons,
) -> Result<MessageDialogResult, String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "Main window not found".to_string())?;
    let (tx, rx) = oneshot::channel();
    app.dialog()
        .message(message.to_string())
        .parent(&window)
        .title(title.to_string())
        .kind(MessageDialogKind::Warning)
        .buttons(buttons)
        .show_with_result(move |result| {
            let _ = tx.send(result);
        });
    rx.await
        .map_err(|_| "Native permission dialog was dismissed unexpectedly".to_string())
}

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
async fn prompt_native_permission_decision<R: Runtime>(
    app: &AppHandle<R>,
    origin: &str,
    method: &str,
) -> Result<Nip07PermissionDecisionAction, String> {
    let title = "NIP-07 Permission";
    let primary = show_native_message_dialog(
        app,
        title,
        &native_permission_dialog_message(origin, method),
        MessageDialogButtons::OkCancelCustom(
            NATIVE_PERMISSION_ALLOW_LABEL.to_string(),
            NATIVE_PERMISSION_DENY_LABEL.to_string(),
        ),
    )
    .await?;

    let secondary = match primary {
        MessageDialogResult::Custom(ref label) if label == NATIVE_PERMISSION_ALLOW_LABEL => Some(
            show_native_message_dialog(
                app,
                title,
                &format!("Remember this permission for {}?", origin),
                MessageDialogButtons::OkCancelCustom(
                    NATIVE_PERMISSION_ALWAYS_ALLOW_LABEL.to_string(),
                    NATIVE_PERMISSION_ALLOW_SESSION_LABEL.to_string(),
                ),
            )
            .await?,
        ),
        _ => Some(
            show_native_message_dialog(
                app,
                title,
                &format!("Block future permission prompts from {}?", origin),
                MessageDialogButtons::OkCancelCustom(
                    NATIVE_PERMISSION_BLOCK_SITE_LABEL.to_string(),
                    NATIVE_PERMISSION_DENY_ONCE_LABEL.to_string(),
                ),
            )
            .await?,
        ),
    };

    Ok(native_permission_decision_from_results(
        &primary,
        secondary.as_ref(),
    ))
}

fn handle_nip07_request_sync(
    nip07_state: Option<Arc<Nip07State>>,
    request: Nip07Request,
) -> Nip07Response {
    let thread = match std::thread::Builder::new()
        .name("iris-nip07-request".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("dedicated NIP-07 runtime");
            runtime.block_on(async move {
                handle_nip07_request_inner(
                    nip07_state.as_deref(),
                    &request.method,
                    &request.params,
                    &request.origin,
                )
                .await
            })
        }) {
        Ok(thread) => thread,
        Err(error) => {
            error!("[htree://nip07] Failed to spawn request thread: {}", error);
            return Nip07Response {
                result: None,
                error: Some("Internal error handling NIP-07 request".to_string()),
            };
        }
    };

    match thread.join() {
        Ok(response) => response,
        Err(_) => {
            error!("[htree://nip07] Request thread panicked");
            Nip07Response {
                result: None,
                error: Some("Internal error handling NIP-07 request".to_string()),
            }
        }
    }
}

fn build_webview_nip07_probe_script(scenario: &str) -> String {
    let scenario = serde_json::to_string(scenario).unwrap_or_else(|_| "\"probe\"".to_string());
    format!(
        r#"
(function() {{
  const scenario = {scenario};
  const markerId = '__iris_nip07_probe__';
  const markerHeaderPass = 'IRIS NIP07 PASS';
  const markerHeaderFail = 'IRIS NIP07 FAIL';

  function ensureMarker() {{
    const existing = document.getElementById(markerId);
    if (existing) return existing;
    const marker = document.createElement('pre');
    marker.id = markerId;
    marker.setAttribute('data-iris-nip07-probe', scenario);
    marker.style.position = 'fixed';
    marker.style.top = '8px';
    marker.style.left = '8px';
    marker.style.right = '8px';
    marker.style.zIndex = '2147483647';
    marker.style.margin = '0';
    marker.style.padding = '10px 12px';
    marker.style.maxHeight = '40vh';
    marker.style.overflow = 'auto';
    marker.style.background = 'rgba(10, 11, 14, 0.92)';
    marker.style.color = '#d8f3ff';
    marker.style.border = '1px solid rgba(140, 220, 255, 0.35)';
    marker.style.borderRadius = '10px';
    marker.style.font = '12px/1.45 ui-monospace, SFMono-Regular, Menlo, monospace';
    marker.style.whiteSpace = 'pre-wrap';
    marker.style.pointerEvents = 'none';
    (document.body || document.documentElement).prepend(marker);
    return marker;
  }}

  function formatError(error) {{
    if (error instanceof Error) {{
      return error.stack || `${{error.name}}: ${{error.message}}`;
    }}
    if (typeof error === 'string') {{
      return error;
    }}
    try {{
      return JSON.stringify(error);
    }} catch (_error) {{
      return String(error);
    }}
  }}

  async function run() {{
    const marker = ensureMarker();
    const lines = [`SCENARIO:${{scenario}}`, `URL:${{location.href}}`];
    marker.textContent = [`IRIS NIP07 RUNNING`, ...lines].join('\n');

    try {{
      if (!window.nostr) {{
        throw new Error('window.nostr is not available');
      }}

      const pubkey = await window.nostr.getPublicKey();
      lines.push(`PUBKEY:${{pubkey}}`);

      const event = await window.nostr.signEvent({{
        kind: 22242,
        created_at: 1711111111,
        tags: [['probe', scenario]],
        content: `iris nip07 probe ${{scenario}}`,
      }});

      if (!event || event.pubkey !== pubkey || typeof event.id !== 'string' || typeof event.sig !== 'string') {{
        throw new Error('signEvent returned an invalid event');
      }}

      lines.push('GET_PUBLIC_KEY:ok');
      lines.push('SIGN_EVENT:ok');
      marker.textContent = [markerHeaderPass, ...lines].join('\n');
      document.title = `${{markerHeaderPass}} ${{scenario}} ${{pubkey.slice(0, 8)}}`;
    }} catch (error) {{
      const message = formatError(error);
      lines.push(`ERROR:${{message}}`);
      marker.textContent = [markerHeaderFail, ...lines].join('\n');
      document.title = `${{markerHeaderFail}} ${{scenario}}`;
      console.error('[Iris][NIP07Probe]', error);
    }}
  }}

  void run();
}})();
"#,
    )
}

#[derive(Debug, Deserialize)]
pub struct Nip07Request {
    pub method: String,
    pub params: serde_json::Value,
    pub origin: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct Nip07Response {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Nip07AccountSummary {
    pub pubkey: String,
    pub npub: String,
    pub added_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Nip07AccountsSummary {
    pub accounts: Vec<Nip07AccountSummary>,
    pub active_pubkey: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Nip07PermissionPrompt {
    pub request_id: String,
    pub origin: String,
    pub method: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Nip07PermissionDecisionAction {
    Deny,
    AllowSession,
    AllowAlways,
    BlockSite,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredNip07Account {
    pubkey: String,
    #[serde(default)]
    added_at: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredNip07Accounts {
    #[serde(default)]
    accounts: Vec<StoredNip07Account>,
    active_pubkey: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConfidentialBlob {
    #[serde(default = "confidential_blob_version")]
    version: u32,
    #[serde(default)]
    nip07: Nip07ConfidentialNamespace,
    #[serde(flatten, default)]
    extra: BTreeMap<String, serde_json::Value>,
}

impl Default for ConfidentialBlob {
    fn default() -> Self {
        Self {
            version: confidential_blob_version(),
            nip07: Nip07ConfidentialNamespace::default(),
            extra: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub(crate) struct Nip07ConfidentialNamespace {
    #[serde(default)]
    accounts: BTreeMap<String, String>,
}

#[derive(Debug)]
struct Nip07PermissionDecision {
    decision: Nip07PermissionDecisionAction,
}

#[derive(Debug, Clone)]
struct ManagedNip07Account {
    pubkey: String,
    added_at: u64,
}

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
const NATIVE_PERMISSION_ALLOW_LABEL: &str = "Allow";
#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
const NATIVE_PERMISSION_DENY_LABEL: &str = "Deny";
#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
const NATIVE_PERMISSION_ALWAYS_ALLOW_LABEL: &str = "Always Allow";
#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
const NATIVE_PERMISSION_ALLOW_SESSION_LABEL: &str = "Only This Session";
#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
const NATIVE_PERMISSION_BLOCK_SITE_LABEL: &str = "Block Site";
#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
const NATIVE_PERMISSION_DENY_ONCE_LABEL: &str = "Deny Once";

// ============================================
// Script generation
// ============================================

/// Generate NIP-07 script for main window (uses Tauri invoke -> proxied to main webview's window.nostr)
pub fn generate_main_window_nip07_script() -> String {
    r#"
(function() {
  if (window.nostr) {
    console.log('[NIP-07] Already initialized');
    return;
  }

  console.log('[NIP-07] Initializing for main window via Tauri invoke');

  async function getInvoke() {
    if (window.__TAURI_INTERNALS__?.invoke) return window.__TAURI_INTERNALS__.invoke;
    if (window.__TAURI__?.core?.invoke) return window.__TAURI__.core.invoke;
    if (window.__TAURI__?.invoke) return window.__TAURI__.invoke;

    for (let i = 0; i < 50; i++) {
      await new Promise(r => setTimeout(r, 100));
      if (window.__TAURI_INTERNALS__?.invoke) return window.__TAURI_INTERNALS__.invoke;
      if (window.__TAURI__?.core?.invoke) return window.__TAURI__.core.invoke;
      if (window.__TAURI__?.invoke) return window.__TAURI__.invoke;
    }
    throw new Error('Tauri invoke not available after timeout');
  }

  async function callNip07(method, params) {
    console.log('[NIP-07] Calling:', method, params);
    try {
      const invoke = await getInvoke();
      const result = await invoke('nip07_request', {
        method,
        params: params || {},
        origin: 'tauri://localhost'
      });
      console.log('[NIP-07] Result:', result);
      if (result.error) {
        throw new Error(result.error);
      }
      return result.result;
    } catch (e) {
      console.error('[NIP-07] Error:', e);
      throw e;
    }
  }

  window.nostr = {
    async getPublicKey() {
      return callNip07('getPublicKey', {});
    },
    async signEvent(event) {
      return callNip07('signEvent', { event });
    },
    async getRelays() {
      return callNip07('getRelays', {});
    },
    nip04: {
      async encrypt(pubkey, plaintext) {
        return callNip07('nip04.encrypt', { pubkey, plaintext });
      },
      async decrypt(pubkey, ciphertext) {
        return callNip07('nip04.decrypt', { pubkey, ciphertext });
      }
    },
    nip44: {
      async encrypt(pubkey, plaintext) {
        return callNip07('nip44.encrypt', { pubkey, plaintext });
      },
      async decrypt(pubkey, ciphertext) {
        return callNip07('nip44.decrypt', { pubkey, ciphertext });
      }
    }
  };

  console.log('[NIP-07] window.nostr initialized for main window');
})();
"#
    .to_string()
}

/// Generate NIP-07 init script for child webviews (uses htree://nip07/ protocol)
pub fn generate_nip07_script(
    server_url: &str,
    session_token: &str,
    label: &str,
    canonical_origin: Option<&str>,
    canonical_url_root: Option<&str>,
    actual_url_root: Option<&str>,
) -> String {
    let canonical_origin_json = canonical_origin
        .map(|value| serde_json::to_string(value).unwrap_or_else(|_| "null".to_string()))
        .unwrap_or_else(|| "null".to_string());
    let canonical_url_root_json = canonical_url_root
        .map(|value| serde_json::to_string(value).unwrap_or_else(|_| "null".to_string()))
        .unwrap_or_else(|| "null".to_string());
    let actual_url_root_json = actual_url_root
        .map(|value| serde_json::to_string(value).unwrap_or_else(|_| "null".to_string()))
        .unwrap_or_else(|| "null".to_string());

    format!(
        r#"
(function() {{
  if (window.__IRIS_CHILD_BRIDGE_INITIALIZED__) {{
    return;
  }}
  window.__IRIS_CHILD_BRIDGE_INITIALIZED__ = true;
  const hasNostr = !!window.nostr;
  const SERVER_URL = "{}";
  const SESSION_TOKEN = "{}";
  const WEBVIEW_LABEL = "{}";
  const CANONICAL_ORIGIN = {};
  const CANONICAL_URL_ROOT = {};
  const ACTUAL_URL_ROOT = {};
  const WEBVIEW_ENDPOINT = `${{SERVER_URL}}/__iris_webview`;
  const NIP07_PROTOCOL_ENDPOINT = 'htree://nip07/';
  const NIP07_HTTP_ENDPOINT = `${{SERVER_URL}}/__iris_nip07`;
  const IS_TOP_LEVEL_DOCUMENT = (() => {{
    try {{
      return window.top === window.self;
    }} catch (_error) {{
      return true;
    }}
  }})();
  console.log('[NIP-07] Initializing with server:', SERVER_URL);
  window.__HTREE_SERVER_URL__ = SERVER_URL;
  window.__HTREE_CANONICAL_URL__ = null;
  window.__HTREE_SESSION_TOKEN__ = SESSION_TOKEN;

  let invokePromise = null;
  let resolvedInvoke = null;
  let flushPromise = null;
  let flushTimer = null;
  const pendingWebviewEvents = [];
  async function getInvoke() {{
    if (resolvedInvoke) return resolvedInvoke;
    const getNow = () =>
      window.__TAURI_INTERNALS__?.invoke ||
      window.__TAURI__?.core?.invoke ||
      window.__TAURI__?.invoke ||
      null;
    const immediate = getNow();
    if (immediate) {{
      resolvedInvoke = immediate;
      return resolvedInvoke;
    }}
    if (!invokePromise) {{
      invokePromise = (async () => {{
        for (let i = 0; i < 20; i++) {{
          await new Promise((resolve) => setTimeout(resolve, 50));
          const candidate = getNow();
          if (candidate) {{
            resolvedInvoke = candidate;
            return candidate;
          }}
        }}
        return null;
      }})().finally(() => {{
        if (!resolvedInvoke) {{
          invokePromise = null;
        }}
      }});
    }}
    return invokePromise;
  }}

  function scheduleWebviewEventFlush() {{
    if (flushTimer) return;
    flushTimer = setTimeout(() => {{
      flushTimer = null;
      flushPendingWebviewEvents().catch((error) => {{
        console.warn('[WebviewBridge] Delayed flush failed', error);
      }});
    }}, 250);
  }}

  async function flushPendingWebviewEvents() {{
    if (flushPromise) return flushPromise;
    flushPromise = (async () => {{
      const invoke = await getInvoke();
      while (pendingWebviewEvents.length > 0) {{
        const payload = pendingWebviewEvents[0];
        try {{
          if (invoke) {{
            await invoke('webview_event', {{
              payload,
              sessionToken: SESSION_TOKEN
            }});
          }} else {{
            const response = await fetch(WEBVIEW_ENDPOINT, {{
              method: 'POST',
              headers: {{
                'Content-Type': 'text/plain;charset=UTF-8'
              }},
              body: JSON.stringify({{
                sessionToken: SESSION_TOKEN,
                payload
              }})
            }});
            if (!response.ok) {{
              throw new Error(`Protocol bridge request failed: ${{response.status}}`);
            }}
          }}
          pendingWebviewEvents.shift();
        }} catch (error) {{
          if (invoke) {{
            resolvedInvoke = null;
            invokePromise = null;
          }}
          console.warn('[WebviewBridge] Failed to flush event', error);
          scheduleWebviewEventFlush();
          return false;
        }}
      }}
      return true;
    }})();
    try {{
      return await flushPromise;
    }} finally {{
      flushPromise = null;
    }}
  }}

  function stripInternalHtreeQueryParams(url) {{
    try {{
      const parsed = new URL(url);
      parsed.searchParams.delete('iris_htree_server');
      parsed.searchParams.delete('iris_htree_canonical');
      parsed.searchParams.delete('iris_htree_session');
      return parsed.toString();
    }} catch (_error) {{
      return url;
    }}
  }}

  function canonicalizeUrl(url) {{
    let mappedUrl = url;
    if (
      typeof url === 'string' &&
      typeof CANONICAL_URL_ROOT === 'string' &&
      typeof ACTUAL_URL_ROOT === 'string' &&
      url.startsWith(ACTUAL_URL_ROOT)
    ) {{
      mappedUrl = `${{CANONICAL_URL_ROOT}}${{url.slice(ACTUAL_URL_ROOT.length)}}`;
    }} else if (
      typeof url === 'string' &&
      typeof CANONICAL_URL_ROOT === 'string' &&
      typeof ACTUAL_URL_ROOT === 'string'
    ) {{
      try {{
        const actualRoot = new URL(ACTUAL_URL_ROOT);
        const candidate = new URL(url);
        const actualRootPath = actualRoot.pathname.replace(/\/$/, '');
        const alreadyUnderRoot = candidate.pathname === actualRootPath ||
          candidate.pathname.startsWith(`${{actualRootPath}}/`);
        if (
          candidate.origin === actualRoot.origin &&
          !alreadyUnderRoot &&
          actualRootPath
        ) {{
          mappedUrl = `${{CANONICAL_URL_ROOT}}${{candidate.pathname}}${{candidate.search}}${{candidate.hash}}`;
        }}
      }} catch (_error) {{}}
    }}
    return stripInternalHtreeQueryParams(mappedUrl);
  }}

  function updateCanonicalLocation() {{
    const canonicalUrl = canonicalizeUrl(window.location.href);
    if (typeof canonicalUrl === 'string') {{
      window.__HTREE_CANONICAL_URL__ = canonicalUrl;
    }}
    return canonicalUrl;
  }}

  function getOrigin() {{
    if (typeof CANONICAL_ORIGIN === 'string' && CANONICAL_ORIGIN) {{
      return CANONICAL_ORIGIN;
    }}
    const origin = window.location.origin;
    if (origin && origin !== 'null') return origin;
    const protocol = window.location.protocol || '';
    const normalizedProtocol = protocol.endsWith(':') ? protocol.slice(0, -1) : protocol;
    const host = window.location.host || '';
    if (host) return `${{normalizedProtocol}}://${{host}}`;
    return normalizedProtocol || 'null';
  }}

  async function postWebviewEvent(payload) {{
    pendingWebviewEvents.push(payload);
    try {{
      const sent = await flushPendingWebviewEvents();
      if (!sent) {{
        scheduleWebviewEventFlush();
      }}
    }} catch (error) {{
      console.warn('[WebviewBridge] Failed to queue event', error);
      scheduleWebviewEventFlush();
    }}
  }}

  let lastLocation = null;
  function notifyLocation(source) {{
    if (!IS_TOP_LEVEL_DOCUMENT) return;
    const url = updateCanonicalLocation();
    if (url === lastLocation) return;
    lastLocation = url;
    postWebviewEvent({{
      kind: 'location',
      label: WEBVIEW_LABEL,
      origin: getOrigin(),
      url,
      source
    }});
  }}

  function getBodyTextPreview() {{
    try {{
      const text = document.body?.innerText || '';
      return text.replace(/\s+/g, ' ').trim().slice(0, 240);
    }} catch (_error) {{
      return '';
    }}
  }}

  function formatDebugValue(value) {{
    if (value instanceof Error) {{
      return value.stack || `${{value.name}}: ${{value.message}}`;
    }}
    if (typeof value === 'string') {{
      return value;
    }}
    try {{
      return JSON.stringify(value);
    }} catch (_error) {{
      return String(value);
    }}
  }}

  function getDebugSummary() {{
    try {{
      const entries = Array.isArray(window.__HTREE_DEBUG_LOG__) ? window.__HTREE_DEBUG_LOG__ : [];
      const relevant = entries.filter((entry) => {{
        const event = entry?.event;
        return event === 'window:error' ||
          event === 'window:unhandledrejection' ||
          event === 'console:error' ||
          event === 'console:warn' ||
          event === 'worker:ready' ||
          (typeof event === 'string' && (
            event.startsWith('worker:init:') ||
            event.startsWith('media:setup:') ||
            event.startsWith('prefix:')
          ));
      }});
      if (relevant.length === 0) return '';
      const tail = relevant.slice(-5).map((entry) => {{
        const event = typeof entry?.event === 'string' ? entry.event : 'debug';
        const data = entry?.data;
        if (data?.message) return `${{event}} ${{data.message}}`;
        if (Array.isArray(data?.args) && data.args.length > 0) {{
          return `${{event}} ${{data.args.join(' ')}}`;
        }}
        if (data?.reason) return `${{event}} ${{data.reason}}`;
        if (data) return `${{event}} ${{formatDebugValue(data)}}`;
        return event;
      }});
      return tail.join(' | ').slice(0, 480);
    }} catch (_error) {{
      return '';
    }}
  }}

  function getMediaSummary() {{
    try {{
      function isLoadedImage(img) {{
        return img.dataset.htreeLoaded === '1' ||
          (img.complete && img.naturalWidth > 0 && img.naturalHeight > 0);
      }}
      function isVisibleElement(element) {{
        const style = window.getComputedStyle(element);
        const rect = element.getBoundingClientRect();
        return style.display !== 'none' &&
          style.visibility !== 'hidden' &&
          rect.width > 20 &&
          rect.height > 20;
      }}
      function isThumbnailCandidate(img) {{
        const src = (img.currentSrc || img.src || '').toLowerCase();
        const anchorHref = (img.closest('a')?.getAttribute('href') || '').toLowerCase();
        const rect = img.getBoundingClientRect();
        if (rect.width < 80 || rect.height < 45) {{
          return false;
        }}
        return src.includes('/thumbnail') ||
          anchorHref.includes('videos%2f') ||
          anchorHref.includes('/videos/') ||
          anchorHref.includes('/video/');
      }}
      const images = Array.from(document.images || []);
      const thumbImages = images.filter(isThumbnailCandidate);
      const loadedThumbImages = thumbImages.filter(isLoadedImage);
      const visibleLoadedThumbImages = loadedThumbImages.filter(isVisibleElement);
      const videos = Array.from(document.querySelectorAll('video'));
      const readyVideos = videos.filter((video) =>
        video.dataset.htreeReady === '1' ||
        video.readyState >= HTMLMediaElement.HAVE_CURRENT_DATA
      );
      const appChildren = document.getElementById('app')?.childElementCount ?? 0;
      const smokeEnabled = new URLSearchParams(window.location.search).get('smoke') === '1' ? 1 : 0;
      const workerReady = window.__workerAdapter || window.__getWorkerAdapter?.() ? 1 : 0;
      const htreeBase = window.htree?.htreeBaseUrl ? 1 : 0;
      const transportRelays = window.__nostrStore?.getState?.()?.transportRelays;
      const relayCount = Array.isArray(transportRelays) && transportRelays.length > 0
        ? transportRelays.length
        : (Array.isArray(window.htree?.relays) ? window.htree.relays.length : 0);
      const hasServiceWorkerController = navigator.serviceWorker?.controller ? 1 : 0;
      const isCrossOriginIsolated = self.crossOriginIsolated ? 1 : 0;
      return `thumbs=${{loadedThumbImages.length}}/${{thumbImages.length}} visible=${{visibleLoadedThumbImages.length}} videos=${{readyVideos.length}}/${{videos.length}} app=${{appChildren}} smoke=${{smokeEnabled}} worker=${{workerReady}} base=${{htreeBase}} relays=${{relayCount}} sw=${{hasServiceWorkerController}} coi=${{isCrossOriginIsolated}}`;
    }} catch (_error) {{
      return '';
    }}
  }}

  function getPwaMetadata() {{
    try {{
      const manifestLink = Array.from(document.querySelectorAll('link[rel][href]'))
        .find((link) => (link.rel || '')
          .split(/\s+/)
          .some((part) => part.toLowerCase() === 'manifest'));
      const manifestUrl = manifestLink?.href || '';
      if (!manifestUrl) {{
        return {{
          manifestUrl: '',
          manifestName: '',
          manifestIconUrl: '',
        }};
      }}
      const applicationName = document
        .querySelector('meta[name="application-name"]')
        ?.getAttribute('content')
        ?.trim() || '';
      const touchIcon = Array.from(document.querySelectorAll('link[rel][href]'))
        .find((link) => (link.rel || '')
          .split(/\s+/)
          .some((part) => part.toLowerCase() === 'apple-touch-icon'))
        ?.href || '';
      return {{
        manifestUrl,
        manifestName: applicationName,
        manifestIconUrl: touchIcon,
      }};
    }} catch (_error) {{
      return {{
        manifestUrl: '',
        manifestName: '',
        manifestIconUrl: '',
      }};
    }}
  }}

  function notifyDiagnostic(phase, errorMessage) {{
    if (!IS_TOP_LEVEL_DOCUMENT) return;
    const debugSummary = getDebugSummary();
    const pwa = getPwaMetadata();
    postWebviewEvent({{
      kind: 'diagnostic',
      label: WEBVIEW_LABEL,
      origin: getOrigin(),
      url: updateCanonicalLocation(),
      source: phase,
      title: document.title || '',
      readyState: document.readyState || '',
      bodyText: getBodyTextPreview(),
      mediaSummary: getMediaSummary(),
      manifestUrl: pwa.manifestUrl || null,
      manifestName: pwa.manifestName || null,
      manifestIconUrl: pwa.manifestIconUrl || null,
      error: errorMessage || debugSummary || null
    }});
  }}

  let diagnosticTimer = null;
  function queueDiagnostic(phase, errorMessage) {{
    if (diagnosticTimer) clearTimeout(diagnosticTimer);
    diagnosticTimer = setTimeout(() => {{
      diagnosticTimer = null;
      notifyDiagnostic(phase, errorMessage);
    }}, 75);
  }}

  const originalPushState = history.pushState;
  history.pushState = function(state, title, url) {{
    const result = originalPushState.apply(this, arguments);
    notifyLocation('pushState');
    return result;
  }};

  const originalReplaceState = history.replaceState;
  history.replaceState = function(state, title, url) {{
    const result = originalReplaceState.apply(this, arguments);
    notifyLocation('replaceState');
    return result;
  }};

  window.addEventListener('popstate', () => notifyLocation('popstate'));
  window.addEventListener('hashchange', () => notifyLocation('hashchange'));
  window.addEventListener('DOMContentLoaded', () => {{
    notifyLocation('domcontentloaded');
    notifyDiagnostic('domcontentloaded');
    if (document.body) {{
      const observer = new MutationObserver(() => queueDiagnostic('mutation'));
      observer.observe(document.body, {{
        childList: true,
        subtree: true,
        characterData: true
      }});
    }}
  }});
  window.addEventListener('load', () => {{
    notifyLocation('load');
    notifyDiagnostic('load');
    setTimeout(() => notifyDiagnostic('post-load'), 250);
    setTimeout(() => notifyDiagnostic('post-load-late'), 1500);
    setTimeout(() => notifyDiagnostic('post-load-media'), 5000);
    setTimeout(() => notifyDiagnostic('post-load-media-late'), 10000);
  }});
  document.addEventListener('load', (event) => {{
    if (event.target instanceof HTMLImageElement) {{
      event.target.dataset.htreeLoaded = '1';
      queueDiagnostic('resource-load');
    }} else if (event.target instanceof HTMLVideoElement) {{
      event.target.dataset.htreeReady = '1';
      queueDiagnostic('resource-load');
    }}
  }}, true);
  document.addEventListener('error', (event) => {{
    if (
      event.target instanceof HTMLImageElement ||
      event.target instanceof HTMLVideoElement ||
      event.target instanceof HTMLScriptElement ||
      event.target instanceof HTMLLinkElement
    ) {{
      const targetUrl = event.target instanceof HTMLImageElement || event.target instanceof HTMLVideoElement
        ? (event.target.currentSrc || event.target.src || '')
        : event.target instanceof HTMLScriptElement
          ? (event.target.src || '')
          : (event.target.href || '');
      const suffix = targetUrl ? `: ${{targetUrl}}` : '';
      queueDiagnostic('resource-error', `${{event.target.tagName.toLowerCase()}} failed to load${{suffix}}`);
    }}
  }}, true);
  document.addEventListener('loadeddata', (event) => {{
    if (event.target instanceof HTMLVideoElement) {{
      event.target.dataset.htreeReady = '1';
      queueDiagnostic('video-loadeddata');
    }}
  }}, true);
  document.addEventListener('loadedmetadata', (event) => {{
    if (event.target instanceof HTMLVideoElement) {{
      event.target.dataset.htreeReady = '1';
      queueDiagnostic('video-loadedmetadata');
    }}
  }}, true);
  document.addEventListener('canplay', (event) => {{
    if (event.target instanceof HTMLVideoElement) {{
      event.target.dataset.htreeReady = '1';
      queueDiagnostic('video-canplay');
    }}
  }}, true);
  window.addEventListener('error', (event) => {{
    const filename = event.filename || '';
    const line = event.lineno || 0;
    const column = event.colno || 0;
    const location = filename ? ` @ ${{filename}}:${{line}}:${{column}}` : '';
    notifyDiagnostic('error', `${{event.message || 'Script error'}}${{location}}`);
  }});
  window.addEventListener('unhandledrejection', (event) => {{
    const reason = event.reason;
    const message = reason instanceof Error
      ? (reason.stack || reason.message)
      : typeof reason === 'string'
        ? reason
        : JSON.stringify(reason);
    notifyDiagnostic('unhandledrejection', message);
  }});
  const originalConsoleError = console.error?.bind(console);
  if (originalConsoleError) {{
    console.error = (...args) => {{
      queueDiagnostic('console-error', args.map(formatDebugValue).join(' ').slice(0, 240));
      originalConsoleError(...args);
    }};
  }}
  queueMicrotask(() => {{
    updateCanonicalLocation();
    notifyLocation('init');
    notifyDiagnostic('init');
  }});

  async function callNip07(method, params) {{
    console.log('[NIP-07] Calling:', method, params);
    const origin = getOrigin();
    const body = JSON.stringify({{ method, params, origin }});
    let transportError = null;
    const invoke = await getInvoke();
    if (invoke) {{
      try {{
        const result = await invoke('nip07_request', {{
          method,
          params: params || {{}},
          origin,
        }});
        if (result?.error) {{
          throw new Error(result.error);
        }}
        return result?.result;
      }} catch (error) {{
        const message = error instanceof Error ? error.message : String(error ?? '');
        const shouldFallback =
          /invoke|ipc|channel|command|not found|not available|unsupported/i.test(message);
        if (!shouldFallback) {{
          console.error('[NIP-07] Invoke request failed:', error);
          throw error;
        }}
        transportError = error;
        console.warn('[NIP-07] Invoke transport unavailable, falling back to fetch bridges', error);
      }}
    }}
    for (const endpoint of [NIP07_PROTOCOL_ENDPOINT, NIP07_HTTP_ENDPOINT]) {{
      let response;
      try {{
        response = await fetch(endpoint, {{
          method: 'POST',
          headers: {{ 'Content-Type': 'text/plain;charset=UTF-8' }},
          body,
        }});
      }} catch (error) {{
        transportError = error;
        console.warn(`[NIP-07] Transport failed via ${{endpoint}}`, error);
        continue;
      }}
      if (!response.ok) {{
        throw new Error(`NIP-07 request failed: ${{response.status}}`);
      }}
      const result = await response.json();
      if (result.error) throw new Error(result.error);
      return result.result;
    }}
    console.error('[NIP-07] Error:', transportError);
    throw transportError;
  }}

  if (!hasNostr) {{
    window.nostr = {{
      async getPublicKey() {{ return callNip07('getPublicKey', {{}}); }},
      async signEvent(event) {{ return callNip07('signEvent', {{ event }}); }},
      async getRelays() {{ return callNip07('getRelays', {{}}); }},
      nip04: {{
        async encrypt(pubkey, plaintext) {{ return callNip07('nip04.encrypt', {{ pubkey, plaintext }}); }},
        async decrypt(pubkey, ciphertext) {{ return callNip07('nip04.decrypt', {{ pubkey, ciphertext }}); }}
      }},
      nip44: {{
        async encrypt(pubkey, plaintext) {{ return callNip07('nip44.encrypt', {{ pubkey, plaintext }}); }},
        async decrypt(pubkey, ciphertext) {{ return callNip07('nip44.decrypt', {{ pubkey, ciphertext }}); }}
      }}
    }};
    console.log('[NIP-07] window.nostr initialized');
  }}
}})();
"#,
        server_url,
        session_token,
        label,
        canonical_origin_json,
        canonical_url_root_json,
        actual_url_root_json
    )
}

fn body_text_preview_js() -> &'static str {
    r#"
function getBodyTextPreview() {
  try {
    const text = document.body?.innerText || '';
    return text.replace(/\s+/g, ' ').trim().slice(0, 240);
  } catch (_error) {
    return '';
  }
}
"#
}

fn media_summary_js() -> &'static str {
    r#"
function getMediaSummary() {
  try {
    function isLoadedImage(img) {
      return img.dataset.htreeLoaded === '1' ||
        (img.complete && img.naturalWidth > 0 && img.naturalHeight > 0);
    }
    function isVisibleElement(element) {
      const style = window.getComputedStyle(element);
      const rect = element.getBoundingClientRect();
      return style.display !== 'none' &&
        style.visibility !== 'hidden' &&
        rect.width > 20 &&
        rect.height > 20;
    }
    function isThumbnailCandidate(img) {
      const src = (img.currentSrc || img.src || '').toLowerCase();
      const anchorHref = (img.closest('a')?.getAttribute('href') || '').toLowerCase();
      const rect = img.getBoundingClientRect();
      if (rect.width < 80 || rect.height < 45) {
        return false;
      }
      return src.includes('/thumbnail') ||
        anchorHref.includes('videos%2f') ||
        anchorHref.includes('/videos/') ||
        anchorHref.includes('/video/');
    }
    const images = Array.from(document.images || []);
    const thumbImages = images.filter(isThumbnailCandidate);
    const loadedThumbImages = thumbImages.filter(isLoadedImage);
    const visibleLoadedThumbImages = loadedThumbImages.filter(isVisibleElement);
    const videos = Array.from(document.querySelectorAll('video'));
    const readyVideos = videos.filter((video) =>
      video.dataset.htreeReady === '1' ||
      video.readyState >= HTMLMediaElement.HAVE_CURRENT_DATA
    );
    const appChildren = document.getElementById('app')?.childElementCount ?? 0;
    const smokeEnabled = new URLSearchParams(window.location.search).get('smoke') === '1' ? 1 : 0;
    const workerReady = window.__workerAdapter || window.__getWorkerAdapter?.() ? 1 : 0;
    const htreeBase = window.htree?.htreeBaseUrl ? 1 : 0;
    const transportRelays = window.__nostrStore?.getState?.()?.transportRelays;
    const relayCount = Array.isArray(transportRelays) && transportRelays.length > 0
      ? transportRelays.length
      : (Array.isArray(window.htree?.relays) ? window.htree.relays.length : 0);
    const hasServiceWorkerController = navigator.serviceWorker?.controller ? 1 : 0;
    const isCrossOriginIsolated = self.crossOriginIsolated ? 1 : 0;
    return `thumbs=${loadedThumbImages.length}/${thumbImages.length} visible=${visibleLoadedThumbImages.length} videos=${readyVideos.length}/${videos.length} app=${appChildren} smoke=${smokeEnabled} worker=${workerReady} base=${htreeBase} relays=${relayCount} sw=${hasServiceWorkerController} coi=${isCrossOriginIsolated}`;
  } catch (_error) {
    return '';
  }
}
"#
}

fn pwa_metadata_js() -> &'static str {
    r#"
function getPwaMetadata() {
  try {
    const manifestLink = Array.from(document.querySelectorAll('link[rel][href]'))
      .find((link) => (link.rel || '')
        .split(/\s+/)
        .some((part) => part.toLowerCase() === 'manifest'));
    const manifestUrl = manifestLink?.href || '';
    if (!manifestUrl) {
      return {
        manifestUrl: '',
        manifestName: '',
        manifestIconUrl: '',
      };
    }
    const applicationName = document
      .querySelector('meta[name="application-name"]')
      ?.getAttribute('content')
      ?.trim() || '';
    const touchIcon = Array.from(document.querySelectorAll('link[rel][href]'))
      .find((link) => (link.rel || '')
        .split(/\s+/)
        .some((part) => part.toLowerCase() === 'apple-touch-icon'))
      ?.href || '';
    return {
      manifestUrl,
      manifestName: applicationName,
      manifestIconUrl: touchIcon,
    };
  } catch (_error) {
    return {
      manifestUrl: '',
      manifestName: '',
      manifestIconUrl: '',
    };
  }
}
"#
}

pub fn generate_webview_diagnostic_probe_script(
    server_url: &str,
    session_token: &str,
    label: &str,
    origin: &str,
    canonical_url_root: Option<&str>,
    actual_url_root: Option<&str>,
    source: &str,
) -> String {
    let webview_endpoint_json = serde_json::to_string(&format!("{server_url}/__iris_webview"))
        .unwrap_or_else(|_| "\"\"".to_string());
    let session_token_json =
        serde_json::to_string(session_token).unwrap_or_else(|_| "\"\"".to_string());
    let label_json = serde_json::to_string(label).unwrap_or_else(|_| "\"\"".to_string());
    let origin_json = serde_json::to_string(origin).unwrap_or_else(|_| "\"\"".to_string());
    let source_json = serde_json::to_string(source).unwrap_or_else(|_| "\"\"".to_string());
    let canonical_url_root_json = canonical_url_root
        .map(|value| serde_json::to_string(value).unwrap_or_else(|_| "null".to_string()))
        .unwrap_or_else(|| "null".to_string());
    let actual_url_root_json = actual_url_root
        .map(|value| serde_json::to_string(value).unwrap_or_else(|_| "null".to_string()))
        .unwrap_or_else(|| "null".to_string());

    format!(
        r#"
(() => {{
  const WEBVIEW_ENDPOINT = {webview_endpoint_json};
  const SESSION_TOKEN = {session_token_json};
  const LABEL = {label_json};
  const ORIGIN = {origin_json};
  const SOURCE = {source_json};
  const CANONICAL_URL_ROOT = {canonical_url_root_json};
  const ACTUAL_URL_ROOT = {actual_url_root_json};

  function stripInternalHtreeQueryParams(url) {{
    try {{
      const parsed = new URL(url);
      parsed.searchParams.delete('iris_htree_server');
      parsed.searchParams.delete('iris_htree_canonical');
      parsed.searchParams.delete('iris_htree_session');
      return parsed.toString();
    }} catch (_error) {{
      return url;
    }}
  }}

  function canonicalizeUrl(url) {{
    let mappedUrl = url;
    if (
      typeof url === 'string' &&
      typeof CANONICAL_URL_ROOT === 'string' &&
      typeof ACTUAL_URL_ROOT === 'string' &&
      url.startsWith(ACTUAL_URL_ROOT)
    ) {{
      mappedUrl = `${{CANONICAL_URL_ROOT}}${{url.slice(ACTUAL_URL_ROOT.length)}}`;
    }} else if (
      typeof url === 'string' &&
      typeof CANONICAL_URL_ROOT === 'string' &&
      typeof ACTUAL_URL_ROOT === 'string'
    ) {{
      try {{
        const actualRoot = new URL(ACTUAL_URL_ROOT);
        const candidate = new URL(url);
        const actualRootPath = actualRoot.pathname.replace(/\/$/, '');
        const alreadyUnderRoot = candidate.pathname === actualRootPath ||
          candidate.pathname.startsWith(`${{actualRootPath}}/`);
        if (
          candidate.origin === actualRoot.origin &&
          !alreadyUnderRoot &&
          actualRootPath
        ) {{
          mappedUrl = `${{CANONICAL_URL_ROOT}}${{candidate.pathname}}${{candidate.search}}${{candidate.hash}}`;
        }}
      }} catch (_error) {{}}
    }}
    return stripInternalHtreeQueryParams(mappedUrl);
  }}

  {body_text_preview}
  function getDebugSummary() {{
    try {{
      const entries = Array.isArray(window.__HTREE_DEBUG_LOG__) ? window.__HTREE_DEBUG_LOG__ : [];
      const relevant = entries.filter((entry) => {{
        const event = entry?.event;
        return event === 'window:error' ||
          event === 'window:unhandledrejection' ||
          event === 'console:error' ||
          event === 'console:warn' ||
          event === 'worker:ready' ||
          (typeof event === 'string' && (
            event.startsWith('worker:init:') ||
            event.startsWith('media:setup:') ||
            event.startsWith('prefix:')
          ));
      }});
      if (relevant.length === 0) return '';
      const tail = relevant.slice(-5).map((entry) => {{
        const event = typeof entry?.event === 'string' ? entry.event : 'debug';
        const data = entry?.data;
        if (data?.message) return `${{event}} ${{data.message}}`;
        if (Array.isArray(data?.args) && data.args.length > 0) {{
          return `${{event}} ${{data.args.join(' ')}}`;
        }}
        if (data?.reason) return `${{event}} ${{data.reason}}`;
        if (data) {{
          try {{
            return `${{event}} ${{JSON.stringify(data)}}`;
          }} catch (_error) {{
            return `${{event}} ${{String(data)}}`;
          }}
        }}
        return event;
      }});
      return tail.join(' | ').slice(0, 480);
    }} catch (_error) {{
      return '';
    }}
  }}
  {media_summary}
  {pwa_metadata}

  const pwa = getPwaMetadata();
    const payload = {{
      kind: 'diagnostic',
      label: LABEL,
      origin: ORIGIN,
      url: canonicalizeUrl(window.location.href),
    source: SOURCE,
      title: document.title || '',
      readyState: document.readyState || '',
      bodyText: getBodyTextPreview(),
      mediaSummary: getMediaSummary(),
      viewportWidth: Math.round(window.innerWidth || 0),
      viewportHeight: Math.round(window.innerHeight || 0),
      manifestUrl: pwa.manifestUrl || null,
      manifestName: pwa.manifestName || null,
      manifestIconUrl: pwa.manifestIconUrl || null,
      error: getDebugSummary() || null
  }};

  fetch(WEBVIEW_ENDPOINT, {{
    method: 'POST',
    headers: {{ 'Content-Type': 'text/plain;charset=UTF-8' }},
    body: JSON.stringify({{ sessionToken: SESSION_TOKEN, payload }})
  }}).catch((error) => {{
    console.warn('[WebviewProbe] Failed to send diagnostic', error);
  }});
}})();
"#,
        body_text_preview = body_text_preview_js(),
        media_summary = media_summary_js(),
        pwa_metadata = pwa_metadata_js()
    )
}

// ============================================
// NIP-07 request handler (proxies to main webview)
// ============================================

fn is_trusted_nip07_origin(origin: &str) -> bool {
    origin == "tauri://localhost"
}

async fn ensure_nip07_permission(
    state: Option<&Nip07State>,
    origin: &str,
    permission_type: PermissionType,
    method: &str,
) -> Result<(), Nip07Response> {
    if is_trusted_nip07_origin(origin) {
        return Ok(());
    }

    let Some(state) = state else {
        return Err(Nip07Response {
            result: None,
            error: Some("NIP-07 state not initialized".to_string()),
        });
    };

    match state
        .request_permission(origin, permission_type, method)
        .await
    {
        Ok(true) => Ok(()),
        Ok(false) => Err(Nip07Response {
            result: None,
            error: Some("Permission denied".to_string()),
        }),
        Err(error) => Err(Nip07Response {
            result: None,
            error: Some(error),
        }),
    }
}

pub async fn handle_nip07_request_inner(
    state: Option<&Nip07State>,
    method: &str,
    params: &serde_json::Value,
    origin: &str,
) -> Nip07Response {
    debug!("[NIP-07] Request: {} from {}", method, origin);

    match method {
        "getPublicKey" => {
            if let Err(response) =
                ensure_nip07_permission(state, origin, PermissionType::GetPublicKey, method).await
            {
                return response;
            }

            let Some(state) = state else {
                return Nip07Response {
                    result: None,
                    error: Some("NIP-07 state not initialized".to_string()),
                };
            };

            match state.current_account() {
                Ok(Some(account)) => Nip07Response {
                    result: Some(serde_json::Value::String(account.pubkey)),
                    error: None,
                },
                Ok(None) => Nip07Response {
                    result: None,
                    error: Some("No Nostr account signed in".to_string()),
                },
                Err(error) => Nip07Response {
                    result: None,
                    error: Some(error),
                },
            }
        }

        "signEvent" => {
            if let Err(response) =
                ensure_nip07_permission(state, origin, PermissionType::SignEvent, method).await
            {
                return response;
            }

            let Some(state) = state else {
                return Nip07Response {
                    result: None,
                    error: Some("NIP-07 state not initialized".to_string()),
                };
            };

            let keys = match state.signer_keys() {
                Ok(keys) => keys,
                Err(error) => {
                    return Nip07Response {
                        result: None,
                        error: Some(error),
                    };
                }
            };
            let unsigned = match unsigned_event_from_nip07_params(params, keys.public_key()) {
                Ok(unsigned) => unsigned,
                Err(error) => {
                    return Nip07Response {
                        result: None,
                        error: Some(error),
                    };
                }
            };
            let event = match unsigned.sign(&keys) {
                Ok(event) => event,
                Err(error) => {
                    return Nip07Response {
                        result: None,
                        error: Some(format!("Failed to sign event: {error}")),
                    };
                }
            };

            Nip07Response {
                result: serde_json::to_value(event).ok(),
                error: None,
            }
        }

        "getRelays" => Nip07Response {
            result: Some(serde_json::json!({})),
            error: None,
        },

        "nip04.encrypt" | "nip44.encrypt" => {
            if let Err(response) =
                ensure_nip07_permission(state, origin, PermissionType::Encrypt, method).await
            {
                return response;
            }

            let Some(state) = state else {
                return Nip07Response {
                    result: None,
                    error: Some("NIP-07 state not initialized".to_string()),
                };
            };

            let keys = match state.signer_keys() {
                Ok(keys) => keys,
                Err(error) => {
                    return Nip07Response {
                        result: None,
                        error: Some(error),
                    };
                }
            };
            let recipient = match nip07_pubkey_param(params) {
                Ok(pubkey) => pubkey,
                Err(error) => {
                    return Nip07Response {
                        result: None,
                        error: Some(error),
                    };
                }
            };
            let plaintext = match nip07_string_param(params, "plaintext") {
                Ok(plaintext) => plaintext,
                Err(error) => {
                    return Nip07Response {
                        result: None,
                        error: Some(error),
                    };
                }
            };

            let ciphertext = match method {
                "nip04.encrypt" => nip04::encrypt(keys.secret_key(), &recipient, plaintext)
                    .map_err(|error| format!("Failed to encrypt with NIP-04: {error}")),
                "nip44.encrypt" => {
                    nip44::encrypt(keys.secret_key(), &recipient, plaintext, nip44::Version::V2)
                        .map_err(|error| format!("Failed to encrypt with NIP-44: {error}"))
                }
                _ => unreachable!("encrypt branch only handles nip04/nip44"),
            };

            match ciphertext {
                Ok(ciphertext) => Nip07Response {
                    result: Some(serde_json::Value::String(ciphertext)),
                    error: None,
                },
                Err(error) => Nip07Response {
                    result: None,
                    error: Some(error),
                },
            }
        }

        "nip04.decrypt" | "nip44.decrypt" => {
            if let Err(response) =
                ensure_nip07_permission(state, origin, PermissionType::Decrypt, method).await
            {
                return response;
            }

            let Some(state) = state else {
                return Nip07Response {
                    result: None,
                    error: Some("NIP-07 state not initialized".to_string()),
                };
            };

            let keys = match state.signer_keys() {
                Ok(keys) => keys,
                Err(error) => {
                    return Nip07Response {
                        result: None,
                        error: Some(error),
                    };
                }
            };
            let sender = match nip07_pubkey_param(params) {
                Ok(pubkey) => pubkey,
                Err(error) => {
                    return Nip07Response {
                        result: None,
                        error: Some(error),
                    };
                }
            };
            let ciphertext = match nip07_string_param(params, "ciphertext") {
                Ok(ciphertext) => ciphertext,
                Err(error) => {
                    return Nip07Response {
                        result: None,
                        error: Some(error),
                    };
                }
            };

            let plaintext = match method {
                "nip04.decrypt" => nip04::decrypt(keys.secret_key(), &sender, ciphertext)
                    .map_err(|error| format!("Failed to decrypt with NIP-04: {error}")),
                "nip44.decrypt" => nip44::decrypt(keys.secret_key(), &sender, ciphertext)
                    .map_err(|error| format!("Failed to decrypt with NIP-44: {error}")),
                _ => unreachable!("decrypt branch only handles nip04/nip44"),
            };

            match plaintext {
                Ok(plaintext) => Nip07Response {
                    result: Some(serde_json::Value::String(plaintext)),
                    error: None,
                },
                Err(error) => Nip07Response {
                    result: None,
                    error: Some(error),
                },
            }
        }

        _ => Nip07Response {
            result: None,
            error: Some(format!("Unknown method: {}", method)),
        },
    }
}

/// Handle NIP-07 requests via htree://nip07/ protocol
pub fn handle_nip07_protocol_request(
    request: tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    let body = request.body();
    info!("[htree://nip07] Request: {} bytes", body.len());

    let nip07_request: Nip07Request = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(e) => {
            error!("[htree://nip07] Failed to parse request body: {}", e);
            let response = Nip07Response {
                result: None,
                error: Some(format!("Invalid request: {}", e)),
            };
            return tauri::http::Response::builder()
                .status(400)
                .header("content-type", "application/json")
                .header("access-control-allow-origin", "*")
                .body(serde_json::to_vec(&response).unwrap_or_default())
                .unwrap();
        }
    };

    let nip07_state = get_nip07_state();
    let response = handle_nip07_request_sync(nip07_state, nip07_request);

    tauri::http::Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .header("access-control-allow-origin", "*")
        .body(serde_json::to_vec(&response).unwrap_or_default())
        .unwrap()
}

pub fn handle_webview_event_protocol_request<R: Runtime>(
    app: AppHandle<R>,
    request: tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    let header_session_token = request
        .headers()
        .get("x-session-token")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);

    let (session_token, payload) = match header_session_token {
        Some(session_token) => match serde_json::from_slice(request.body()) {
            Ok(payload) => (session_token, payload),
            Err(error) => {
                warn!("[webview-event:http] Invalid payload: {}", error);
                return tauri::http::Response::builder()
                    .status(400)
                    .header("content-type", "text/plain")
                    .body(format!("Invalid webview event payload: {}", error).into_bytes())
                    .unwrap();
            }
        },
        None => match serde_json::from_slice::<WebviewEventHttpEnvelope>(request.body()) {
            Ok(envelope) if !envelope.session_token.trim().is_empty() => {
                (envelope.session_token, envelope.payload)
            }
            Ok(_) => {
                return tauri::http::Response::builder()
                    .status(401)
                    .header("content-type", "text/plain")
                    .body(b"Missing session token".to_vec())
                    .unwrap();
            }
            Err(error) => {
                warn!("[webview-event:http] Invalid payload envelope: {}", error);
                return tauri::http::Response::builder()
                    .status(400)
                    .header("content-type", "text/plain")
                    .body(format!("Invalid webview event payload: {}", error).into_bytes())
                    .unwrap();
            }
        },
    };

    debug!(
        "[webview-event:http] Received kind={} label={} origin={} url={:?}",
        payload.kind, payload.label, payload.origin, payload.url
    );

    match webview_event(app, payload, session_token) {
        Ok(()) => tauri::http::Response::builder()
            .status(204)
            .header("access-control-allow-origin", "*")
            .body(Vec::new())
            .unwrap(),
        Err(error) => {
            warn!("[webview-event:http] Rejected event: {}", error);
            tauri::http::Response::builder()
                .status(403)
                .header("content-type", "text/plain")
                .header("access-control-allow-origin", "*")
                .body(error.into_bytes())
                .unwrap()
        }
    }
}

pub async fn handle_nip07_http_bridge(body: Bytes) -> AxumResponse<Body> {
    let request = tauri::http::Request::builder()
        .uri("http://127.0.0.1/__iris_nip07")
        .body(body.to_vec())
        .unwrap_or_else(|_| tauri::http::Request::new(Vec::new()));
    tauri_response_to_axum(handle_nip07_protocol_request(request))
}

pub async fn handle_webview_event_http_bridge<R: Runtime>(
    app: AppHandle<R>,
    headers: HeaderMap,
    body: Bytes,
) -> AxumResponse<Body> {
    let mut builder = tauri::http::Request::builder().uri("http://127.0.0.1/__iris_webview");
    if let Some(session_token) = headers.get("x-session-token") {
        builder = builder.header("x-session-token", session_token);
    }
    let request = builder
        .body(body.to_vec())
        .unwrap_or_else(|_| tauri::http::Request::new(Vec::new()));
    tauri_response_to_axum(handle_webview_event_protocol_request(app, request))
}

// ============================================
// Tauri commands
// ============================================

#[tauri::command]
pub async fn nip07_request<R: Runtime>(
    app: AppHandle<R>,
    method: String,
    params: serde_json::Value,
    origin: String,
) -> Nip07Response {
    let nip07_state = app.try_state::<Arc<Nip07State>>();

    handle_nip07_request_inner(
        nip07_state.as_ref().map(|state| &***state),
        &method,
        &params,
        &origin,
    )
    .await
}

#[tauri::command]
pub fn get_nip07_account<R: Runtime>(
    app: AppHandle<R>,
) -> Result<Option<Nip07AccountSummary>, String> {
    let nip07_state = app
        .try_state::<Arc<Nip07State>>()
        .ok_or_else(|| "NIP-07 state not initialized".to_string())?;
    nip07_state.current_account()
}

#[tauri::command]
pub fn list_nip07_accounts<R: Runtime>(app: AppHandle<R>) -> Result<Nip07AccountsSummary, String> {
    let nip07_state = app
        .try_state::<Arc<Nip07State>>()
        .ok_or_else(|| "NIP-07 state not initialized".to_string())?;
    nip07_state.list_accounts()
}

#[tauri::command]
pub fn login_nip07_account<R: Runtime>(
    app: AppHandle<R>,
    secret: String,
) -> Result<Nip07AccountSummary, String> {
    let nip07_state = app
        .try_state::<Arc<Nip07State>>()
        .ok_or_else(|| "NIP-07 state not initialized".to_string())?;
    nip07_state.login_with_secret(secret)
}

#[tauri::command]
pub fn generate_nip07_account<R: Runtime>(
    app: AppHandle<R>,
) -> Result<Nip07AccountSummary, String> {
    let nip07_state = app
        .try_state::<Arc<Nip07State>>()
        .ok_or_else(|| "NIP-07 state not initialized".to_string())?;
    nip07_state.generate_account()
}

#[tauri::command]
pub fn logout_nip07_account<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    let nip07_state = app
        .try_state::<Arc<Nip07State>>()
        .ok_or_else(|| "NIP-07 state not initialized".to_string())?;
    nip07_state.logout()
}

#[tauri::command]
pub fn set_active_nip07_account<R: Runtime>(
    app: AppHandle<R>,
    pubkey: String,
) -> Result<Nip07AccountSummary, String> {
    let nip07_state = app
        .try_state::<Arc<Nip07State>>()
        .ok_or_else(|| "NIP-07 state not initialized".to_string())?;
    nip07_state.set_active_account(pubkey)
}

#[tauri::command]
pub fn remove_nip07_account<R: Runtime>(
    app: AppHandle<R>,
    pubkey: String,
) -> Result<Nip07AccountsSummary, String> {
    let nip07_state = app
        .try_state::<Arc<Nip07State>>()
        .ok_or_else(|| "NIP-07 state not initialized".to_string())?;
    nip07_state.remove_account(pubkey)
}

#[tauri::command]
pub fn export_nip07_account_secret<R: Runtime>(
    app: AppHandle<R>,
    pubkey: String,
) -> Result<String, String> {
    let nip07_state = app
        .try_state::<Arc<Nip07State>>()
        .ok_or_else(|| "NIP-07 state not initialized".to_string())?;
    nip07_state.export_account_secret(pubkey)
}

#[tauri::command]
pub async fn take_nip07_permission_prompt<R: Runtime>(
    app: AppHandle<R>,
) -> Result<Option<Nip07PermissionPrompt>, String> {
    let nip07_state = app
        .try_state::<Arc<Nip07State>>()
        .ok_or_else(|| "NIP-07 state not initialized".to_string())?;
    Ok(nip07_state.take_permission_prompt().await)
}

#[tauri::command]
pub async fn respond_nip07_permission_prompt<R: Runtime>(
    app: AppHandle<R>,
    request_id: String,
    decision: Nip07PermissionDecisionAction,
) -> Result<(), String> {
    let nip07_state = app
        .try_state::<Arc<Nip07State>>()
        .ok_or_else(|| "NIP-07 state not initialized".to_string())?;
    nip07_state
        .resolve_permission_prompt(&request_id, decision)
        .await
}

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
#[tauri::command]
pub async fn show_native_nip07_permission_dialog<R: Runtime>(
    app: AppHandle<R>,
    origin: String,
    method: String,
) -> Result<Nip07PermissionDecisionAction, String> {
    prompt_native_permission_decision(&app, &origin, &method).await
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
#[tauri::command]
pub async fn show_native_nip07_permission_dialog<R: Runtime>(
    _app: AppHandle<R>,
    _origin: String,
    _method: String,
) -> Result<Nip07PermissionDecisionAction, String> {
    Err("Native permission dialogs are not available on this platform".to_string())
}

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
#[tauri::command]
pub async fn create_nip07_webview<R: Runtime>(
    app: AppHandle<R>,
    label: String,
    url: String,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    scale: Option<f64>,
) -> Result<(), String> {
    info!("[NIP-07] Creating webview {} for {}", label, url);

    let server_url =
        crate::htree_protocol::get_htree_server_url().ok_or("htree server not running")?;

    let parsed_url = tauri::Url::parse(&url).map_err(|e| format!("Invalid URL: {}", e))?;
    let origin = if let Some(host) = parsed_url.host_str() {
        if let Some(port) = parsed_url.port() {
            format!("{}://{}:{}", parsed_url.scheme(), host, port)
        } else {
            format!("{}://{}", parsed_url.scheme(), host)
        }
    } else {
        parsed_url.scheme().to_string()
    };

    let nip07_state = app
        .try_state::<Arc<Nip07State>>()
        .ok_or("Nip07State not found")?;
    let session_token = nip07_state.new_session(&origin);

    let init_script = generate_nip07_script(&server_url, &session_token, &label, None, None, None);
    let diagnostic_probe_script = generate_webview_diagnostic_probe_script(
        &server_url,
        &session_token,
        &label,
        &origin,
        None,
        None,
        "page-load-probe",
    );

    let window = app.get_window("main").ok_or("Main window not found")?;

    let child_background_color =
        child_webview_placeholder_background_color(window.theme().unwrap_or(Theme::Light));

    let mut navigate_after_create: Option<tauri::Url> = None;
    let webview_url = if url.starts_with("tauri://localhost/") {
        let mut path = parsed_url.path().trim_start_matches('/').to_string();
        if path.is_empty() {
            path = "index.html".to_string();
        }
        if parsed_url.fragment().is_some() || parsed_url.query().is_some() {
            navigate_after_create = Some(parsed_url.clone());
        }
        WebviewUrl::App(path.into())
    } else {
        webview_url_for_parsed_url(&parsed_url)
    };

    let app_for_nav = app.clone();
    let label_for_nav = label.clone();
    let app_for_load = app.clone();
    let label_for_load = label.clone();
    let init_script_for_load = init_script.clone();
    let diagnostic_probe_script_for_load = diagnostic_probe_script.clone();

    let webview_builder = WebviewBuilder::new(&label, webview_url)
        .initialization_script(&init_script)
        .auto_resize()
        .background_color(child_background_color)
        .on_navigation(move |nav_url| {
            let url_str = nav_url.to_string();
            debug!("[NIP-07] Child webview navigating to: {}", url_str);
            let _ = app_for_nav.emit(
                "child-webview-location",
                serde_json::json!({
                    "label": label_for_nav,
                    "url": url_str,
                    "source": "navigation"
                }),
            );
            true
        })
        .on_page_load(move |_webview, payload| {
            let event = match payload.event() {
                tauri::webview::PageLoadEvent::Started => "started",
                tauri::webview::PageLoadEvent::Finished => "finished",
            };
            let context = format!("page-load:{event}");
            inject_child_init_script(
                &app_for_load,
                &label_for_load,
                &init_script_for_load,
                &context,
            );
            if matches!(payload.event(), tauri::webview::PageLoadEvent::Finished) {
                inject_child_init_script(
                    &app_for_load,
                    &label_for_load,
                    &diagnostic_probe_script_for_load,
                    "page-load:finished-diagnostic-probe",
                );
                schedule_child_init_script_retry(
                    app_for_load.clone(),
                    label_for_load.clone(),
                    init_script_for_load.clone(),
                    Duration::from_millis(150),
                    "page-load:finished-retry-150ms".to_string(),
                );
                schedule_child_init_script_retry(
                    app_for_load.clone(),
                    label_for_load.clone(),
                    init_script_for_load.clone(),
                    Duration::from_millis(1000),
                    "page-load:finished-retry-1000ms".to_string(),
                );
                schedule_child_init_script_retry(
                    app_for_load.clone(),
                    label_for_load.clone(),
                    diagnostic_probe_script_for_load.clone(),
                    Duration::from_millis(150),
                    "page-load:finished-diagnostic-probe-150ms".to_string(),
                );
                schedule_child_init_script_retry(
                    app_for_load.clone(),
                    label_for_load.clone(),
                    diagnostic_probe_script_for_load.clone(),
                    Duration::from_millis(1000),
                    "page-load:finished-diagnostic-probe-1000ms".to_string(),
                );
            }
            let _ = app_for_load.emit(
                "child-webview-page-load",
                serde_json::json!({
                    "label": label_for_load,
                    "url": payload.url().to_string(),
                    "event": event
                }),
            );
        });

    let bounds = desktop_child_webview_bounds(x, y, width, height, scale);
    let webview = window
        .add_child(webview_builder, bounds.position, bounds.size)
        .map_err(|e| format!("Failed to create webview: {}", e))?;

    if let Some(target_url) = navigate_after_create {
        if let Err(e) = webview.navigate(target_url) {
            warn!("[NIP-07] Failed to set initial URL: {}", e);
        }
    }

    info!("[NIP-07] Webview created with session token for {}", origin);
    Ok(())
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
#[tauri::command]
pub async fn create_nip07_webview<R: Runtime>(
    app: AppHandle<R>,
    label: String,
    url: String,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    scale: f64,
) -> Result<(), String> {
    let server_url =
        crate::htree_protocol::get_htree_server_url().ok_or("htree server not running")?;

    let parsed_url = tauri::Url::parse(&url).map_err(|e| format!("Invalid URL: {}", e))?;
    let origin = url_origin(&parsed_url).unwrap_or_else(|| parsed_url.scheme().to_string());
    let allowed_origin_rule = url_origin(&parsed_url);

    let nip07_state = app
        .try_state::<Arc<Nip07State>>()
        .ok_or("Nip07State not found")?;
    let session_token = nip07_state.new_session(&origin);

    let init_script = generate_nip07_script(&server_url, &session_token, &label, None, None, None);
    let diagnostic_probe_script = generate_webview_diagnostic_probe_script(
        &server_url,
        &session_token,
        &label,
        &origin,
        None,
        None,
        "page-load-probe",
    );

    app.mobile_browser().create(BrowserCreateRequest {
        label,
        url,
        x,
        y,
        width,
        height,
        scale,
        init_script,
        diagnostic_script: diagnostic_probe_script,
        allowed_origin_rule,
        actual_url_root: None,
        canonical_url_root: None,
        server_url: None,
        session_token: None,
    })
}

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
#[tauri::command]
pub async fn create_htree_webview<R: Runtime>(
    app: AppHandle<R>,
    label: String,
    host: Option<String>,
    nhash: Option<String>,
    npub: Option<String>,
    treename: Option<String>,
    path: String,
    query: Option<String>,
    fragment: Option<String>,
    cache_bust: Option<String>,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    scale: Option<f64>,
    prefer_plain_loopback_host: Option<bool>,
) -> Result<(), String> {
    let server_url =
        crate::htree_protocol::get_htree_server_url().ok_or("htree server not running")?;
    let use_origin_isolated_hosts =
        use_origin_isolated_loopback_hosts() && !prefer_plain_loopback_host.unwrap_or(false);

    // The child webview keeps a canonical htree:// identity for permissions and
    // diagnostics, but it loads over a per-root loopback host so the browser's
    // own origin model isolates storage, service workers, and other origin-
    // scoped state between different trees and nhashes.
    let (canonical_url, actual_url_base, origin, canonical_url_root, actual_url_root) =
        if let Some(nhash) = &nhash {
            let request_host = host.as_deref().unwrap_or(nhash);
            let (canonical_url, canonical_root) = if let Some(treename) = &treename {
                let resolved_host = resolve_tree_request_host(
                    request_host,
                    crate::htree_protocol::get_self_npub(),
                )?;
                (
                    append_fragment(
                        append_query(
                            htree_url_from_tree_host(resolved_host, treename, &path),
                            query.as_deref(),
                        ),
                        fragment.as_deref(),
                    ),
                    htree_url_from_tree_host(resolved_host, treename, "/")
                        .trim_end_matches('/')
                        .to_string(),
                )
            } else {
                (
                    append_fragment(
                        append_query(htree_url_from_nhash(request_host, &path), query.as_deref()),
                        fragment.as_deref(),
                    ),
                    htree_url_from_nhash(request_host, "/")
                        .trim_end_matches('/')
                        .to_string(),
                )
            };
            let actual_url = append_query(
                daemon_proxy_url_from_nhash(&server_url, nhash, &path, use_origin_isolated_hosts)?,
                query.as_deref(),
            );
            let actual_root =
                daemon_proxy_url_from_nhash(&server_url, nhash, "/", use_origin_isolated_hosts)?
                    .trim_end_matches('/')
                    .to_string();
            let origin = canonical_root.clone();
            (
                canonical_url,
                actual_url,
                origin,
                canonical_root,
                actual_root,
            )
        } else if let Some(treename) = &treename {
            let request_host = host
                .as_deref()
                .or(npub.as_deref())
                .ok_or_else(|| "Either nhash or (host + treename) must be provided".to_string())?;
            let resolved_host =
                resolve_tree_request_host(request_host, crate::htree_protocol::get_self_npub())?;
            let canonical_url = append_fragment(
                append_query(
                    htree_url_from_tree_host(resolved_host, treename, &path),
                    query.as_deref(),
                ),
                fragment.as_deref(),
            );
            let canonical_root = htree_url_from_tree_host(resolved_host, treename, "/")
                .trim_end_matches('/')
                .to_string();
            let actual_url = append_query(
                daemon_proxy_url_from_tree_host(
                    &server_url,
                    resolved_host,
                    treename,
                    &path,
                    use_origin_isolated_hosts,
                )?,
                query.as_deref(),
            );
            let actual_root = daemon_proxy_url_from_tree_host(
                &server_url,
                resolved_host,
                treename,
                "/",
                use_origin_isolated_hosts,
            )?
            .trim_end_matches('/')
            .to_string();
            let origin = canonical_root.clone();
            (
                canonical_url,
                actual_url,
                origin,
                canonical_root,
                actual_root,
            )
        } else {
            return Err("Either nhash or (host + treename) must be provided".to_string());
        };

    info!(
        "[htree] Creating webview {} for {} (origin: {})",
        label, canonical_url, origin
    );

    if let Ok(parsed_actual_root) = tauri::Url::parse(&actual_url_root) {
        if let Some(host) = parsed_actual_root.host_str() {
            if host.ends_with(".htree.localhost") {
                register_virtual_tree_host(host, parsed_actual_root.path());
            }
        }
    }

    let nip07_state = app
        .try_state::<Arc<Nip07State>>()
        .ok_or("Nip07State not found")?;
    let session_token = nip07_state.new_session(&origin);
    let actual_url = append_internal_htree_query_params(
        &actual_url_base,
        &server_url,
        &canonical_url,
        &session_token,
        cache_bust.as_deref(),
    )?;
    let actual_url = append_fragment(actual_url, fragment.as_deref());

    let init_script = generate_nip07_script(
        &server_url,
        &session_token,
        &label,
        Some(&origin),
        Some(&canonical_url_root),
        Some(&actual_url_root),
    );
    let diagnostic_probe_script = generate_webview_diagnostic_probe_script(
        &server_url,
        &session_token,
        &label,
        &origin,
        Some(&canonical_url_root),
        Some(&actual_url_root),
        "page-load-probe",
    );

    let window = app.get_window("main").ok_or("Main window not found")?;
    let parsed_url = tauri::Url::parse(&actual_url).map_err(|e| format!("Invalid URL: {}", e))?;
    let child_background_color =
        child_webview_placeholder_background_color(window.theme().unwrap_or(Theme::Light));

    let app_for_nav = app.clone();
    let label_for_nav = label.clone();
    let app_for_load = app.clone();
    let label_for_load = label.clone();
    let init_script_for_load = init_script.clone();
    let diagnostic_probe_script_for_load = diagnostic_probe_script.clone();

    let canonical_url_root_for_nav = canonical_url_root.clone();
    let actual_url_root_for_nav = actual_url_root.clone();
    let canonical_url_root_for_load = canonical_url_root.clone();
    let actual_url_root_for_load = actual_url_root.clone();

    let webview_builder = WebviewBuilder::new(&label, webview_url_for_parsed_url(&parsed_url))
        .initialization_script(&init_script)
        .auto_resize()
        .background_color(child_background_color)
        .on_navigation(move |nav_url| {
            let url_str = canonicalize_child_webview_url(
                &nav_url.to_string(),
                &actual_url_root_for_nav,
                &canonical_url_root_for_nav,
            );
            debug!("[htree] Child webview navigating to: {}", url_str);
            let _ = app_for_nav.emit(
                "child-webview-location",
                serde_json::json!({
                    "label": label_for_nav,
                    "url": url_str,
                    "source": "navigation"
                }),
            );
            true
        })
        .on_page_load(move |_webview, payload| {
            let event = match payload.event() {
                tauri::webview::PageLoadEvent::Started => "started",
                tauri::webview::PageLoadEvent::Finished => "finished",
            };
            let context = format!("page-load:{event}");
            inject_child_init_script(
                &app_for_load,
                &label_for_load,
                &init_script_for_load,
                &context,
            );
            if matches!(payload.event(), tauri::webview::PageLoadEvent::Finished) {
                inject_child_init_script(
                    &app_for_load,
                    &label_for_load,
                    &diagnostic_probe_script_for_load,
                    "page-load:finished-diagnostic-probe",
                );
                schedule_child_init_script_retry(
                    app_for_load.clone(),
                    label_for_load.clone(),
                    init_script_for_load.clone(),
                    Duration::from_millis(150),
                    "page-load:finished-retry-150ms".to_string(),
                );
                schedule_child_init_script_retry(
                    app_for_load.clone(),
                    label_for_load.clone(),
                    init_script_for_load.clone(),
                    Duration::from_millis(1000),
                    "page-load:finished-retry-1000ms".to_string(),
                );
                schedule_child_init_script_retry(
                    app_for_load.clone(),
                    label_for_load.clone(),
                    diagnostic_probe_script_for_load.clone(),
                    Duration::from_millis(150),
                    "page-load:finished-diagnostic-probe-150ms".to_string(),
                );
                schedule_child_init_script_retry(
                    app_for_load.clone(),
                    label_for_load.clone(),
                    diagnostic_probe_script_for_load.clone(),
                    Duration::from_millis(1000),
                    "page-load:finished-diagnostic-probe-1000ms".to_string(),
                );
            }
            let url_str = canonicalize_child_webview_url(
                &payload.url().to_string(),
                &actual_url_root_for_load,
                &canonical_url_root_for_load,
            );
            let _ = app_for_load.emit(
                "child-webview-page-load",
                serde_json::json!({
                    "label": label_for_load,
                    "url": url_str,
                    "event": event
                }),
            );
        });

    let bounds = desktop_child_webview_bounds(x, y, width, height, scale);
    window
        .add_child(webview_builder, bounds.position, bounds.size)
        .map_err(|e| format!("Failed to create webview: {}", e))?;

    info!(
        "[htree] Webview created with session token for origin {}",
        origin
    );
    Ok(())
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
#[tauri::command]
pub async fn create_htree_webview<R: Runtime>(
    app: AppHandle<R>,
    label: String,
    host: Option<String>,
    nhash: Option<String>,
    npub: Option<String>,
    treename: Option<String>,
    path: String,
    query: Option<String>,
    fragment: Option<String>,
    cache_bust: Option<String>,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    scale: f64,
    _prefer_plain_loopback_host: Option<bool>,
) -> Result<(), String> {
    let server_url =
        crate::htree_protocol::get_htree_server_url().ok_or("htree server not running")?;

    let (_canonical_url, actual_url_base, origin, canonical_url_root, actual_url_root) =
        if let Some(nhash) = &nhash {
            let request_host = host.as_deref().unwrap_or(nhash);
            let canonical_url = append_fragment(
                append_query(htree_url_from_nhash(request_host, &path), query.as_deref()),
                fragment.as_deref(),
            );
            let canonical_root = htree_url_from_nhash(request_host, "/")
                .trim_end_matches('/')
                .to_string();
            let actual_url = append_query(
                daemon_proxy_url_from_nhash(
                    &server_url,
                    request_host,
                    &path,
                    use_origin_isolated_loopback_hosts(),
                )?,
                query.as_deref(),
            );
            let actual_root = daemon_proxy_url_from_nhash(
                &server_url,
                request_host,
                "/",
                use_origin_isolated_loopback_hosts(),
            )?
            .trim_end_matches('/')
            .to_string();
            let origin = canonical_root.clone();
            (
                canonical_url,
                actual_url,
                origin,
                canonical_root,
                actual_root,
            )
        } else if let Some(treename) = &treename {
            let request_host = host
                .as_deref()
                .or(npub.as_deref())
                .ok_or_else(|| "Either nhash or (host + treename) must be provided".to_string())?;
            let resolved_host =
                resolve_tree_request_host(request_host, crate::htree_protocol::get_self_npub())?;
            let canonical_url = append_fragment(
                append_query(
                    htree_url_from_tree_host(resolved_host, treename, &path),
                    query.as_deref(),
                ),
                fragment.as_deref(),
            );
            let canonical_root = htree_url_from_tree_host(resolved_host, treename, "/")
                .trim_end_matches('/')
                .to_string();
            let actual_url = append_query(
                daemon_proxy_url_from_tree_host(
                    &server_url,
                    resolved_host,
                    treename,
                    &path,
                    use_origin_isolated_loopback_hosts(),
                )?,
                query.as_deref(),
            );
            let actual_root = daemon_proxy_url_from_tree_host(
                &server_url,
                resolved_host,
                treename,
                "/",
                use_origin_isolated_loopback_hosts(),
            )?
            .trim_end_matches('/')
            .to_string();
            let origin = canonical_root.clone();
            (
                canonical_url,
                actual_url,
                origin,
                canonical_root,
                actual_root,
            )
        } else {
            return Err("Either nhash or (host + treename) must be provided".to_string());
        };

    let nip07_state = app
        .try_state::<Arc<Nip07State>>()
        .ok_or("Nip07State not found")?;
    let session_token = nip07_state.new_session(&origin);
    let actual_url = append_internal_htree_query_params(
        &actual_url_base,
        &server_url,
        &_canonical_url,
        &session_token,
        cache_bust.as_deref(),
    )?;
    let actual_url = append_fragment(actual_url, fragment.as_deref());

    let init_script = generate_nip07_script(
        &server_url,
        &session_token,
        &label,
        Some(&origin),
        Some(&canonical_url_root),
        Some(&actual_url_root),
    );
    let diagnostic_probe_script = generate_webview_diagnostic_probe_script(
        &server_url,
        &session_token,
        &label,
        &origin,
        Some(&canonical_url_root),
        Some(&actual_url_root),
        "page-load-probe",
    );
    let actual_parsed_url =
        tauri::Url::parse(&actual_url).map_err(|e| format!("Invalid URL: {}", e))?;

    app.mobile_browser().create(BrowserCreateRequest {
        label,
        url: actual_url,
        x,
        y,
        width,
        height,
        scale,
        init_script,
        diagnostic_script: diagnostic_probe_script,
        allowed_origin_rule: url_origin(&actual_parsed_url),
        actual_url_root: Some(actual_url_root),
        canonical_url_root: Some(canonical_url_root),
        server_url: Some(server_url),
        session_token: Some(session_token),
    })
}

fn remap_child_webview_url_to_canonical_root(
    url: &str,
    actual_url_root: &str,
    canonical_url_root: &str,
) -> Option<String> {
    if let Some(suffix) = url.strip_prefix(actual_url_root) {
        return Some(format!("{}{}", canonical_url_root, suffix));
    }

    let actual_root = tauri::Url::parse(actual_url_root).ok()?;
    let candidate = tauri::Url::parse(url).ok()?;
    if actual_root.origin().ascii_serialization() != candidate.origin().ascii_serialization() {
        return None;
    }

    let actual_root_path = actual_root.path().trim_end_matches('/');
    let candidate_path = candidate.path();
    if actual_root_path.is_empty()
        || candidate_path == actual_root_path
        || candidate_path.starts_with(&format!("{actual_root_path}/"))
    {
        return None;
    }

    let mut mapped = format!("{}{}", canonical_url_root, candidate_path);
    if let Some(query) = candidate.query() {
        mapped.push('?');
        mapped.push_str(query);
    }
    if let Some(fragment) = candidate.fragment() {
        mapped.push('#');
        mapped.push_str(fragment);
    }
    Some(mapped)
}

fn canonicalize_child_webview_url(
    url: &str,
    actual_url_root: &str,
    canonical_url_root: &str,
) -> String {
    let canonical_url =
        remap_child_webview_url_to_canonical_root(url, actual_url_root, canonical_url_root)
            .unwrap_or_else(|| url.to_string());
    strip_internal_htree_query_params(&canonical_url)
}

fn strip_internal_htree_query_params(url: &str) -> String {
    let Ok(mut parsed) = tauri::Url::parse(url) else {
        return url.to_string();
    };

    let retained_query: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(key, _)| {
            key != "iris_htree_server"
                && key != "iris_htree_canonical"
                && key != "iris_htree_session"
                && key != "iris_htree_root"
        })
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();

    parsed.set_query(None);
    if !retained_query.is_empty() {
        let mut query_pairs = parsed.query_pairs_mut();
        for (key, value) in retained_query {
            query_pairs.append_pair(&key, &value);
        }
    }

    parsed.into()
}

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
#[tauri::command]
pub fn close_webview<R: Runtime>(app: AppHandle<R>, label: String) -> Result<(), String> {
    if let Some(webview) = app.get_webview(&label) {
        webview
            .close()
            .map_err(|e| format!("Failed to close webview: {}", e))?;
        info!("[NIP-07] Closed webview {}", label);
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
#[tauri::command]
pub fn close_webview<R: Runtime>(app: AppHandle<R>, label: String) -> Result<(), String> {
    app.mobile_browser().close(label)
}

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
#[tauri::command]
pub fn navigate_webview<R: Runtime>(
    app: AppHandle<R>,
    label: String,
    url: String,
) -> Result<(), String> {
    let webview = app
        .get_webview(&label)
        .ok_or_else(|| format!("Webview {} not found", label))?;
    let parsed = tauri::Url::parse(&url).map_err(|e| format!("Invalid URL: {}", e))?;
    webview
        .navigate(parsed)
        .map_err(|e| format!("Failed to navigate: {}", e))?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
#[tauri::command]
pub fn navigate_webview<R: Runtime>(
    app: AppHandle<R>,
    label: String,
    url: String,
) -> Result<(), String> {
    app.mobile_browser().navigate(label, url)
}

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
#[tauri::command]
pub fn set_webview_bounds<R: Runtime>(
    app: AppHandle<R>,
    label: String,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    scale: Option<f64>,
) -> Result<(), String> {
    let webview = app
        .get_webview(&label)
        .ok_or_else(|| format!("Webview {} not found", label))?;
    let bounds = desktop_child_webview_bounds(x, y, width, height, scale);
    webview
        .set_bounds(bounds)
        .map_err(|e| format!("Failed to set bounds: {}", e))?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
#[tauri::command]
pub fn set_webview_bounds<R: Runtime>(
    app: AppHandle<R>,
    label: String,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    scale: f64,
) -> Result<(), String> {
    app.mobile_browser().set_bounds(BrowserBoundsRequest {
        label,
        x,
        y,
        width,
        height,
        scale,
    })
}

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
#[tauri::command]
pub fn set_mobile_shell_overlay<R: Runtime>(
    _app: AppHandle<R>,
    _enabled: bool,
    _x: f64,
    _y: f64,
    _width: f64,
    _height: f64,
    _scale: Option<f64>,
) -> Result<(), String> {
    Ok(())
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
#[tauri::command]
pub fn set_mobile_shell_overlay<R: Runtime>(
    app: AppHandle<R>,
    enabled: bool,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    scale: f64,
) -> Result<(), String> {
    app.mobile_browser().set_shell_overlay(ShellOverlayRequest {
        enabled,
        x,
        y,
        width,
        height,
        scale,
    })
}

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
#[tauri::command]
pub fn webview_history<R: Runtime>(
    app: AppHandle<R>,
    label: String,
    direction: String,
) -> Result<(), String> {
    let webview = app
        .get_webview(&label)
        .ok_or_else(|| format!("Webview {} not found", label))?;
    let script = match direction.as_str() {
        "back" => "history.back()",
        "forward" => "history.forward()",
        _ => return Err("Invalid history direction".to_string()),
    };
    webview
        .eval(script)
        .map_err(|e| format!("Failed to navigate history: {}", e))?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
#[tauri::command]
pub fn webview_history<R: Runtime>(
    app: AppHandle<R>,
    label: String,
    direction: String,
) -> Result<(), String> {
    app.mobile_browser().history(label, direction)
}

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
#[tauri::command]
pub fn reload_webview<R: Runtime>(app: AppHandle<R>, label: String) -> Result<(), String> {
    let webview = app
        .get_webview(&label)
        .ok_or_else(|| format!("Webview {} not found", label))?;
    webview
        .eval("location.reload()")
        .map_err(|e| format!("Failed to reload webview: {}", e))?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
#[tauri::command]
pub fn reload_webview<R: Runtime>(app: AppHandle<R>, label: String) -> Result<(), String> {
    app.mobile_browser().reload(label)
}

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
pub fn run_webview_script<R: Runtime>(
    app: AppHandle<R>,
    label: String,
    script: String,
) -> Result<(), String> {
    let script = script.trim();
    if script.is_empty() {
        return Err("Script cannot be empty".to_string());
    }
    let webview = app
        .get_webview(&label)
        .ok_or_else(|| format!("Webview {} not found", label))?;
    webview
        .eval(script)
        .map_err(|error| format!("Failed to run child webview script: {}", error))?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
pub fn run_webview_script<R: Runtime>(
    app: AppHandle<R>,
    label: String,
    _script: String,
) -> Result<(), String> {
    let _ = (app, label);
    Err("Child webview scripting is not supported on mobile yet".to_string())
}

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
pub fn run_webview_nip07_probe<R: Runtime>(
    app: AppHandle<R>,
    label: String,
    scenario: String,
) -> Result<(), String> {
    let webview = app
        .get_webview(&label)
        .ok_or_else(|| format!("Webview {} not found", label))?;
    webview
        .eval(&build_webview_nip07_probe_script(&scenario))
        .map_err(|error| format!("Failed to run NIP-07 probe in webview: {}", error))?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
pub fn run_webview_nip07_probe<R: Runtime>(
    app: AppHandle<R>,
    label: String,
    _scenario: String,
) -> Result<(), String> {
    let _ = (app, label);
    Err("NIP-07 child webview probing is not supported on mobile yet".to_string())
}

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
#[tauri::command]
pub fn webview_current_url<R: Runtime>(app: AppHandle<R>, label: String) -> Result<String, String> {
    let webview = app
        .get_webview(&label)
        .ok_or_else(|| format!("Webview {} not found", label))?;
    webview
        .url()
        .map(|url| url.to_string())
        .map_err(|e| format!("Failed to read webview URL: {}", e))
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
#[tauri::command]
pub fn webview_current_url<R: Runtime>(app: AppHandle<R>, label: String) -> Result<String, String> {
    app.mobile_browser().current_url(label)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebviewEventRequest {
    kind: String,
    label: String,
    origin: String,
    url: Option<String>,
    source: Option<String>,
    action: Option<String>,
    title: Option<String>,
    ready_state: Option<String>,
    body_text: Option<String>,
    media_summary: Option<String>,
    viewport_width: Option<i32>,
    viewport_height: Option<i32>,
    manifest_url: Option<String>,
    manifest_name: Option<String>,
    manifest_icon_url: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebviewEventHttpEnvelope {
    #[serde(rename = "sessionToken", alias = "session_token")]
    session_token: String,
    payload: WebviewEventRequest,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IrisRelaySessionQuery {
    #[serde(rename = "sessionToken", alias = "session_token")]
    pub session_token: Option<String>,
}

pub async fn handle_authenticated_relay_websocket<R: Runtime>(
    app: AppHandle<R>,
    State(state): State<hashtree_cli::server::AppState>,
    Query(query): Query<IrisRelaySessionQuery>,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, StatusCode> {
    let token = query
        .session_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let nip07_state = app
        .try_state::<Arc<Nip07State>>()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    if !nip07_state.validate_any_token(token) {
        warn!("[iris-relay] Rejecting websocket with invalid session token");
        return Err(StatusCode::UNAUTHORIZED);
    }

    let client_pubkey = nip07_state
        .current_account()
        .map_err(|error| {
            warn!("[iris-relay] Failed to resolve active account: {}", error);
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .map(|account| account.pubkey);

    Ok(hashtree_cli::server::ws_relay::ws_data_with_client_pubkey(
        state,
        ws,
        client_pubkey,
    ))
}

#[tauri::command]
pub fn webview_event<R: Runtime>(
    app: AppHandle<R>,
    payload: WebviewEventRequest,
    session_token: String,
) -> Result<(), String> {
    let nip07_state =
        get_nip07_state().ok_or_else(|| "NIP-07 state not initialized".to_string())?;

    if !nip07_state.validate_token(&payload.origin, &session_token) {
        let message = format!(
            "[webview-event] Invalid session token for kind={} label={} origin={}",
            payload.kind, payload.label, payload.origin
        );
        if payload.kind == "location" {
            debug!("{}", message);
        } else {
            warn!("{}", message);
        }
        return Err("Invalid session token".to_string());
    }

    debug!(
        "[webview-event] kind={} label={} origin={} url={:?} source={:?} media_summary={:?} error={:?}",
        payload.kind,
        payload.label,
        payload.origin,
        payload.url,
        payload.source,
        payload.media_summary,
        payload.error
    );

    match payload.kind.as_str() {
        "location" => {
            let url = payload
                .url
                .clone()
                .ok_or_else(|| "Missing url".to_string())?;
            let source = payload
                .source
                .clone()
                .unwrap_or_else(|| "unknown".to_string());
            let _ = app.emit(
                "child-webview-location",
                serde_json::json!({
                    "label": payload.label,
                    "url": url,
                    "source": source
                }),
            );
        }
        "navigate" => {
            let action = match payload.action.as_deref() {
                Some("back") => "back",
                Some("forward") => "forward",
                _ => return Err("Invalid action".to_string()),
            };
            let _ = app.emit(
                "child-webview-navigate",
                serde_json::json!({
                    "label": payload.label,
                    "action": action
                }),
            );
        }
        "diagnostic" => {
            let _ = app.emit(
                "child-webview-diagnostic",
                serde_json::json!({
                    "label": payload.label,
                    "url": payload.url,
                    "source": payload.source,
                    "title": payload.title,
                    "readyState": payload.ready_state,
                    "bodyText": payload.body_text,
                    "mediaSummary": payload.media_summary,
                    "viewportWidth": payload.viewport_width,
                    "viewportHeight": payload.viewport_height,
                    "manifestUrl": payload.manifest_url,
                    "manifestName": payload.manifest_name,
                    "manifestIconUrl": payload.manifest_icon_url,
                    "error": payload.error
                }),
            );
        }
        _ => {
            return Err("Invalid event kind".to_string());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;

    const TEST_SECRET_HEX: &str =
        "1111111111111111111111111111111111111111111111111111111111111111";
    const SECOND_TEST_SECRET_HEX: &str =
        "2222222222222222222222222222222222222222222222222222222222222222";

    #[derive(Default)]
    struct MemoryConfidentialStore {
        blob: RwLock<Option<ConfidentialBlob>>,
        blob_loads: AtomicUsize,
    }

    impl MemoryConfidentialStore {
        fn stored_secret(&self, pubkey: &str) -> Option<String> {
            self.blob
                .read()
                .as_ref()
                .and_then(|blob| confidential_blob_nsec(blob, pubkey))
        }

        fn blob_load_count(&self) -> usize {
            self.blob_loads.load(Ordering::SeqCst)
        }
    }

    impl ConfidentialStore for MemoryConfidentialStore {
        fn load_blob(&self) -> Result<Option<ConfidentialBlob>, String> {
            self.blob_loads.fetch_add(1, Ordering::SeqCst);
            Ok(self.blob.read().clone())
        }

        fn save_blob(&self, blob: &ConfidentialBlob) -> Result<(), String> {
            *self.blob.write() = Some(blob.clone());
            Ok(())
        }

        fn clear_blob(&self) -> Result<(), String> {
            *self.blob.write() = None;
            Ok(())
        }
    }

    fn test_nip07_state() -> (tempfile::TempDir, Nip07State) {
        let (temp_dir, state, _secret_store) = test_nip07_state_with_store();
        (temp_dir, state)
    }

    fn test_nip07_state_with_store() -> (tempfile::TempDir, Nip07State, Arc<MemoryConfidentialStore>)
    {
        let temp_dir = tempdir().expect("tempdir");
        let storage_path = temp_dir.path().join("nip07-account.json");
        let secret_store = Arc::new(MemoryConfidentialStore::default());
        let state = Nip07State::new(
            Arc::new(PermissionStore::new(None)),
            storage_path,
            secret_store.clone(),
        );
        (temp_dir, state, secret_store)
    }

    #[test]
    fn nhash_origin_uses_root_identity() {
        assert_eq!(
            htree_origin_from_nhash("nhash1example"),
            "htree://nhash1example"
        );
    }

    #[test]
    fn tree_origin_uses_tree_root_identity() {
        assert_eq!(
            htree_origin_from_tree_host("npub1example", "video"),
            "htree://npub1example/video"
        );
    }

    #[test]
    fn child_webview_scale_defaults_to_one_for_invalid_values() {
        assert_eq!(normalized_child_webview_scale(None), 1.0);
        assert_eq!(normalized_child_webview_scale(Some(0.0)), 1.0);
        assert_eq!(normalized_child_webview_scale(Some(-2.0)), 1.0);
        assert_eq!(normalized_child_webview_scale(Some(f64::NAN)), 1.0);
        assert_eq!(normalized_child_webview_scale(Some(2.0)), 2.0);
    }

    #[test]
    fn child_webview_dimensions_scale_css_pixels_to_physical_pixels() {
        assert_eq!(
            scaled_child_webview_dimensions(0.0, 48.0, 600.0, 340.0, Some(2.0)),
            (0.0, 96.0, 1200.0, 680.0)
        );
    }

    #[test]
    fn child_webview_placeholder_background_matches_shell_header_theme() {
        assert_eq!(
            child_webview_placeholder_background_color(Theme::Light),
            tauri::utils::config::Color(235, 235, 235, 255)
        );
        assert_eq!(
            child_webview_placeholder_background_color(Theme::Dark),
            tauri::utils::config::Color(24, 24, 24, 255)
        );
    }

    #[test]
    fn npub_urls_use_path_segments() {
        assert_eq!(
            htree_url_from_tree_host("npub1example", "public", "/index.html"),
            "htree://npub1example/public/index.html"
        );
    }

    #[test]
    fn npub_urls_encode_tree_name_as_single_segment() {
        assert_eq!(
            htree_url_from_tree_host("npub1example", "videos/My Clip", "/index.html"),
            "htree://npub1example/videos%2FMy%20Clip/index.html"
        );
    }

    #[test]
    fn append_fragment_keeps_hash_routes_after_query_strings() {
        let url = append_fragment(
            append_query("htree://npub1example/git/".to_string(), Some("smoke=1")),
            Some("/npub1owner/hashtree?tab=pulls"),
        );
        assert_eq!(
            url,
            "htree://npub1example/git/?smoke=1#/npub1owner/hashtree?tab=pulls"
        );
    }

    #[test]
    fn self_urls_use_same_tree_path_shape() {
        assert_eq!(
            htree_url_from_tree_host("self", "video", "/index.html"),
            "htree://self/video/index.html"
        );
    }

    #[test]
    fn tree_root_urls_keep_trailing_slash_for_relative_assets() {
        assert_eq!(
            htree_url_from_tree_host("self", "video", "/"),
            "htree://self/video/"
        );
    }

    #[test]
    fn daemon_proxy_tree_urls_use_origin_isolated_loopback_hosts() {
        let url = daemon_proxy_url_from_tree_host(
            "http://127.0.0.1:21417",
            "npub1example",
            "videos/My Clip",
            "/index.html",
            use_origin_isolated_loopback_hosts(),
        )
        .unwrap();
        let parsed = tauri::Url::parse(&url).expect("valid URL");
        assert_eq!(
            parsed.path(),
            "/htree/npub1example/videos%2FMy%20Clip/index.html"
        );
        let host = parsed.host_str().expect("loopback host");
        if use_origin_isolated_loopback_hosts() {
            assert!(
                host.ends_with(".htree.localhost"),
                "expected isolated loopback host, got {url}"
            );
        } else {
            assert_eq!(host, "127.0.0.1", "expected plain loopback host, got {url}");
        }
    }

    #[test]
    fn daemon_proxy_tree_root_urls_keep_trailing_slash() {
        let url = daemon_proxy_url_from_tree_host(
            "http://127.0.0.1:21417",
            "npub1example",
            "video",
            "/",
            use_origin_isolated_loopback_hosts(),
        )
        .unwrap();
        assert!(
            url.ends_with("/htree/npub1example/video/"),
            "expected tree root URL to keep trailing slash, got {url}"
        );
    }

    #[test]
    fn daemon_proxy_nhash_urls_use_embedded_server_paths() {
        let url = daemon_proxy_url_from_nhash(
            "http://127.0.0.1:21417",
            "nhash1example",
            "/poster.png",
            use_origin_isolated_loopback_hosts(),
        )
        .unwrap();
        let parsed = tauri::Url::parse(&url).expect("valid URL");
        assert_eq!(parsed.path(), "/htree/nhash1example/poster.png");
        let host = parsed.host_str().expect("loopback host");
        if use_origin_isolated_loopback_hosts() {
            assert!(
                host.ends_with(".htree.localhost"),
                "expected isolated loopback host, got {url}"
            );
        } else {
            assert_eq!(host, "127.0.0.1", "expected plain loopback host, got {url}");
        }
    }

    #[test]
    fn origin_isolated_loopback_hosts_are_stable_per_tree_root() {
        let canonical_root = htree_origin_from_tree_host("npub1example", "video");
        let first = loopback_server_url("http://127.0.0.1:21417", &canonical_root, true).unwrap();
        let second = loopback_server_url("http://127.0.0.1:21417", &canonical_root, true).unwrap();
        let first_host = tauri::Url::parse(&first)
            .expect("valid URL")
            .host_str()
            .expect("first host")
            .to_string();
        let second_host = tauri::Url::parse(&second)
            .expect("valid URL")
            .host_str()
            .expect("second host")
            .to_string();
        assert_eq!(first_host, second_host);
    }

    #[test]
    fn origin_isolated_loopback_hosts_differ_across_tree_roots_and_nhashes() {
        let owner_a = loopback_server_url(
            "http://127.0.0.1:21417",
            &htree_origin_from_tree_host("npub1alice", "video"),
            true,
        )
        .unwrap();
        let owner_b = loopback_server_url(
            "http://127.0.0.1:21417",
            &htree_origin_from_tree_host("npub1bob", "video"),
            true,
        )
        .unwrap();
        let nhash = loopback_server_url(
            "http://127.0.0.1:21417",
            &htree_origin_from_nhash("nhash1example"),
            true,
        )
        .unwrap();
        let owner_a_host = tauri::Url::parse(&owner_a)
            .expect("valid URL")
            .host_str()
            .expect("owner A host")
            .to_string();
        let owner_b_host = tauri::Url::parse(&owner_b)
            .expect("valid URL")
            .host_str()
            .expect("owner B host")
            .to_string();
        let nhash_host = tauri::Url::parse(&nhash)
            .expect("valid nhash URL")
            .host_str()
            .expect("nhash host")
            .to_string();
        assert_ne!(owner_a_host, owner_b_host);
        assert_ne!(owner_a_host, nhash_host);
        assert_ne!(owner_b_host, nhash_host);
    }

    #[test]
    fn daemon_proxy_tree_urls_can_use_plain_loopback_hosts_when_requested() {
        let url = daemon_proxy_url_from_tree_host(
            "http://127.0.0.1:21417",
            "npub1example",
            "video",
            "/index.html",
            false,
        )
        .unwrap();
        let parsed = tauri::Url::parse(&url).expect("valid URL");
        assert_eq!(parsed.host_str(), Some("127.0.0.1"));
        assert_eq!(parsed.path(), "/htree/npub1example/video/index.html");
    }

    #[test]
    fn canonicalized_child_urls_map_back_to_htree_identity() {
        let url = canonicalize_child_webview_url(
            "http://tree-deadbeef.htree.localhost:21417/htree/npub1example/video/index.html?smoke=1&iris_htree_server=http%3A%2F%2F127.0.0.1%3A21417&iris_htree_canonical=htree%3A%2F%2Fnpub1example%2Fvideo%2Findex.html%3Fsmoke%3D1#/feed",
            "http://tree-deadbeef.htree.localhost:21417/htree/npub1example/video",
            "htree://npub1example/video",
        );
        assert_eq!(url, "htree://npub1example/video/index.html?smoke=1#/feed");
    }

    #[test]
    fn canonicalized_child_urls_map_back_from_plain_loopback_transport() {
        let url = canonicalize_child_webview_url(
            "http://127.0.0.1:21417/htree/npub1example/video/index.html?smoke=1&iris_htree_server=http%3A%2F%2F127.0.0.1%3A21417&iris_htree_canonical=htree%3A%2F%2Fnpub1example%2Fvideo%2Findex.html%3Fsmoke%3D1#/feed",
            "http://127.0.0.1:21417/htree/npub1example/video",
            "htree://npub1example/video",
        );
        assert_eq!(url, "htree://npub1example/video/index.html?smoke=1#/feed");
    }

    #[test]
    fn canonicalized_child_urls_map_nhash_transport_back_to_tree_host_identity() {
        let url = canonicalize_child_webview_url(
            "http://127.0.0.1:21417/htree/nhash1example/index.html?smoke=1&iris_htree_server=http%3A%2F%2F127.0.0.1%3A21417&iris_htree_canonical=htree%3A%2F%2Fnpub1example%2Fvideo%2Findex.html%3Fsmoke%3D1#/feed",
            "http://127.0.0.1:21417/htree/nhash1example",
            "htree://npub1example/video",
        );
        assert_eq!(url, "htree://npub1example/video/index.html?smoke=1#/feed");
    }

    #[test]
    fn canonicalized_child_urls_map_virtual_host_root_paths_back_to_tree_identity() {
        let url = canonicalize_child_webview_url(
            "http://tree-deadbeef.htree.localhost:21417/users/npub1cj8znuztfqkvq89pl8hceph0svvvqk0qay6nydgk9uyq7fhpfsgsqwrz4u",
            "http://tree-deadbeef.htree.localhost:21417/htree/nhash1example",
            "htree://nhash1example",
        );
        assert_eq!(
            url,
            "htree://nhash1example/users/npub1cj8znuztfqkvq89pl8hceph0svvvqk0qay6nydgk9uyq7fhpfsgsqwrz4u"
        );
    }

    #[test]
    fn canonicalized_child_urls_strip_internal_query_params_without_removing_user_query() {
        let url = canonicalize_child_webview_url(
            "htree://npub1example/video/index.html?smoke=1&iris_htree_server=http%3A%2F%2F127.0.0.1%3A21417&iris_htree_canonical=htree%3A%2F%2Fnpub1example%2Fvideo%2Findex.html%3Fsmoke%3D1&iris_htree_session=session-token&iris_htree_root=97562a6d",
            "http://127.0.0.1:21417/htree/npub1example/video",
            "htree://npub1example/video",
        );
        assert_eq!(url, "htree://npub1example/video/index.html?smoke=1");
    }

    #[test]
    fn appended_internal_query_params_include_session_token() {
        let url = append_internal_htree_query_params(
            "http://127.0.0.1:21417/htree/nhash1example/index.html?smoke=1",
            "http://127.0.0.1:21417",
            "htree://nhash1example/index.html?smoke=1",
            "session-token",
            Some("deadbeef"),
        )
        .expect("internal query params appended");
        let parsed = tauri::Url::parse(&url).expect("valid URL");
        let params: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(
            params.get("iris_htree_server").map(String::as_str),
            Some("http://127.0.0.1:21417")
        );
        assert_eq!(
            params.get("iris_htree_canonical").map(String::as_str),
            Some("htree://nhash1example/index.html?smoke=1")
        );
        assert_eq!(
            params.get("iris_htree_session").map(String::as_str),
            Some("session-token")
        );
        assert_eq!(
            params.get("iris_htree_root").map(String::as_str),
            Some("deadbeef")
        );
    }

    #[test]
    fn generated_bridge_script_only_reports_top_level_location_and_diagnostics() {
        let script = generate_nip07_script(
            "http://127.0.0.1:21417",
            "session-token",
            "content",
            None,
            None,
            None,
        );

        assert!(
            script.contains("const IS_TOP_LEVEL_DOCUMENT = (() => {"),
            "expected top-level frame detection in bridge script"
        );
        assert_eq!(
            script
                .matches("if (!IS_TOP_LEVEL_DOCUMENT) return;")
                .count(),
            2,
            "expected subframe guard for both location and diagnostic reporting"
        );
    }

    #[test]
    fn generated_bridge_script_exposes_session_token_for_authenticated_relay_use() {
        let script = generate_nip07_script(
            "http://127.0.0.1:21417",
            "session-token",
            "content",
            None,
            None,
            None,
        );

        assert!(
            script.contains("window.__HTREE_SESSION_TOKEN__ = SESSION_TOKEN;"),
            "expected child bridge to expose the session token for authenticated relay use"
        );
    }

    #[test]
    fn generated_bridge_script_prefers_tauri_invoke_for_nip07_requests() {
        let script = generate_nip07_script(
            "http://127.0.0.1:21417",
            "session-token",
            "content",
            Some("https://jumble.social"),
            None,
            None,
        );

        assert!(
            script.contains("const invoke = await getInvoke();"),
            "expected child bridge to check for Tauri invoke before network transports"
        );
        assert!(
            script.contains("await invoke('nip07_request'"),
            "expected child bridge to call nip07_request over Tauri IPC"
        );
        assert!(
            script.contains("Invoke transport unavailable, falling back to fetch bridges"),
            "expected child bridge to log when it has to fall back from Tauri IPC"
        );
    }

    #[test]
    fn self_tree_host_resolves_to_owner_npub_before_loading() {
        assert_eq!(
            resolve_tree_request_host("self", Some("npub1owner")).unwrap(),
            "npub1owner"
        );
    }

    #[test]
    fn self_tree_host_requires_owner_identity() {
        let err =
            resolve_tree_request_host("self", None).expect_err("self should require identity");
        assert!(err.contains("self identity"));
    }

    #[test]
    fn http_urls_use_external_webview_variant() {
        let url = tauri::Url::parse("https://files.iris.to").unwrap();
        assert!(matches!(
            webview_url_for_parsed_url(&url),
            WebviewUrl::External(_)
        ));
    }

    #[test]
    fn custom_scheme_urls_use_custom_protocol_webview_variant() {
        let url = tauri::Url::parse("htree://self/video").unwrap();
        assert!(matches!(
            webview_url_for_parsed_url(&url),
            WebviewUrl::CustomProtocol(_)
        ));
    }

    #[test]
    fn webview_event_http_envelope_accepts_legacy_session_token_key() {
        let envelope: WebviewEventHttpEnvelope = serde_json::from_str(
            r#"{
                "session_token": "legacy-token",
                "payload": {
                    "kind": "location",
                    "label": "content",
                    "origin": "htree://npub1example/git",
                    "url": "htree://npub1example/git/index.html"
                }
            }"#,
        )
        .expect("legacy envelope should deserialize");

        assert_eq!(envelope.session_token, "legacy-token");
        assert_eq!(envelope.payload.kind, "location");
        assert_eq!(envelope.payload.label, "content");
    }

    #[test]
    fn permission_decision_action_uses_camel_case_wire_format() {
        let parsed: Nip07PermissionDecisionAction =
            serde_json::from_value(serde_json::json!("allowSession"))
                .expect("decision should deserialize");
        assert_eq!(parsed, Nip07PermissionDecisionAction::AllowSession);
        assert_eq!(
            serde_json::to_value(Nip07PermissionDecisionAction::BlockSite)
                .expect("decision should serialize"),
            serde_json::json!("blockSite")
        );
    }

    #[cfg(any(target_os = "macos", windows, target_os = "linux"))]
    #[test]
    fn native_permission_dialog_results_map_to_safe_decisions() {
        assert_eq!(
            native_permission_decision_from_results(
                &MessageDialogResult::Custom(NATIVE_PERMISSION_ALLOW_LABEL.to_string()),
                Some(&MessageDialogResult::Custom(
                    NATIVE_PERMISSION_ALWAYS_ALLOW_LABEL.to_string()
                )),
            ),
            Nip07PermissionDecisionAction::AllowAlways
        );
        assert_eq!(
            native_permission_decision_from_results(
                &MessageDialogResult::Custom(NATIVE_PERMISSION_ALLOW_LABEL.to_string()),
                Some(&MessageDialogResult::Cancel),
            ),
            Nip07PermissionDecisionAction::AllowSession
        );
        assert_eq!(
            native_permission_decision_from_results(
                &MessageDialogResult::Cancel,
                Some(&MessageDialogResult::Custom(
                    NATIVE_PERMISSION_BLOCK_SITE_LABEL.to_string()
                )),
            ),
            Nip07PermissionDecisionAction::BlockSite
        );
        assert_eq!(
            native_permission_decision_from_results(
                &MessageDialogResult::Custom(NATIVE_PERMISSION_DENY_LABEL.to_string()),
                Some(&MessageDialogResult::Cancel),
            ),
            Nip07PermissionDecisionAction::Deny
        );
    }

    #[tokio::test]
    async fn get_public_key_returns_the_signed_in_account() {
        let (_temp_dir, state) = test_nip07_state();
        let account = state
            .login_with_secret(TEST_SECRET_HEX)
            .expect("login succeeds");

        let response = handle_nip07_request_inner(
            Some(&state),
            "getPublicKey",
            &serde_json::json!({}),
            "tauri://localhost",
        )
        .await;

        assert_eq!(response.error, None);
        assert_eq!(
            response.result,
            Some(serde_json::Value::String(account.pubkey))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sync_protocol_bridge_handles_requests_inside_tokio_runtime() {
        let (_temp_dir, state) = test_nip07_state();
        let account = state
            .login_with_secret(TEST_SECRET_HEX)
            .expect("login succeeds");

        let response = handle_nip07_request_sync(
            Some(Arc::new(state)),
            Nip07Request {
                method: "getPublicKey".to_string(),
                params: serde_json::json!({}),
                origin: "tauri://localhost".to_string(),
            },
        );

        assert_eq!(response.error, None);
        assert_eq!(
            response.result,
            Some(serde_json::Value::String(account.pubkey))
        );
    }

    #[tokio::test]
    async fn sign_event_returns_a_verified_event_and_grants_sign_permission() {
        let (_temp_dir, state) = test_nip07_state();
        let account = state
            .login_with_secret(TEST_SECRET_HEX)
            .expect("login succeeds");

        let response = handle_nip07_request_inner(
            Some(&state),
            "signEvent",
            &serde_json::json!({
                "event": {
                    "created_at": 1_711_111_111,
                    "kind": 1,
                    "tags": [["t", "iris"]],
                    "content": "hello from Rust"
                }
            }),
            "tauri://localhost",
        )
        .await;

        assert_eq!(response.error, None);
        let event: nostr_sdk::Event =
            serde_json::from_value(response.result.expect("signed event payload"))
                .expect("signed event parses");
        assert_eq!(event.pubkey.to_hex(), account.pubkey);
        event.verify().expect("event verifies");
    }

    #[tokio::test]
    async fn nip04_encrypt_and_decrypt_round_trip_between_local_accounts() {
        let (_sender_dir, sender_state) = test_nip07_state();
        let sender = sender_state
            .login_with_secret(TEST_SECRET_HEX)
            .expect("sender login succeeds");
        let receiver_keys = Keys::parse(SECOND_TEST_SECRET_HEX).expect("receiver keys parse");
        let plaintext = "hello from nip04";

        let encrypted = handle_nip07_request_inner(
            Some(&sender_state),
            "nip04.encrypt",
            &serde_json::json!({
                "pubkey": receiver_keys.public_key().to_hex(),
                "plaintext": plaintext
            }),
            "tauri://localhost",
        )
        .await;
        assert_eq!(encrypted.error, None);
        let ciphertext = encrypted
            .result
            .expect("ciphertext payload")
            .as_str()
            .expect("ciphertext string")
            .to_string();
        assert_ne!(ciphertext, plaintext);

        let (_receiver_dir, receiver_state) = test_nip07_state();
        receiver_state
            .login_with_secret(SECOND_TEST_SECRET_HEX)
            .expect("receiver login succeeds");
        let decrypted = handle_nip07_request_inner(
            Some(&receiver_state),
            "nip04.decrypt",
            &serde_json::json!({
                "pubkey": sender.pubkey,
                "ciphertext": ciphertext
            }),
            "tauri://localhost",
        )
        .await;
        assert_eq!(decrypted.error, None);
        assert_eq!(
            decrypted.result,
            Some(serde_json::Value::String(plaintext.to_string()))
        );
    }

    #[tokio::test]
    async fn nip44_encrypt_and_decrypt_round_trip_between_local_accounts() {
        let (_sender_dir, sender_state) = test_nip07_state();
        let sender = sender_state
            .login_with_secret(TEST_SECRET_HEX)
            .expect("sender login succeeds");
        let receiver_keys = Keys::parse(SECOND_TEST_SECRET_HEX).expect("receiver keys parse");
        let plaintext = "hello from nip44";

        let encrypted = handle_nip07_request_inner(
            Some(&sender_state),
            "nip44.encrypt",
            &serde_json::json!({
                "pubkey": receiver_keys.public_key().to_hex(),
                "plaintext": plaintext
            }),
            "tauri://localhost",
        )
        .await;
        assert_eq!(encrypted.error, None);
        let ciphertext = encrypted
            .result
            .expect("ciphertext payload")
            .as_str()
            .expect("ciphertext string")
            .to_string();
        assert_ne!(ciphertext, plaintext);

        let (_receiver_dir, receiver_state) = test_nip07_state();
        receiver_state
            .login_with_secret(SECOND_TEST_SECRET_HEX)
            .expect("receiver login succeeds");
        let decrypted = handle_nip07_request_inner(
            Some(&receiver_state),
            "nip44.decrypt",
            &serde_json::json!({
                "pubkey": sender.pubkey,
                "ciphertext": ciphertext
            }),
            "tauri://localhost",
        )
        .await;
        assert_eq!(decrypted.error, None);
        assert_eq!(
            decrypted.result,
            Some(serde_json::Value::String(plaintext.to_string()))
        );
    }

    async fn wait_for_permission_prompt(state: &Nip07State) -> Nip07PermissionPrompt {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(prompt) = state.take_permission_prompt().await {
                    return prompt;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("permission prompt should arrive")
    }

    #[tokio::test]
    async fn external_sites_can_be_allowed_or_blocked_per_prompt() {
        let temp_dir = tempdir().expect("tempdir");
        let storage_path = temp_dir.path().join("nip07-account.json");
        let secret_store = Arc::new(MemoryConfidentialStore::default());
        let state = Arc::new(Nip07State::new(
            Arc::new(PermissionStore::new(None)),
            storage_path,
            secret_store,
        ));
        let account = state
            .login_with_secret(TEST_SECRET_HEX)
            .expect("login succeeds");
        let allowed_origin = "https://jumble.social";

        let state_for_request = state.clone();
        let allowed_request = tokio::spawn(async move {
            handle_nip07_request_inner(
                Some(state_for_request.as_ref()),
                "getPublicKey",
                &serde_json::json!({}),
                allowed_origin,
            )
            .await
        });
        let prompt = wait_for_permission_prompt(state.as_ref()).await;
        assert_eq!(prompt.origin, allowed_origin);
        assert_eq!(prompt.method, "getPublicKey");
        state
            .resolve_permission_prompt(
                &prompt.request_id,
                Nip07PermissionDecisionAction::AllowAlways,
            )
            .await
            .expect("prompt resolution succeeds");

        let allowed_response = allowed_request.await.expect("request task completes");
        assert_eq!(
            allowed_response.result,
            Some(serde_json::Value::String(account.pubkey.clone()))
        );
        assert_eq!(allowed_response.error, None);

        let remembered_response = handle_nip07_request_inner(
            Some(state.as_ref()),
            "getPublicKey",
            &serde_json::json!({}),
            allowed_origin,
        )
        .await;
        assert_eq!(
            remembered_response.result,
            Some(serde_json::Value::String(account.pubkey))
        );
        assert_eq!(remembered_response.error, None);
        assert!(state.take_permission_prompt().await.is_none());

        let blocked_origin = "https://spam.example";
        let state_for_block = state.clone();
        let blocked_request = tokio::spawn(async move {
            handle_nip07_request_inner(
                Some(state_for_block.as_ref()),
                "getPublicKey",
                &serde_json::json!({}),
                blocked_origin,
            )
            .await
        });
        let blocked_prompt = wait_for_permission_prompt(state.as_ref()).await;
        assert_eq!(blocked_prompt.origin, blocked_origin);
        state
            .resolve_permission_prompt(
                &blocked_prompt.request_id,
                Nip07PermissionDecisionAction::BlockSite,
            )
            .await
            .expect("block resolution succeeds");

        let blocked_response = blocked_request.await.expect("blocked request completes");
        assert_eq!(blocked_response.result, None);
        assert_eq!(
            blocked_response.error,
            Some("Permission denied".to_string())
        );

        let blocked_repeat = handle_nip07_request_inner(
            Some(state.as_ref()),
            "getPublicKey",
            &serde_json::json!({}),
            blocked_origin,
        )
        .await;
        assert_eq!(blocked_repeat.result, None);
        assert_eq!(blocked_repeat.error, Some("Permission denied".to_string()));
        assert!(state.take_permission_prompt().await.is_none());
    }

    #[test]
    fn account_storage_round_trips_across_state_reloads() {
        let temp_dir = tempdir().expect("tempdir");
        let storage_path = temp_dir.path().join("nip07-account.json");
        let secret_store = Arc::new(MemoryConfidentialStore::default());

        let initial_state = Nip07State::new(
            Arc::new(PermissionStore::new(None)),
            storage_path.clone(),
            secret_store.clone(),
        );
        let expected = initial_state
            .login_with_secret(TEST_SECRET_HEX)
            .expect("login succeeds");
        let second = initial_state
            .login_with_secret(SECOND_TEST_SECRET_HEX)
            .expect("second login succeeds");
        initial_state
            .set_active_account(expected.pubkey.clone())
            .expect("active account switches");

        let reloaded_state = Nip07State::new(
            Arc::new(PermissionStore::new(None)),
            storage_path.clone(),
            secret_store.clone(),
        );
        let reloaded_accounts = reloaded_state.list_accounts().expect("accounts load");
        assert_eq!(reloaded_accounts.accounts.len(), 2);
        assert_eq!(
            reloaded_accounts.active_pubkey,
            Some(expected.pubkey.clone())
        );
        assert!(reloaded_accounts
            .accounts
            .iter()
            .any(|account| account.pubkey == second.pubkey));
        assert_eq!(
            reloaded_state.current_account().expect("account loads"),
            Some(expected)
        );

        let metadata = std::fs::read_to_string(storage_path).expect("metadata file is readable");
        assert!(!metadata.contains("secret_key"));
        assert!(!metadata.contains("nsec1"));
        assert!(metadata.contains(&second.pubkey));
    }

    #[test]
    fn session_tokens_can_be_validated_without_origin_round_trip() {
        let (_temp_dir, state) = test_nip07_state();

        let token = state.new_session("htree://npub1example/iris-client");

        assert!(state.validate_any_token(&token));
        assert!(!state.validate_any_token("not-a-real-token"));
    }

    #[test]
    fn exporting_account_secret_returns_bech32_nsec() {
        let (_temp_dir, state) = test_nip07_state();
        let account = state
            .login_with_secret(TEST_SECRET_HEX)
            .expect("login succeeds");
        let expected_nsec = Keys::parse(TEST_SECRET_HEX)
            .expect("test keys parse")
            .secret_key()
            .to_bech32()
            .expect("secret encodes");

        let exported = state
            .export_account_secret(account.pubkey)
            .expect("secret exports");

        assert_eq!(exported, expected_nsec);
    }

    #[test]
    fn legacy_plaintext_account_storage_is_rejected() {
        let temp_dir = tempdir().expect("tempdir");
        let storage_path = temp_dir.path().join("nip07-account.json");
        let first_keys = Keys::parse(TEST_SECRET_HEX).expect("first keys parse");
        let second_keys = Keys::parse(SECOND_TEST_SECRET_HEX).expect("second keys parse");
        let second_pubkey = second_keys.public_key().to_hex();
        let first_nsec = first_keys
            .secret_key()
            .to_bech32()
            .expect("first secret encodes");
        let second_nsec = second_keys
            .secret_key()
            .to_bech32()
            .expect("second secret encodes");

        let legacy = serde_json::json!({
            "accounts": [
                {
                    "secret_key": first_nsec.clone(),
                    "added_at": 11_u64
                },
                {
                    "secret_key": second_nsec.clone(),
                    "added_at": 22_u64
                }
            ],
            "activePubkey": second_pubkey.clone()
        });
        std::fs::write(
            &storage_path,
            serde_json::to_vec_pretty(&legacy).expect("legacy storage serializes"),
        )
        .expect("legacy storage writes");

        let error = load_nip07_accounts(&storage_path).expect_err("legacy plaintext should fail");
        assert!(error.contains("Failed to parse saved account metadata"));
    }

    #[test]
    fn metadata_reload_does_not_touch_confidential_storage_until_secret_use() {
        let temp_dir = tempdir().expect("tempdir");
        let storage_path = temp_dir.path().join("nip07-account.json");
        let pubkey = Keys::parse(TEST_SECRET_HEX)
            .expect("test keys parse")
            .public_key()
            .to_hex();
        let metadata = serde_json::json!({
            "accounts": [
                {
                    "pubkey": pubkey.clone(),
                    "added_at": 7_u64
                }
            ],
            "activePubkey": pubkey.clone()
        });
        std::fs::write(
            &storage_path,
            serde_json::to_vec_pretty(&metadata).expect("metadata serializes"),
        )
        .expect("metadata writes");

        let state = Nip07State::new(
            Arc::new(PermissionStore::new(None)),
            storage_path.clone(),
            Arc::new(MemoryConfidentialStore::default()),
        );

        let accounts = state.list_accounts().expect("accounts load");
        assert_eq!(accounts.accounts.len(), 1);
        assert_eq!(accounts.active_pubkey, Some(pubkey.clone()));
        assert_eq!(
            state.current_account().expect("current account loads"),
            Some(Nip07AccountSummary {
                pubkey: pubkey.clone(),
                npub: PublicKey::parse(&pubkey)
                    .expect("pubkey parses")
                    .to_bech32()
                    .expect("npub encodes"),
                added_at: 7_u64,
            })
        );
    }

    #[test]
    fn loading_account_lists_does_not_read_confidential_blob() {
        let temp_dir = tempdir().expect("tempdir");
        let storage_path = temp_dir.path().join("nip07-account.json");
        let pubkey = Keys::parse(TEST_SECRET_HEX)
            .expect("test keys parse")
            .public_key()
            .to_hex();
        let metadata = serde_json::json!({
            "accounts": [
                {
                    "pubkey": pubkey.clone(),
                    "added_at": 7_u64
                }
            ],
            "activePubkey": pubkey
        });
        std::fs::write(
            &storage_path,
            serde_json::to_vec_pretty(&metadata).expect("metadata serializes"),
        )
        .expect("metadata writes");
        let secret_store = Arc::new(MemoryConfidentialStore::default());

        let state = Nip07State::new(
            Arc::new(PermissionStore::new(None)),
            storage_path,
            secret_store.clone(),
        );

        assert_eq!(secret_store.blob_load_count(), 0);
        let _ = state.list_accounts().expect("accounts list");
        let _ = state.current_account().expect("current account");
        assert_eq!(secret_store.blob_load_count(), 0);
    }

    #[test]
    fn missing_confidential_secret_is_reported_when_signer_is_needed() {
        let temp_dir = tempdir().expect("tempdir");
        let storage_path = temp_dir.path().join("nip07-account.json");
        let keys = Keys::parse(TEST_SECRET_HEX).expect("keys parse");
        let pubkey = keys.public_key().to_hex();
        let metadata = serde_json::json!({
            "accounts": [
                {
                    "pubkey": pubkey.clone(),
                    "added_at": 7_u64
                }
            ],
            "activePubkey": pubkey.clone()
        });
        std::fs::write(
            &storage_path,
            serde_json::to_vec_pretty(&metadata).expect("metadata serializes"),
        )
        .expect("metadata writes");
        let secret_store = Arc::new(MemoryConfidentialStore::default());

        let state = Nip07State::new(
            Arc::new(PermissionStore::new(None)),
            storage_path,
            secret_store.clone(),
        );

        assert_eq!(secret_store.stored_secret(&pubkey), None);
        let error = state.signer_keys().expect_err("missing secret should fail");
        assert!(error.contains("Secure Nostr secret missing"));
    }

    #[test]
    fn switching_existing_accounts_keeps_one_entry_per_pubkey_and_refreshes_active_signer() {
        let (_temp_dir, state) = test_nip07_state();
        let first = state
            .login_with_secret(TEST_SECRET_HEX)
            .expect("first login succeeds");
        let second = state
            .login_with_secret(SECOND_TEST_SECRET_HEX)
            .expect("second login succeeds");

        let after_two_accounts = state.list_accounts().expect("accounts list");
        assert_eq!(after_two_accounts.accounts.len(), 2);
        assert_eq!(
            after_two_accounts.active_pubkey,
            Some(second.pubkey.clone())
        );

        let switched = state
            .set_active_account(first.pubkey.clone())
            .expect("switch succeeds");
        assert_eq!(switched.pubkey, first.pubkey);

        let after_switch = state.list_accounts().expect("accounts list");
        assert_eq!(after_switch.accounts.len(), 2);
        assert_eq!(after_switch.active_pubkey, Some(first.pubkey.clone()));

        let duplicate_login = state
            .login_with_secret(TEST_SECRET_HEX)
            .expect("duplicate login succeeds");
        assert_eq!(duplicate_login.pubkey, first.pubkey);

        let after_duplicate_login = state.list_accounts().expect("accounts list");
        assert_eq!(after_duplicate_login.accounts.len(), 2);
        assert_eq!(
            after_duplicate_login.active_pubkey,
            Some(first.pubkey.clone())
        );

        let after_remove = state
            .remove_account(first.pubkey.clone())
            .expect("remove succeeds");
        assert_eq!(after_remove.accounts.len(), 1);
        assert_eq!(after_remove.active_pubkey, Some(second.pubkey.clone()));
        assert_eq!(after_remove.accounts[0].pubkey, second.pubkey);
    }
}
