//! Iris - Thin native shell with embedded htree daemon
//!
//! This is the native desktop app that:
//! 1. Starts an embedded htree daemon (content storage, P2P, Nostr relay)
//! 2. Opens a webview pointing to iris-files web app
//! 3. Injects window.__HTREE_SERVER_URL__ so the web app can use the daemon
//! 4. Provides htree:// URI scheme for child webviews
//! 5. Manages NIP-07 permissions for child webviews
#![cfg_attr(any(target_os = "android", target_os = "ios"), allow(dead_code))]

pub mod automation;
pub mod backend_routes;
pub mod history;
pub mod htree_protocol;
pub mod mobile_bluetooth;
pub mod nip07;
pub mod permissions;
pub mod pwa;
pub mod relay_proxy;

use axum::body::Bytes;
use axum::http::HeaderMap;
use axum::routing::{any, get, post};
use axum::Router;
use hashtree_cli::daemon::{EmbeddedDaemonInfo, EmbeddedDaemonOptions};
use hashtree_cli::server::AppState;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Once;
#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
use std::time::Duration;
#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
use tauri::menu::{Menu, MenuBuilder, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder};
#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_deep_link::DeepLinkExt;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;
use tracing_subscriber::EnvFilter;

static RUSTLS_PROVIDER_INIT: Once = Once::new();
const TRAY_ICON_ID: &str = "main";
const TRAY_OPEN_MENU_ID: &str = "tray_open_main";
const TRAY_HOME_MENU_ID: &str = "tray_home";
const TRAY_SETTINGS_MENU_ID: &str = "tray_settings";
const TRAY_QUIT_MENU_ID: &str = "tray_quit";
const CHILD_WEBVIEW_LABEL: &str = "content";
const TOGGLE_CHILD_WEBVIEW_DEVTOOLS_MENU_ID: &str = "view_toggle_child_webview_devtools";

#[derive(Debug, Clone, PartialEq, Eq)]
struct IrisPaths {
    shell_data_dir: PathBuf,
    htree_config_dir: PathBuf,
    htree_data_dir: PathBuf,
}

#[derive(Default)]
struct DeepLinkState {
    frontend_ready: RwLock<bool>,
    pending_urls: RwLock<Vec<String>>,
}

impl DeepLinkState {
    fn new() -> Self {
        Self::default()
    }

    fn is_frontend_ready(&self) -> bool {
        *self.frontend_ready.read()
    }

    fn queue_urls<I>(&self, urls: I)
    where
        I: IntoIterator<Item = String>,
    {
        self.pending_urls.write().extend(urls);
    }

    fn mark_frontend_ready(&self) -> Vec<String> {
        *self.frontend_ready.write() = true;
        std::mem::take(&mut *self.pending_urls.write())
    }
}

const DEFAULT_MULTICAST_TOGGLE_MAX_PEERS: usize = 12;
const DEFAULT_BLUETOOTH_TOGGLE_MAX_PEERS: usize = 6;
const IRIS_BLUETOOTH_DEFAULTS_MARKER_FILE: &str = ".iris-bluetooth-defaults-v3";
const IRIS_BLUETOOTH_DEFAULTS_MARKER_VERSION: &str = "v3\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BluetoothDefaultPlatform {
    Android,
    Ios,
    MacOs,
    Other,
}

fn current_bluetooth_default_platform() -> BluetoothDefaultPlatform {
    if cfg!(target_os = "android") {
        BluetoothDefaultPlatform::Android
    } else if cfg!(target_os = "ios") {
        BluetoothDefaultPlatform::Ios
    } else if cfg!(target_os = "macos") {
        BluetoothDefaultPlatform::MacOs
    } else {
        BluetoothDefaultPlatform::Other
    }
}

fn bluetooth_enabled_by_default_for_platform(platform: BluetoothDefaultPlatform) -> bool {
    matches!(
        platform,
        BluetoothDefaultPlatform::Android
            | BluetoothDefaultPlatform::Ios
            | BluetoothDefaultPlatform::MacOs
    )
}

fn apply_iris_transport_defaults(
    config: &mut hashtree_cli::Config,
    platform: BluetoothDefaultPlatform,
) -> bool {
    let mut changed = false;

    if !config.server.enable_multicast && config.server.max_multicast_peers == 0 {
        config.server.enable_multicast = true;
        config.server.max_multicast_peers = DEFAULT_MULTICAST_TOGGLE_MAX_PEERS;
        changed = true;
    }

    if bluetooth_enabled_by_default_for_platform(platform)
        && !config.server.enable_bluetooth
        && config.server.max_bluetooth_peers == 0
    {
        config.server.enable_bluetooth = true;
        config.server.max_bluetooth_peers = DEFAULT_BLUETOOTH_TOGGLE_MAX_PEERS;
        changed = true;
    }

    changed
}

fn ensure_iris_default_network_config(paths: &IrisPaths) -> Result<(), String> {
    ensure_iris_default_network_config_for_platform(paths, current_bluetooth_default_platform())
}

fn ensure_iris_default_network_config_for_platform(
    paths: &IrisPaths,
    platform: BluetoothDefaultPlatform,
) -> Result<(), String> {
    let marker_path = paths
        .shell_data_dir
        .join(IRIS_BLUETOOTH_DEFAULTS_MARKER_FILE);
    if marker_path.exists() {
        return Ok(());
    }

    let mut config = hashtree_cli::Config::load()
        .map_err(|error| format!("Failed to load config for Iris defaults: {}", error))?;
    if apply_iris_transport_defaults(&mut config, platform) {
        config
            .save()
            .map_err(|error| format!("Failed to save Iris transport defaults: {}", error))?;
    }

    std::fs::write(&marker_path, IRIS_BLUETOOTH_DEFAULTS_MARKER_VERSION)
        .map_err(|error| format!("Failed to persist Iris defaults marker: {}", error))?;
    Ok(())
}

#[derive(Default)]
struct DaemonRuntimeState {
    peer_router_controller: RwLock<Option<Arc<hashtree_cli::daemon::EmbeddedPeerRouterController>>>,
    background_services_controller:
        RwLock<Option<Arc<hashtree_cli::daemon::EmbeddedBackgroundServicesController>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct DaemonTransportSettings {
    webrtc: bool,
    multicast: bool,
    bluetooth: bool,
    max_multicast_peers: usize,
    max_bluetooth_peers: usize,
}

impl DaemonTransportSettings {
    fn from_config(config: &hashtree_cli::Config) -> Self {
        Self {
            webrtc: config.server.enable_webrtc,
            multicast: config.server.enable_multicast && config.server.max_multicast_peers > 0,
            bluetooth: config.server.enable_bluetooth && config.server.max_bluetooth_peers > 0,
            max_multicast_peers: config.server.max_multicast_peers,
            max_bluetooth_peers: config.server.max_bluetooth_peers,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct DaemonBlossomServerSettings {
    url: String,
    read: bool,
    write: bool,
}

impl DaemonBlossomServerSettings {
    fn merge_into(
        servers: &mut Vec<DaemonBlossomServerSettings>,
        url: &str,
        read: bool,
        write: bool,
    ) {
        let trimmed = url.trim();
        if trimmed.is_empty() {
            return;
        }

        if let Some(existing) = servers.iter_mut().find(|server| server.url == trimmed) {
            existing.read |= read;
            existing.write |= write;
            return;
        }

        servers.push(Self {
            url: trimmed.to_string(),
            read,
            write,
        });
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct DaemonNetworkSettings {
    webrtc: bool,
    multicast: bool,
    bluetooth: bool,
    nostr_relays_enabled: bool,
    blossom_enabled: bool,
    max_multicast_peers: usize,
    max_bluetooth_peers: usize,
    multicast_group: String,
    multicast_port: u16,
    relay_urls: Vec<String>,
    blossom_servers: Vec<DaemonBlossomServerSettings>,
}

impl DaemonNetworkSettings {
    fn from_config(config: &hashtree_cli::Config) -> Self {
        let mut blossom_servers = Vec::new();
        for url in &config.blossom.servers {
            DaemonBlossomServerSettings::merge_into(&mut blossom_servers, url, true, true);
        }
        for url in &config.blossom.read_servers {
            DaemonBlossomServerSettings::merge_into(&mut blossom_servers, url, true, false);
        }
        for url in &config.blossom.write_servers {
            DaemonBlossomServerSettings::merge_into(&mut blossom_servers, url, false, true);
        }

        Self {
            webrtc: config.server.enable_webrtc,
            multicast: config.server.enable_multicast && config.server.max_multicast_peers > 0,
            bluetooth: config.server.enable_bluetooth && config.server.max_bluetooth_peers > 0,
            nostr_relays_enabled: config.nostr.enabled,
            blossom_enabled: config.blossom.enabled,
            max_multicast_peers: config.server.max_multicast_peers,
            max_bluetooth_peers: config.server.max_bluetooth_peers,
            multicast_group: config.server.multicast_group.clone(),
            multicast_port: config.server.multicast_port,
            relay_urls: config.nostr.relays.clone(),
            blossom_servers,
        }
    }
}

impl From<&DaemonNetworkSettings> for DaemonTransportSettings {
    fn from(settings: &DaemonNetworkSettings) -> Self {
        Self {
            webrtc: settings.webrtc,
            multicast: settings.multicast,
            bluetooth: settings.bluetooth,
            max_multicast_peers: settings.max_multicast_peers,
            max_bluetooth_peers: settings.max_bluetooth_peers,
        }
    }
}

fn apply_transport_settings(
    config: &mut hashtree_cli::Config,
    settings: &DaemonTransportSettings,
) -> DaemonTransportSettings {
    config.server.enable_webrtc = settings.webrtc;
    config.server.enable_multicast = settings.multicast;
    config.server.max_multicast_peers = if settings.multicast {
        if settings.max_multicast_peers > 0 {
            settings.max_multicast_peers
        } else {
            DEFAULT_MULTICAST_TOGGLE_MAX_PEERS
        }
    } else {
        0
    };
    config.server.enable_bluetooth = settings.bluetooth;
    config.server.max_bluetooth_peers = if settings.bluetooth {
        if settings.max_bluetooth_peers > 0 {
            settings.max_bluetooth_peers
        } else {
            DEFAULT_BLUETOOTH_TOGGLE_MAX_PEERS
        }
    } else {
        0
    };

    DaemonTransportSettings::from_config(config)
}

fn normalize_relay_urls(urls: &[String]) -> Result<Vec<String>, String> {
    let mut normalized = Vec::new();

    for url in urls {
        let trimmed = url.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parsed =
            tauri::Url::parse(trimmed).map_err(|_| format!("Invalid relay URL: {}", trimmed))?;
        match parsed.scheme() {
            "ws" | "wss" => {}
            _ => return Err(format!("Relay URL must use ws:// or wss://: {}", trimmed)),
        }
        if !normalized.iter().any(|existing| existing == trimmed) {
            normalized.push(trimmed.to_string());
        }
    }

    Ok(normalized)
}

fn normalize_blossom_servers(
    servers: &[DaemonBlossomServerSettings],
) -> Result<Vec<DaemonBlossomServerSettings>, String> {
    let mut normalized: Vec<DaemonBlossomServerSettings> = Vec::new();

    for server in servers {
        let trimmed = server.url.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parsed = tauri::Url::parse(trimmed)
            .map_err(|_| format!("Invalid Blossom server URL: {}", trimmed))?;
        match parsed.scheme() {
            "http" | "https" => {}
            _ => {
                return Err(format!(
                    "Blossom server URL must use http:// or https://: {}",
                    trimmed
                ))
            }
        }
        if let Some(existing) = normalized
            .iter_mut()
            .find(|existing| existing.url == trimmed)
        {
            existing.read |= server.read;
            existing.write |= server.write;
            continue;
        }
        normalized.push(DaemonBlossomServerSettings {
            url: trimmed.to_string(),
            read: server.read,
            write: server.write,
        });
    }

    normalized.retain(|server| server.read || server.write);
    Ok(normalized)
}

fn apply_network_settings(
    config: &mut hashtree_cli::Config,
    settings: &DaemonNetworkSettings,
) -> Result<DaemonNetworkSettings, String> {
    let normalized_relays = normalize_relay_urls(&settings.relay_urls)?;
    let normalized_blossom = normalize_blossom_servers(&settings.blossom_servers)?;

    let transport_settings =
        apply_transport_settings(config, &DaemonTransportSettings::from(settings));
    config.server.multicast_group = settings.multicast_group.trim().to_string();
    config.server.multicast_port = settings.multicast_port;
    config.nostr.enabled = settings.nostr_relays_enabled;
    config.nostr.relays = normalized_relays;
    config.blossom.enabled = settings.blossom_enabled;
    config.blossom.servers.clear();
    config.blossom.read_servers.clear();
    config.blossom.write_servers.clear();

    for server in normalized_blossom {
        if server.read && server.write {
            config.blossom.servers.push(server.url);
        } else if server.read {
            config.blossom.read_servers.push(server.url);
        } else if server.write {
            config.blossom.write_servers.push(server.url);
        }
    }

    let mut applied = DaemonNetworkSettings::from_config(config);
    applied.webrtc = transport_settings.webrtc;
    applied.multicast = transport_settings.multicast;
    applied.bluetooth = transport_settings.bluetooth;
    applied.max_multicast_peers = transport_settings.max_multicast_peers;
    applied.max_bluetooth_peers = transport_settings.max_bluetooth_peers;
    Ok(applied)
}

pub fn ensure_rustls_provider() {
    RUSTLS_PROVIDER_INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn daemon_bind_address() -> String {
    if let Ok(bind) = std::env::var("IRIS_DAEMON_BIND") {
        let trimmed = bind.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    if let Ok(port) = std::env::var("IRIS_DAEMON_PORT") {
        let trimmed = port.trim();
        if !trimmed.is_empty() {
            return format!("127.0.0.1:{}", trimmed);
        }
    }

    "127.0.0.1:21417".to_string()
}

fn env_path(var: &str) -> Option<PathBuf> {
    let value = std::env::var(var).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

#[cfg(any(target_os = "android", target_os = "ios", test))]
fn mobile_default_htree_paths(shell_data_dir: &Path) -> (PathBuf, PathBuf) {
    let config_dir = shell_data_dir.join("hashtree");
    let data_dir = config_dir.join("data");
    (config_dir, data_dir)
}

#[cfg(any(target_os = "android", target_os = "ios"))]
fn default_htree_paths(shell_data_dir: &Path) -> (PathBuf, PathBuf) {
    mobile_default_htree_paths(shell_data_dir)
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn default_htree_paths(_shell_data_dir: &Path) -> (PathBuf, PathBuf) {
    let config_dir = hashtree_cli::config::get_hashtree_dir();
    let data_dir = PathBuf::from(
        hashtree_cli::Config::load()
            .unwrap_or_default()
            .storage
            .data_dir,
    );
    (config_dir, data_dir)
}

fn resolve_iris_paths(
    shell_data_dir: PathBuf,
    env_config_dir: Option<PathBuf>,
    env_data_dir: Option<PathBuf>,
    shared_config_dir: PathBuf,
    shared_data_dir: PathBuf,
) -> IrisPaths {
    IrisPaths {
        shell_data_dir,
        htree_config_dir: env_config_dir.unwrap_or(shared_config_dir),
        htree_data_dir: env_data_dir.unwrap_or(shared_data_dir),
    }
}

#[cfg(any(target_os = "android", target_os = "ios"))]
fn ensure_mobile_peer_id() -> Result<String, String> {
    let (keys, _) = hashtree_cli::config::ensure_keys()
        .map_err(|error| format!("Failed to load keys for mobile Bluetooth: {}", error))?;
    Ok(keys.public_key().to_hex())
}

/// Start the embedded htree daemon
async fn start_daemon<R: tauri::Runtime + 'static>(
    app: AppHandle<R>,
    data_dir: PathBuf,
) -> Result<EmbeddedDaemonInfo, String> {
    relay_proxy::init_relay_proxy_state();

    let bind_address = daemon_bind_address();
    let config_path = hashtree_cli::config::get_config_path();
    let mut config =
        hashtree_cli::Config::load().map_err(|e| format!("Failed to load config: {}", e))?;
    info!(
        "Embedded daemon config loaded from {:?}: webrtc={} multicast={} (max {}) bluetooth={} (max {}) relays={} blossom_read_servers={}",
        config_path,
        config.server.enable_webrtc,
        config.server.enable_multicast,
        config.server.max_multicast_peers,
        config.server.enable_bluetooth,
        config.server.max_bluetooth_peers,
        config.nostr.active_relays().len(),
        config.blossom.all_read_servers().len(),
    );
    config.storage.data_dir = data_dir.to_string_lossy().to_string();
    config.server.bind_address = bind_address.clone();
    config.server.enable_auth = false;
    config.server.stun_port = 0;

    // Add extra routes for relay proxy and NIP-07
    let app_for_webview_bridge = app.clone();
    let app_for_authenticated_relay = app.clone();
    let app_for_authenticated_relay_slash = app.clone();
    let extra_routes =
        Router::<AppState>::new()
            .merge(backend_routes::router())
            .route("/relay", any(relay_proxy::handle_relay_websocket))
            .route(
                "/__iris_relay",
                get(move |state, query, ws| {
                    let app = app_for_authenticated_relay.clone();
                    async move {
                        nip07::handle_authenticated_relay_websocket(app, state, query, ws).await
                    }
                }),
            )
            .route(
                "/__iris_relay/",
                get(move |state, query, ws| {
                    let app = app_for_authenticated_relay_slash.clone();
                    async move {
                        nip07::handle_authenticated_relay_websocket(app, state, query, ws).await
                    }
                }),
            )
            .route(
                "/__iris_nip07",
                post(|body: Bytes| async move { nip07::handle_nip07_http_bridge(body).await }),
            )
            .route(
                "/__iris_webview",
                post(move |headers: HeaderMap, body: Bytes| {
                    let app = app_for_webview_bridge.clone();
                    async move { nip07::handle_webview_event_http_bridge(app, headers, body).await }
                }),
            );

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers([
            axum::http::header::ACCEPT_RANGES,
            axum::http::header::CONTENT_RANGE,
            axum::http::header::CONTENT_LENGTH,
            axum::http::header::CONTENT_TYPE,
        ]);

    let info = hashtree_cli::daemon::start_embedded(EmbeddedDaemonOptions {
        config,
        data_dir,
        bind_address,
        relays: None,
        extra_routes: Some(extra_routes),
        cors: Some(cors),
    })
    .await
    .map_err(|e| format!("Failed to start daemon: {}", e))?;

    Ok(info)
}

// ============================================
// Menu construction
// ============================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrayConnectionStatus {
    Starting,
    Running { connected_peers: Option<usize> },
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
enum DesktopPlatform {
    MacOs,
    Windows,
    Linux,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrayPrimaryClickAction {
    ShowMenu,
    OpenWindow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TrayMenuItemSpec {
    Text {
        id: Option<String>,
        text: String,
        enabled: bool,
    },
    Separator,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TrayRuntimeState {
    connection_status: TrayConnectionStatus,
}

impl Default for TrayRuntimeState {
    fn default() -> Self {
        Self {
            connection_status: TrayConnectionStatus::Starting,
        }
    }
}

struct TrayState {
    runtime: RwLock<TrayRuntimeState>,
}

impl TrayState {
    fn new() -> Self {
        Self {
            runtime: RwLock::new(TrayRuntimeState::default()),
        }
    }

    fn snapshot(&self) -> TrayRuntimeState {
        self.runtime.read().clone()
    }

    fn set_connection_status(&self, connection_status: TrayConnectionStatus) -> bool {
        let mut runtime = self.runtime.write();
        if runtime.connection_status == connection_status {
            return false;
        }
        runtime.connection_status = connection_status;
        true
    }
}

#[derive(Debug, Deserialize, Default)]
struct TrayPeersResponse {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    connected: usize,
}

fn tray_connection_status_from_peers(response: TrayPeersResponse) -> TrayConnectionStatus {
    let connected_peers = if response.enabled {
        Some(response.connected)
    } else {
        None
    };
    TrayConnectionStatus::Running { connected_peers }
}

fn tray_status_text(connection_status: TrayConnectionStatus) -> String {
    match connection_status {
        TrayConnectionStatus::Starting => "Starting daemon...".to_string(),
        TrayConnectionStatus::Running {
            connected_peers: None,
        } => "Daemon running".to_string(),
        TrayConnectionStatus::Running {
            connected_peers: Some(1),
        } => "Daemon running, 1 peer connected".to_string(),
        TrayConnectionStatus::Running {
            connected_peers: Some(connected_peers),
        } => format!("Daemon running, {} peers connected", connected_peers),
        TrayConnectionStatus::Failed => "Daemon failed to start".to_string(),
    }
}

const fn current_desktop_platform() -> DesktopPlatform {
    #[cfg(target_os = "macos")]
    {
        DesktopPlatform::MacOs
    }
    #[cfg(windows)]
    {
        DesktopPlatform::Windows
    }
    #[cfg(target_os = "linux")]
    {
        DesktopPlatform::Linux
    }
    #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
    {
        DesktopPlatform::Other
    }
}

const fn tray_primary_click_action(platform: DesktopPlatform) -> TrayPrimaryClickAction {
    match platform {
        DesktopPlatform::MacOs => TrayPrimaryClickAction::ShowMenu,
        DesktopPlatform::Windows | DesktopPlatform::Linux | DesktopPlatform::Other => {
            TrayPrimaryClickAction::OpenWindow
        }
    }
}

const fn tray_show_menu_on_left_click() -> bool {
    matches!(
        tray_primary_click_action(current_desktop_platform()),
        TrayPrimaryClickAction::ShowMenu
    )
}

fn tray_menu_spec(connection_status: TrayConnectionStatus) -> Vec<TrayMenuItemSpec> {
    vec![
        TrayMenuItemSpec::Text {
            id: None,
            text: tray_status_text(connection_status),
            enabled: false,
        },
        TrayMenuItemSpec::Separator,
        TrayMenuItemSpec::Text {
            id: Some(TRAY_OPEN_MENU_ID.to_string()),
            text: "Open Iris".to_string(),
            enabled: true,
        },
        TrayMenuItemSpec::Text {
            id: Some(TRAY_HOME_MENU_ID.to_string()),
            text: "Home".to_string(),
            enabled: true,
        },
        TrayMenuItemSpec::Text {
            id: Some(TRAY_SETTINGS_MENU_ID.to_string()),
            text: "Settings".to_string(),
            enabled: true,
        },
        TrayMenuItemSpec::Separator,
        TrayMenuItemSpec::Text {
            id: Some(TRAY_QUIT_MENU_ID.to_string()),
            text: "Quit".to_string(),
            enabled: true,
        },
    ]
}

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
fn append_tray_spec_to_menu<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    menu: &Menu<R>,
    spec: &TrayMenuItemSpec,
) -> tauri::Result<()> {
    match spec {
        TrayMenuItemSpec::Text { id, text, enabled } => {
            let item = if let Some(id) = id {
                MenuItemBuilder::with_id(id.clone(), text)
                    .enabled(*enabled)
                    .build(app)?
            } else {
                MenuItemBuilder::new(text).enabled(*enabled).build(app)?
            };
            menu.append(&item)?;
        }
        TrayMenuItemSpec::Separator => {
            let separator = PredefinedMenuItem::separator(app)?;
            menu.append(&separator)?;
        }
    }

    Ok(())
}

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
fn build_tray_menu<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    connection_status: TrayConnectionStatus,
) -> tauri::Result<Menu<R>> {
    let menu = Menu::new(app)?;
    for spec in tray_menu_spec(connection_status) {
        append_tray_spec_to_menu(app, &menu, &spec)?;
    }
    Ok(menu)
}

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
fn show_main_window<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> tauri::Result<()> {
    let Some(window) = app.get_webview_window("main") else {
        return Ok(());
    };

    let _ = window.unminimize();
    window.show()?;
    window.set_focus()?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
fn show_main_window<R: tauri::Runtime>(_app: &tauri::AppHandle<R>) -> tauri::Result<()> {
    Ok(())
}

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
fn hide_main_window_to_tray<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };

    let _ = window.minimize();
    let _ = window.hide();
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
fn hide_main_window_to_tray<R: tauri::Runtime>(_app: &tauri::AppHandle<R>) {}

fn started_minimized() -> bool {
    std::env::args().any(|arg| arg == "--minimized")
}

fn is_supported_launch_host(host: &str) -> bool {
    host == "self" || host.starts_with("nhash1") || host.starts_with("npub1")
}

fn normalize_supported_launch_deep_link(url: &tauri::Url) -> Option<String> {
    if url.scheme() != "htree" {
        return None;
    }

    let host = url.host_str()?;
    if !is_supported_launch_host(host) {
        return None;
    }

    Some(url.to_string())
}

fn collect_supported_launch_deep_links(urls: &[tauri::Url]) -> Vec<String> {
    urls.iter()
        .filter_map(normalize_supported_launch_deep_link)
        .collect()
}

fn automation_startup_url_from_env() -> Option<String> {
    let raw = std::env::var("IRIS_AUTOMATION_OPEN_URL").ok()?;
    normalize_automation_startup_url(&raw)
}

fn normalize_automation_startup_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let url = tauri::Url::parse(trimmed).ok()?;
    normalize_supported_launch_deep_link(&url)
}

fn emit_open_url_command<R: tauri::Runtime>(app: &tauri::AppHandle<R>, url: String) {
    tracing::info!("Emitting open_url automation command for deep link {}", url);
    let _ = show_main_window(app);
    let _ = app.emit(
        "automation-command",
        automation::AutomationCommand {
            action: automation::AutomationAction::OpenUrl,
            url: Some(url),
            request_id: None,
            decision: None,
        },
    );
}

fn handle_deep_link_urls<R: tauri::Runtime>(app: &tauri::AppHandle<R>, urls: &[tauri::Url]) {
    let supported_urls = collect_supported_launch_deep_links(urls);
    tracing::info!(
        "Handling deep-link URLs: total={} supported={:?}",
        urls.len(),
        supported_urls
    );
    if supported_urls.is_empty() {
        return;
    }

    let Some(state) = app.try_state::<Arc<DeepLinkState>>() else {
        return;
    };

    if state.is_frontend_ready() {
        tracing::info!("Deep-link frontend ready; emitting URLs immediately");
        for url in supported_urls {
            emit_open_url_command(app, url);
        }
    } else {
        tracing::info!("Deep-link frontend not ready; queueing URLs");
        let _ = show_main_window(app);
        state.queue_urls(supported_urls);
    }
}

#[tauri::command]
fn deep_link_frontend_ready<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<Vec<String>, String> {
    let state = app
        .try_state::<Arc<DeepLinkState>>()
        .ok_or_else(|| "DeepLinkState not found".to_string())?;
    let pending_urls = state.mark_frontend_ready();
    tracing::info!(
        "Frontend reported deep-link readiness; pending URLs={:?}",
        pending_urls
    );
    if !pending_urls.is_empty() {
        let _ = show_main_window(&app);
    }
    Ok(pending_urls)
}

#[tauri::command]
fn get_daemon_transport_settings() -> Result<DaemonTransportSettings, String> {
    hashtree_cli::Config::load()
        .map(|config| DaemonTransportSettings::from_config(&config))
        .map_err(|error| format!("Failed to load daemon transport settings: {}", error))
}

#[tauri::command]
fn get_daemon_network_settings() -> Result<DaemonNetworkSettings, String> {
    hashtree_cli::Config::load()
        .map(|config| DaemonNetworkSettings::from_config(&config))
        .map_err(|error| format!("Failed to load daemon network settings: {}", error))
}

#[tauri::command]
async fn update_daemon_transport_settings<R: tauri::Runtime>(
    _app: tauri::AppHandle<R>,
    daemon_runtime: tauri::State<'_, Arc<DaemonRuntimeState>>,
    settings: DaemonTransportSettings,
) -> Result<DaemonTransportSettings, String> {
    let mut config = hashtree_cli::Config::load()
        .map_err(|error| format!("Failed to load daemon transport settings: {}", error))?;
    let applied = apply_transport_settings(&mut config, &settings);
    config
        .save()
        .map_err(|error| format!("Failed to save daemon transport settings: {}", error))?;

    #[cfg(any(target_os = "android", target_os = "ios"))]
    if applied.bluetooth {
        match ensure_mobile_peer_id()
            .and_then(|peer_id| mobile_bluetooth::prestart_from_app(&_app, peer_id))
        {
            Ok(()) => info!("Prestarted mobile Bluetooth plugin from live settings update"),
            Err(error) => tracing::warn!(
                "Failed to prestart mobile Bluetooth plugin from settings update: {}",
                error
            ),
        }
    }

    let peer_router_controller = { daemon_runtime.peer_router_controller.read().clone() };
    if let Some(controller) = peer_router_controller {
        controller
            .apply_config(&config)
            .await
            .map_err(|error| format!("Failed to apply daemon transport settings: {}", error))?;
    }

    let background_services_controller =
        { daemon_runtime.background_services_controller.read().clone() };
    if let Some(controller) = background_services_controller {
        controller.apply_config(&config).await.map_err(|error| {
            format!(
                "Failed to apply daemon background service settings: {}",
                error
            )
        })?;
    }

    Ok(applied)
}

#[tauri::command]
async fn update_daemon_network_settings<R: tauri::Runtime>(
    _app: tauri::AppHandle<R>,
    daemon_runtime: tauri::State<'_, Arc<DaemonRuntimeState>>,
    settings: DaemonNetworkSettings,
) -> Result<DaemonNetworkSettings, String> {
    let mut config = hashtree_cli::Config::load()
        .map_err(|error| format!("Failed to load daemon network settings: {}", error))?;
    let applied = apply_network_settings(&mut config, &settings)?;
    config
        .save()
        .map_err(|error| format!("Failed to save daemon network settings: {}", error))?;

    #[cfg(any(target_os = "android", target_os = "ios"))]
    if applied.bluetooth {
        match ensure_mobile_peer_id()
            .and_then(|peer_id| mobile_bluetooth::prestart_from_app(&_app, peer_id))
        {
            Ok(()) => info!("Prestarted mobile Bluetooth plugin from live network update"),
            Err(error) => tracing::warn!(
                "Failed to prestart mobile Bluetooth plugin from network settings update: {}",
                error
            ),
        }
    }

    let peer_router_controller = { daemon_runtime.peer_router_controller.read().clone() };
    if let Some(controller) = peer_router_controller {
        controller
            .apply_config(&config)
            .await
            .map_err(|error| format!("Failed to apply daemon network settings: {}", error))?;
    }

    let background_services_controller =
        { daemon_runtime.background_services_controller.read().clone() };
    if let Some(controller) = background_services_controller {
        controller.apply_config(&config).await.map_err(|error| {
            format!(
                "Failed to apply daemon background service settings: {}",
                error
            )
        })?;
    }

    Ok(applied)
}

fn emit_tray_action<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    action: automation::AutomationAction,
) {
    let _ = app.emit(
        "automation-command",
        automation::AutomationCommand {
            action,
            url: None,
            request_id: None,
            decision: None,
        },
    );
}

fn current_tray_connection_status<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> TrayConnectionStatus {
    app.try_state::<Arc<TrayState>>()
        .map(|state| state.snapshot().connection_status)
        .unwrap_or(TrayConnectionStatus::Starting)
}

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
fn refresh_tray_menu<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    let Some(tray) = app.tray_by_id(TRAY_ICON_ID) else {
        return;
    };

    let connection_status = current_tray_connection_status(app);
    let status_text = tray_status_text(connection_status);

    if let Ok(menu) = build_tray_menu(app, connection_status) {
        let _ = tray.set_menu(Some(menu));
    }
    let _ = tray.set_tooltip(Some(status_text));
}

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
fn update_tray_connection_status<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    connection_status: TrayConnectionStatus,
) {
    let Some(state) = app.try_state::<Arc<TrayState>>() else {
        return;
    };
    if !state.set_connection_status(connection_status) {
        return;
    }

    let refresh_app = app.clone();
    let _ = app.run_on_main_thread(move || refresh_tray_menu(&refresh_app));
}

fn fetch_tray_connection_status(
    client: &reqwest::blocking::Client,
    url: &str,
) -> Option<TrayConnectionStatus> {
    let response = client.get(url).send().ok()?;
    let status = response.json::<TrayPeersResponse>().ok()?;
    Some(tray_connection_status_from_peers(status))
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
fn refresh_tray_menu<R: tauri::Runtime>(_app: &tauri::AppHandle<R>) {}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
fn update_tray_connection_status<R: tauri::Runtime>(
    _app: &tauri::AppHandle<R>,
    _connection_status: TrayConnectionStatus,
) {
}

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
fn spawn_tray_status_poller<R: tauri::Runtime + 'static>(app: tauri::AppHandle<R>, port: u16) {
    std::thread::spawn(move || {
        let client = match reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                tracing::warn!("Failed to build tray status client: {}", error);
                return;
            }
        };
        let url = format!("http://127.0.0.1:{}/api/peers", port);

        loop {
            let connection_status = fetch_tray_connection_status(&client, &url).unwrap_or(
                TrayConnectionStatus::Running {
                    connected_peers: None,
                },
            );
            update_tray_connection_status(&app, connection_status);
            std::thread::sleep(Duration::from_secs(5));
        }
    });
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
fn spawn_tray_status_poller<R: tauri::Runtime + 'static>(_app: tauri::AppHandle<R>, _port: u16) {}

#[cfg(any(target_os = "macos", windows, target_os = "linux", test))]
#[cfg(test)]
fn build_edit_menu<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> tauri::Result<tauri::menu::Submenu<R>> {
    let cut = MenuItemBuilder::with_id("edit_cut", "Cut")
        .accelerator("CmdOrCtrl+X")
        .build(app)?;
    let copy = MenuItemBuilder::with_id("edit_copy", "Copy")
        .accelerator("CmdOrCtrl+C")
        .build(app)?;
    let paste = MenuItemBuilder::with_id("edit_paste", "Paste")
        .accelerator("CmdOrCtrl+V")
        .build(app)?;
    let select_all = MenuItemBuilder::with_id("edit_select_all", "Select All")
        .accelerator("CmdOrCtrl+A")
        .build(app)?;

    SubmenuBuilder::with_id(app, "edit_menu", "Edit")
        .item(&cut)
        .item(&copy)
        .item(&paste)
        .item(&select_all)
        .build()
}

#[cfg(any(target_os = "macos", windows, target_os = "linux", test))]
#[cfg(not(test))]
fn build_edit_menu<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> tauri::Result<tauri::menu::Submenu<R>> {
    SubmenuBuilder::with_id(app, "edit_menu", "Edit")
        .cut()
        .copy()
        .paste()
        .select_all()
        .build()
}

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
#[cfg(target_os = "macos")]
const fn developer_tools_accelerator() -> &'static str {
    "Cmd+Alt+I"
}

#[cfg(any(target_os = "linux", windows))]
const fn developer_tools_accelerator() -> &'static str {
    "CmdOrCtrl+Shift+I"
}

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
fn build_view_menu<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> tauri::Result<tauri::menu::Submenu<R>> {
    let developer_tools =
        MenuItemBuilder::with_id(TOGGLE_CHILD_WEBVIEW_DEVTOOLS_MENU_ID, "Developer Tools")
            .accelerator(developer_tools_accelerator())
            .build(app)?;

    SubmenuBuilder::new(app, "View")
        .item(&developer_tools)
        .build()
}

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
fn toggle_child_webview_devtools<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    if let Some(webview) = app.get_webview(CHILD_WEBVIEW_LABEL) {
        if webview.is_devtools_open() {
            webview.close_devtools();
        } else {
            webview.open_devtools();
        }
    }
}

#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
fn build_menu<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> tauri::Result<tauri::menu::Menu<R>> {
    let app_name = app.package_info().name.clone();
    let quit = MenuItemBuilder::with_id("app_quit", "Quit")
        .accelerator("CmdOrCtrl+Q")
        .build(app)?;
    let app_menu = SubmenuBuilder::new(app, app_name).item(&quit).build()?;

    let back = MenuItemBuilder::with_id("nav_back", "Back")
        .accelerator("CmdOrCtrl+Left")
        .build(app)?;
    let forward = MenuItemBuilder::with_id("nav_forward", "Forward")
        .accelerator("CmdOrCtrl+Right")
        .build(app)?;

    let navigation = SubmenuBuilder::new(app, "Navigation")
        .item(&back)
        .item(&forward)
        .build()?;

    let edit = build_edit_menu(app)?;
    let view = build_view_menu(app)?;

    MenuBuilder::new(app)
        .item(&app_menu)
        .item(&edit)
        .item(&view)
        .item(&navigation)
        .build()
}

// ============================================
// App entry point
// ============================================

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    ensure_rustls_provider();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("iris=info,hashtree_cli::server=info")),
        )
        .init();

    let mut builder = tauri::Builder::default();

    #[cfg(any(target_os = "macos", windows, target_os = "linux"))]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            let _ = show_main_window(app);
        }));
    }

    builder = builder.plugin(tauri_plugin_deep_link::init());

    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        builder = builder.plugin(tauri_plugin_iris_mobile_browser::init());
    }

    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        builder = builder.plugin(tauri_plugin_iris_mobile_bluetooth::init());
    }

    #[cfg(any(target_os = "macos", windows, target_os = "linux"))]
    {
        builder = builder
            .menu(build_menu)
            .on_tray_icon_event(|app, event| {
                if let TrayIconEvent::Click {
                    button,
                    button_state,
                    ..
                } = event
                {
                    if matches!(
                        tray_primary_click_action(current_desktop_platform()),
                        TrayPrimaryClickAction::OpenWindow
                    ) && button == MouseButton::Left
                        && button_state == MouseButtonState::Up
                    {
                        let _ = show_main_window(app);
                    }
                }
            })
            .on_menu_event(|app, event| match event.id().as_ref() {
                "nav_back" => {
                    let _ = app.emit(
                        "child-webview-navigate",
                        serde_json::json!({ "action": "back" }),
                    );
                }
                "nav_forward" => {
                    let _ = app.emit(
                        "child-webview-navigate",
                        serde_json::json!({ "action": "forward" }),
                    );
                }
                TOGGLE_CHILD_WEBVIEW_DEVTOOLS_MENU_ID => {
                    toggle_child_webview_devtools(app);
                }
                TRAY_OPEN_MENU_ID => {
                    let _ = show_main_window(app);
                }
                TRAY_HOME_MENU_ID => {
                    let _ = show_main_window(app);
                    emit_tray_action(app, automation::AutomationAction::Home);
                }
                TRAY_SETTINGS_MENU_ID => {
                    let _ = show_main_window(app);
                    emit_tray_action(app, automation::AutomationAction::Settings);
                }
                TRAY_QUIT_MENU_ID | "app_quit" => {
                    app.exit(0);
                }
                _ => {}
            });
    }

    builder
        .plugin(tauri_plugin_os::init())
        .register_asynchronous_uri_scheme_protocol("htree", htree_protocol::handle_htree_protocol)
        .invoke_handler(tauri::generate_handler![
            automation::automation_update_state,
            automation::automation_get_state,
            automation::automation_shutdown,
            deep_link_frontend_ready,
            get_daemon_transport_settings,
            get_daemon_network_settings,
            update_daemon_transport_settings,
            update_daemon_network_settings,
            htree_protocol::get_htree_server_url,
            htree_protocol::cache_tree_root,
            htree_protocol::clear_tree_root_cache,
            nip07::create_nip07_webview,
            nip07::create_htree_webview,
            nip07::close_webview,
            nip07::navigate_webview,
            nip07::set_webview_bounds,
            nip07::set_mobile_shell_overlay,
            nip07::webview_history,
            nip07::reload_webview,
            nip07::webview_current_url,
            nip07::nip07_request,
            nip07::get_nip07_account,
            nip07::list_nip07_accounts,
            nip07::login_nip07_account,
            nip07::generate_nip07_account,
            nip07::logout_nip07_account,
            nip07::set_active_nip07_account,
            nip07::remove_nip07_account,
            nip07::export_nip07_account_secret,
            nip07::take_nip07_permission_prompt,
            nip07::respond_nip07_permission_prompt,
            nip07::show_native_nip07_permission_dialog,
            nip07::webview_event,
            pwa::install_site_pwa,
            pwa::cache_bookmark_icon,
            history::record_history_visit,
            history::search_history,
            history::get_recent_history,
            history::delete_history_entry,
            history::clear_history
        ])
        .on_page_load(|webview, payload| {
            if webview.label() == "main" {
                if matches!(payload.event(), tauri::webview::PageLoadEvent::Finished) {
                    info!("Main window page loaded: {}", payload.url());

                    // Inject daemon server URL so the web app can find it
                    let port = htree_protocol::get_daemon_port().unwrap_or(21417);
                    let inject_url = format!(
                        "window.__HTREE_SERVER_URL__ = 'http://127.0.0.1:{}';",
                        port
                    );
                    if let Err(e) = webview.eval(&inject_url) {
                        tracing::warn!("Failed to inject __HTREE_SERVER_URL__: {}", e);
                    }

                    // Inject NIP-07 window.nostr
                    let script = nip07::generate_main_window_nip07_script();
                    if let Err(e) = webview.eval(&script) {
                        tracing::warn!("Failed to inject NIP-07 script: {}", e);
                    } else {
                        info!("Injected NIP-07 window.nostr and __HTREE_SERVER_URL__ into main window");
                    }
                }
            }
        })
        .setup(|app| {
            let shell_data_dir = app
                .path()
                .app_data_dir()
                .expect("failed to get app data dir");
            let (shared_config_dir, shared_data_dir) = default_htree_paths(&shell_data_dir);
            let paths = resolve_iris_paths(
                shell_data_dir,
                env_path("HTREE_CONFIG_DIR"),
                env_path("HTREE_DATA_DIR"),
                shared_config_dir,
                shared_data_dir,
            );

            std::fs::create_dir_all(&paths.shell_data_dir)
                .expect("failed to create iris shell data dir");
            std::fs::create_dir_all(&paths.htree_config_dir)
                .expect("failed to create shared htree config dir");
            std::fs::create_dir_all(&paths.htree_data_dir)
                .expect("failed to create shared htree data dir");

            info!("Iris shell data directory: {:?}", paths.shell_data_dir);
            info!("Hashtree config directory: {:?}", paths.htree_config_dir);
            info!("Hashtree data directory: {:?}", paths.htree_data_dir);

            std::env::set_var("HTREE_CONFIG_DIR", &paths.htree_config_dir);
            std::env::set_var("HTREE_DATA_DIR", &paths.htree_data_dir);
            std::env::set_var("HTREE_BLUETOOTH_NOSTR_ONLY", "1");

            if let Err(error) = ensure_iris_default_network_config(&paths) {
                tracing::warn!("Failed to apply Iris default network config: {}", error);
            }

            #[cfg(any(target_os = "android", target_os = "ios"))]
            if let Err(error) = mobile_bluetooth::install_from_app(&app.handle()) {
                tracing::warn!("Failed to install mobile Bluetooth bridge: {}", error);
            }
            #[cfg(any(target_os = "android", target_os = "ios"))]
            match hashtree_cli::Config::load() {
                Ok(config)
                    if config.server.enable_bluetooth && config.server.max_bluetooth_peers > 0 =>
                {
                    match ensure_mobile_peer_id()
                        .and_then(|peer_id| mobile_bluetooth::prestart_from_app(&app.handle(), peer_id))
                    {
                        Ok(()) => info!("Prestarted mobile Bluetooth plugin from app setup"),
                        Err(error) => {
                            tracing::warn!("Failed to prestart mobile Bluetooth plugin: {}", error)
                        }
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!("Failed to load config for mobile Bluetooth prestart: {}", error)
                }
            }

            app.handle().plugin(tauri_plugin_secure_storage::init())?;

            // Initialize NIP-07 permission state
            let permission_store = Arc::new(permissions::PermissionStore::new(Some(
                paths.shell_data_dir.join("nip07-permissions.json"),
            )));
            let nip07_account_path = paths.shell_data_dir.join("nip07-account.json");
            let nip07_confidential_path = paths.shell_data_dir.join("confidential.json");
            let nip07_secret_store = nip07::confidential_store(
                app.handle().clone(),
                Some(nip07_confidential_path),
            );
            let nip07_state = Arc::new(nip07::Nip07State::new(
                permission_store,
                nip07_account_path,
                nip07_secret_store,
            ));
            nip07::init_global_state(nip07_state.clone());
            app.manage(nip07_state);

            // Initialize history store
            let history_store = Arc::new(
                history::HistoryStore::new(&paths.shell_data_dir)
                    .expect("failed to initialize history store"),
            );
            app.manage(history_store);

            let tray_state = Arc::new(TrayState::new());
            app.manage(tray_state);

            let automation_state = Arc::new(automation::AutomationState::new(
                automation::automation_requested(),
            ));
            automation::maybe_start_server(app.handle().clone(), automation_state.clone());
            app.manage(automation_state);

            let deep_link_state = Arc::new(DeepLinkState::new());
            if let Some(url) = automation_startup_url_from_env() {
                deep_link_state.queue_urls([url]);
            }
            app.manage(deep_link_state);

            let daemon_runtime_state = Arc::new(DaemonRuntimeState::default());
            app.manage(daemon_runtime_state.clone());
            let pwa_install_state = Arc::new(pwa::PwaInstallState::default());
            app.manage(pwa_install_state.clone());

            #[cfg(any(target_os = "linux", all(debug_assertions, windows)))]
            if let Err(error) = app.deep_link().register_all() {
                tracing::warn!("Failed to register deep-link schemes at runtime: {}", error);
            }

            match app.deep_link().get_current() {
                Ok(Some(start_urls)) => handle_deep_link_urls(&app.handle().clone(), &start_urls),
                Ok(None) => {}
                Err(error) => {
                    tracing::debug!("Deep-link startup URLs unavailable: {}", error);
                }
            }

            let app_for_deep_links = app.handle().clone();
            let _deep_link_listener = app.deep_link().on_open_url(move |event| {
                handle_deep_link_urls(&app_for_deep_links, &event.urls());
            });

            // Start the embedded htree daemon
            let daemon_data_dir = paths.htree_data_dir.clone();
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                match start_daemon(app_handle.clone(), daemon_data_dir).await {
                    Ok(info) => {
                        *daemon_runtime_state.peer_router_controller.write() =
                            info.peer_router_controller.clone();
                        *daemon_runtime_state.background_services_controller.write() =
                            info.background_services_controller.clone();
                        *pwa_install_state.store.write() = Some(info.store.clone());
                        htree_protocol::set_daemon_port(info.port);
                        htree_protocol::set_self_npub(info.npub.clone());
                        info!("Embedded daemon started on port {}", info.port);
                        update_tray_connection_status(
                            &app_handle,
                            TrayConnectionStatus::Running {
                                connected_peers: None,
                            },
                        );
                        spawn_tray_status_poller(app_handle.clone(), info.port);
                    }
                    Err(e) => {
                        tracing::error!("Failed to start embedded daemon: {}", e);
                        update_tray_connection_status(&app_handle, TrayConnectionStatus::Failed);
                    }
                }
            });

            #[cfg(any(target_os = "macos", windows, target_os = "linux"))]
            {
                if let Some(tray) = app.tray_by_id(TRAY_ICON_ID) {
                    let _ = tray.set_show_menu_on_left_click(tray_show_menu_on_left_click());
                }
                refresh_tray_menu(app.handle());

                if started_minimized() {
                    hide_main_window_to_tray(app.handle());
                    info!("Started hidden in tray (autostart)");
                }
            }

            // Add plugins
            app.handle().plugin(tauri_plugin_notification::init())?;
            app.handle().plugin(tauri_plugin_opener::init())?;
            app.handle().plugin(tauri_plugin_dialog::init())?;

            #[cfg(any(target_os = "macos", windows, target_os = "linux"))]
            app.handle().plugin(tauri_plugin_autostart::init(
                tauri_plugin_autostart::MacosLauncher::LaunchAgent,
                Some(vec!["--minimized"]),
            ))?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::{
        apply_iris_transport_defaults, apply_network_settings, apply_transport_settings,
        bluetooth_enabled_by_default_for_platform, collect_supported_launch_deep_links,
        ensure_iris_default_network_config_for_platform, is_supported_launch_host,
        mobile_default_htree_paths, normalize_automation_startup_url,
        normalize_supported_launch_deep_link, resolve_iris_paths,
        tray_connection_status_from_peers, tray_menu_spec, tray_primary_click_action,
        tray_status_text, BluetoothDefaultPlatform, DaemonBlossomServerSettings,
        DaemonNetworkSettings, DaemonTransportSettings, DesktopPlatform, IrisPaths,
        TrayConnectionStatus, TrayMenuItemSpec, TrayPeersResponse, TrayPrimaryClickAction,
        DEFAULT_BLUETOOTH_TOGGLE_MAX_PEERS, DEFAULT_MULTICAST_TOGGLE_MAX_PEERS,
        IRIS_BLUETOOTH_DEFAULTS_MARKER_FILE,
    };
    #[cfg(any(target_os = "macos", windows, target_os = "linux"))]
    use super::{build_menu, developer_tools_accelerator};
    use std::ffi::{OsStr, OsString};
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};
    use tempfile::TempDir;

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.previous {
                std::env::set_var(self.key, value);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    #[cfg(any(target_os = "macos", windows, target_os = "linux"))]
    #[cfg_attr(target_os = "macos", ignore = "requires main thread for menu items")]
    #[test]
    fn app_menu_includes_quit_item() {
        let app = tauri::test::mock_app();
        let handle = app.handle();
        let menu = build_menu(&handle).expect("failed to build menu");
        let mut has_quit = false;

        for item in menu.items().unwrap_or_default() {
            if let tauri::menu::MenuItemKind::Submenu(submenu) = item {
                for subitem in submenu.items().unwrap_or_default() {
                    if subitem.id().as_ref() == "app_quit" {
                        has_quit = true;
                    }
                }
            }
        }

        assert!(has_quit, "expected app_quit menu item");
    }

    #[cfg(any(target_os = "macos", windows, target_os = "linux"))]
    #[cfg_attr(target_os = "macos", ignore = "requires main thread for menu items")]
    #[test]
    fn app_menu_includes_child_webview_devtools_item() {
        let app = tauri::test::mock_app();
        let handle = app.handle();
        let menu = build_menu(&handle).expect("failed to build menu");
        let mut has_devtools = false;

        for item in menu.items().unwrap_or_default() {
            if let tauri::menu::MenuItemKind::Submenu(submenu) = item {
                for subitem in submenu.items().unwrap_or_default() {
                    if subitem.id().as_ref() == "view_toggle_child_webview_devtools" {
                        has_devtools = true;
                    }
                }
            }
        }

        assert!(has_devtools, "expected child webview devtools menu item");
    }

    #[cfg(any(target_os = "macos", windows, target_os = "linux"))]
    #[test]
    fn child_webview_devtools_accelerator_matches_platform_conventions() {
        #[cfg(target_os = "macos")]
        assert_eq!(developer_tools_accelerator(), "Cmd+Alt+I");

        #[cfg(not(target_os = "macos"))]
        assert_eq!(developer_tools_accelerator(), "CmdOrCtrl+Shift+I");
    }

    #[test]
    fn tray_status_text_covers_starting_running_and_failure_states() {
        assert_eq!(
            tray_status_text(TrayConnectionStatus::Starting),
            "Starting daemon..."
        );
        assert_eq!(
            tray_status_text(TrayConnectionStatus::Running {
                connected_peers: None,
            }),
            "Daemon running"
        );
        assert_eq!(
            tray_status_text(TrayConnectionStatus::Running {
                connected_peers: Some(1),
            }),
            "Daemon running, 1 peer connected"
        );
        assert_eq!(
            tray_status_text(TrayConnectionStatus::Running {
                connected_peers: Some(3),
            }),
            "Daemon running, 3 peers connected"
        );
        assert_eq!(
            tray_status_text(TrayConnectionStatus::Failed),
            "Daemon failed to start"
        );
    }

    #[test]
    fn tray_menu_spec_stays_small_and_action_focused() {
        let items = tray_menu_spec(TrayConnectionStatus::Running {
            connected_peers: Some(2),
        });

        assert_eq!(
            items,
            vec![
                TrayMenuItemSpec::Text {
                    id: None,
                    text: "Daemon running, 2 peers connected".to_string(),
                    enabled: false,
                },
                TrayMenuItemSpec::Separator,
                TrayMenuItemSpec::Text {
                    id: Some("tray_open_main".to_string()),
                    text: "Open Iris".to_string(),
                    enabled: true,
                },
                TrayMenuItemSpec::Text {
                    id: Some("tray_home".to_string()),
                    text: "Home".to_string(),
                    enabled: true,
                },
                TrayMenuItemSpec::Text {
                    id: Some("tray_settings".to_string()),
                    text: "Settings".to_string(),
                    enabled: true,
                },
                TrayMenuItemSpec::Separator,
                TrayMenuItemSpec::Text {
                    id: Some("tray_quit".to_string()),
                    text: "Quit".to_string(),
                    enabled: true,
                },
            ]
        );
    }

    #[test]
    fn tray_primary_click_prefers_menu_only_on_macos() {
        assert_eq!(
            tray_primary_click_action(DesktopPlatform::MacOs),
            TrayPrimaryClickAction::ShowMenu
        );
        assert_eq!(
            tray_primary_click_action(DesktopPlatform::Windows),
            TrayPrimaryClickAction::OpenWindow
        );
        assert_eq!(
            tray_primary_click_action(DesktopPlatform::Linux),
            TrayPrimaryClickAction::OpenWindow
        );
        assert_eq!(
            tray_primary_click_action(DesktopPlatform::Other),
            TrayPrimaryClickAction::OpenWindow
        );
    }

    #[test]
    fn tray_connection_status_uses_peer_endpoint_shape() {
        assert_eq!(
            tray_connection_status_from_peers(TrayPeersResponse {
                enabled: true,
                connected: 4,
            }),
            TrayConnectionStatus::Running {
                connected_peers: Some(4),
            }
        );
        assert_eq!(
            tray_connection_status_from_peers(TrayPeersResponse {
                enabled: false,
                connected: 99,
            }),
            TrayConnectionStatus::Running {
                connected_peers: None,
            }
        );
    }

    #[test]
    fn resolve_iris_paths_keeps_shell_state_separate_from_shared_hashtree_paths() {
        let paths = resolve_iris_paths(
            PathBuf::from("/tmp/iris"),
            None,
            None,
            PathBuf::from("/home/test/.hashtree"),
            PathBuf::from("/home/test/.hashtree/data"),
        );

        assert_eq!(
            paths,
            IrisPaths {
                shell_data_dir: PathBuf::from("/tmp/iris"),
                htree_config_dir: PathBuf::from("/home/test/.hashtree"),
                htree_data_dir: PathBuf::from("/home/test/.hashtree/data"),
            }
        );
    }

    #[test]
    fn resolve_iris_paths_respects_explicit_htree_overrides() {
        let paths = resolve_iris_paths(
            PathBuf::from("/tmp/iris"),
            Some(PathBuf::from("/tmp/htree-config")),
            Some(PathBuf::from("/tmp/htree-data")),
            PathBuf::from("/home/test/.hashtree"),
            PathBuf::from("/home/test/.hashtree/data"),
        );

        assert_eq!(
            paths,
            IrisPaths {
                shell_data_dir: PathBuf::from("/tmp/iris"),
                htree_config_dir: PathBuf::from("/tmp/htree-config"),
                htree_data_dir: PathBuf::from("/tmp/htree-data"),
            }
        );
    }

    #[test]
    fn mobile_default_htree_paths_live_under_shell_state() {
        let (config_dir, data_dir) = mobile_default_htree_paths(Path::new("/tmp/iris"));

        assert_eq!(config_dir, PathBuf::from("/tmp/iris/hashtree"));
        assert_eq!(data_dir, PathBuf::from("/tmp/iris/hashtree/data"));
    }

    #[test]
    fn supported_launch_hosts_match_user_facing_htree_targets_only() {
        assert!(is_supported_launch_host("self"));
        assert!(is_supported_launch_host("nhash1example"));
        assert!(is_supported_launch_host("npub1example"));
        assert!(is_supported_launch_host("npub1example.videos%2FMy%20Clip"));
        assert!(!is_supported_launch_host("nip07"));
        assert!(!is_supported_launch_host("webview"));
        assert!(!is_supported_launch_host(""));
    }

    #[test]
    fn normalize_supported_launch_deep_link_accepts_user_facing_htree_urls() {
        let url = tauri::Url::parse("htree://self/video/index.html?autoplay=1").unwrap();
        assert_eq!(
            normalize_supported_launch_deep_link(&url),
            Some("htree://self/video/index.html?autoplay=1".to_string())
        );
    }

    #[test]
    fn normalize_supported_launch_deep_link_rejects_internal_or_non_htree_urls() {
        let http_url = tauri::Url::parse("https://files.iris.to").unwrap();
        let nip07_url = tauri::Url::parse("htree://nip07/").unwrap();
        let webview_url = tauri::Url::parse("htree://webview/").unwrap();

        assert_eq!(normalize_supported_launch_deep_link(&http_url), None);
        assert_eq!(normalize_supported_launch_deep_link(&nip07_url), None);
        assert_eq!(normalize_supported_launch_deep_link(&webview_url), None);
    }

    #[test]
    fn collect_supported_launch_deep_links_filters_out_non_launchable_urls() {
        let urls = vec![
            tauri::Url::parse("htree://self/video").unwrap(),
            tauri::Url::parse("htree://nip07/").unwrap(),
            tauri::Url::parse("https://files.iris.to").unwrap(),
            tauri::Url::parse("htree://npub1example/video").unwrap(),
        ];

        assert_eq!(
            collect_supported_launch_deep_links(&urls),
            vec![
                "htree://self/video".to_string(),
                "htree://npub1example/video".to_string(),
            ]
        );
    }

    #[test]
    fn automation_startup_url_accepts_user_facing_htree_targets() {
        assert_eq!(
            normalize_automation_startup_url(" htree://npub1example/video/index.html?autoplay=1 "),
            Some("htree://npub1example/video/index.html?autoplay=1".to_string())
        );
    }

    #[test]
    fn automation_startup_url_rejects_internal_or_invalid_urls() {
        assert_eq!(normalize_automation_startup_url(""), None);
        assert_eq!(normalize_automation_startup_url("not a url"), None);
        assert_eq!(normalize_automation_startup_url("htree://nip07/"), None);
        assert_eq!(
            normalize_automation_startup_url("https://files.iris.to"),
            None
        );
    }

    #[test]
    fn apply_transport_settings_enables_zero_max_transports_with_defaults() {
        let mut config = hashtree_cli::Config::default();
        config.server.enable_webrtc = false;
        config.server.enable_multicast = false;
        config.server.max_multicast_peers = 0;
        config.server.enable_bluetooth = false;
        config.server.max_bluetooth_peers = 0;

        let applied = apply_transport_settings(
            &mut config,
            &DaemonTransportSettings {
                webrtc: true,
                multicast: true,
                bluetooth: true,
                max_multicast_peers: 0,
                max_bluetooth_peers: 0,
            },
        );

        assert!(applied.webrtc);
        assert!(applied.multicast);
        assert!(applied.bluetooth);
        assert_eq!(
            applied.max_multicast_peers,
            DEFAULT_MULTICAST_TOGGLE_MAX_PEERS
        );
        assert_eq!(
            applied.max_bluetooth_peers,
            DEFAULT_BLUETOOTH_TOGGLE_MAX_PEERS
        );
    }

    #[test]
    fn iris_transport_defaults_enable_supported_platforms() {
        for platform in [
            BluetoothDefaultPlatform::Android,
            BluetoothDefaultPlatform::Ios,
            BluetoothDefaultPlatform::MacOs,
        ] {
            let mut config = hashtree_cli::Config::default();
            config.server.enable_multicast = false;
            config.server.max_multicast_peers = 0;
            let changed = apply_iris_transport_defaults(&mut config, platform);

            assert!(bluetooth_enabled_by_default_for_platform(platform));
            assert!(changed);
            assert!(config.server.enable_multicast);
            assert_eq!(
                config.server.max_multicast_peers,
                DEFAULT_MULTICAST_TOGGLE_MAX_PEERS
            );
            assert!(config.server.enable_bluetooth);
            assert_eq!(
                config.server.max_bluetooth_peers,
                DEFAULT_BLUETOOTH_TOGGLE_MAX_PEERS
            );
        }
    }

    #[test]
    fn iris_transport_defaults_skip_unsupported_platforms_and_existing_choices() {
        let mut unsupported = hashtree_cli::Config::default();
        unsupported.server.enable_multicast = false;
        unsupported.server.max_multicast_peers = 0;
        let changed = apply_iris_transport_defaults(&mut unsupported, BluetoothDefaultPlatform::Other);
        assert!(changed);
        assert!(!unsupported.server.enable_bluetooth);
        assert_eq!(unsupported.server.max_bluetooth_peers, 0);
        assert!(unsupported.server.enable_multicast);
        assert_eq!(
            unsupported.server.max_multicast_peers,
            DEFAULT_MULTICAST_TOGGLE_MAX_PEERS
        );

        let mut existing = hashtree_cli::Config::default();
        existing.server.enable_multicast = true;
        existing.server.max_multicast_peers = 9;
        existing.server.enable_bluetooth = true;
        existing.server.max_bluetooth_peers = 2;
        let changed = apply_iris_transport_defaults(&mut existing, BluetoothDefaultPlatform::Android);
        assert!(!changed);
        assert!(existing.server.enable_multicast);
        assert_eq!(existing.server.max_multicast_peers, 9);
        assert!(existing.server.enable_bluetooth);
        assert_eq!(existing.server.max_bluetooth_peers, 2);
    }

    #[test]
    fn iris_transport_defaults_reapply_after_marker_version_bump() {
        let _lock = env_lock().lock().expect("env lock");
        let temp = TempDir::new().expect("temp dir");
        let shell_data_dir = temp.path().join("iris-shell");
        let htree_config_dir = temp.path().join("htree-config");
        let htree_data_dir = temp.path().join("htree-data");
        std::fs::create_dir_all(&shell_data_dir).expect("create shell data dir");
        std::fs::create_dir_all(&htree_config_dir).expect("create config dir");
        std::fs::create_dir_all(&htree_data_dir).expect("create data dir");

        std::fs::write(shell_data_dir.join(".iris-bluetooth-defaults-v2"), b"v2\n")
            .expect("write old marker");

        let _config_env = EnvVarGuard::set("HTREE_CONFIG_DIR", &htree_config_dir);
        let _data_env = EnvVarGuard::set("HTREE_DATA_DIR", &htree_data_dir);

        ensure_iris_default_network_config_for_platform(
            &IrisPaths {
                shell_data_dir: shell_data_dir.clone(),
                htree_config_dir,
                htree_data_dir,
            },
            BluetoothDefaultPlatform::MacOs,
        )
        .expect("apply Iris defaults");

        let config = hashtree_cli::Config::load().expect("load updated config");
        assert!(config.server.enable_multicast);
        assert_eq!(
            config.server.max_multicast_peers,
            DEFAULT_MULTICAST_TOGGLE_MAX_PEERS
        );
        assert!(config.server.enable_bluetooth);
        assert_eq!(
            config.server.max_bluetooth_peers,
            DEFAULT_BLUETOOTH_TOGGLE_MAX_PEERS
        );
        assert!(shell_data_dir
            .join(IRIS_BLUETOOTH_DEFAULTS_MARKER_FILE)
            .exists());
    }

    #[test]
    fn apply_transport_settings_disables_multicast_and_bluetooth_by_zeroing_limits() {
        let mut config = hashtree_cli::Config::default();
        config.server.enable_multicast = true;
        config.server.max_multicast_peers = 9;
        config.server.enable_bluetooth = true;
        config.server.max_bluetooth_peers = 4;

        let applied = apply_transport_settings(
            &mut config,
            &DaemonTransportSettings {
                webrtc: false,
                multicast: false,
                bluetooth: false,
                max_multicast_peers: 9,
                max_bluetooth_peers: 4,
            },
        );

        assert!(!applied.webrtc);
        assert!(!applied.multicast);
        assert!(!applied.bluetooth);
        assert!(!config.server.enable_bluetooth);
        assert_eq!(config.server.max_multicast_peers, 0);
        assert_eq!(config.server.max_bluetooth_peers, 0);
    }

    #[test]
    fn daemon_network_settings_from_config_merges_blossom_modes() {
        let mut config = hashtree_cli::Config::default();
        config.nostr.relays = vec![
            "wss://relay.one".to_string(),
            "ws://127.0.0.1:21417/ws".to_string(),
        ];
        config.blossom.servers = vec!["https://both.example".to_string()];
        config.blossom.read_servers = vec![
            "https://both.example".to_string(),
            "https://read.example".to_string(),
        ];
        config.blossom.write_servers = vec![
            "https://both.example".to_string(),
            "https://write.example".to_string(),
        ];

        let settings = DaemonNetworkSettings::from_config(&config);

        assert!(settings.nostr_relays_enabled);
        assert!(settings.blossom_enabled);
        assert_eq!(settings.relay_urls, config.nostr.relays);
        assert_eq!(
            settings.blossom_servers,
            vec![
                DaemonBlossomServerSettings {
                    url: "https://both.example".to_string(),
                    read: true,
                    write: true,
                },
                DaemonBlossomServerSettings {
                    url: "https://read.example".to_string(),
                    read: true,
                    write: false,
                },
                DaemonBlossomServerSettings {
                    url: "https://write.example".to_string(),
                    read: false,
                    write: true,
                },
            ]
        );
    }

    #[test]
    fn apply_network_settings_round_trips_relays_and_blossom_servers() {
        let mut config = hashtree_cli::Config::default();

        let applied = apply_network_settings(
            &mut config,
            &DaemonNetworkSettings {
                webrtc: true,
                multicast: true,
                bluetooth: true,
                nostr_relays_enabled: false,
                blossom_enabled: false,
                max_multicast_peers: 0,
                max_bluetooth_peers: 0,
                multicast_group: "239.255.42.77".to_string(),
                multicast_port: 49_123,
                relay_urls: vec![
                    "wss://relay.example".to_string(),
                    "ws://127.0.0.1:21417/ws".to_string(),
                ],
                blossom_servers: vec![
                    DaemonBlossomServerSettings {
                        url: "https://read-write.example".to_string(),
                        read: true,
                        write: true,
                    },
                    DaemonBlossomServerSettings {
                        url: "https://read-only.example".to_string(),
                        read: true,
                        write: false,
                    },
                    DaemonBlossomServerSettings {
                        url: "https://write-only.example".to_string(),
                        read: false,
                        write: true,
                    },
                ],
            },
        )
        .expect("apply daemon network settings");

        assert_eq!(
            applied.max_multicast_peers,
            DEFAULT_MULTICAST_TOGGLE_MAX_PEERS
        );
        assert!(applied.bluetooth);
        assert_eq!(
            applied.max_bluetooth_peers,
            DEFAULT_BLUETOOTH_TOGGLE_MAX_PEERS
        );
        assert!(!applied.nostr_relays_enabled);
        assert!(!applied.blossom_enabled);
        assert_eq!(applied.multicast_group, "239.255.42.77");
        assert_eq!(applied.multicast_port, 49_123);
        assert!(!config.nostr.enabled);
        assert!(config.server.enable_bluetooth);
        assert_eq!(
            config.server.max_bluetooth_peers,
            DEFAULT_BLUETOOTH_TOGGLE_MAX_PEERS
        );
        assert_eq!(
            config.nostr.relays,
            vec![
                "wss://relay.example".to_string(),
                "ws://127.0.0.1:21417/ws".to_string(),
            ]
        );
        assert_eq!(
            config.blossom.servers,
            vec!["https://read-write.example".to_string()]
        );
        assert_eq!(
            config.blossom.read_servers,
            vec!["https://read-only.example".to_string()]
        );
        assert!(!config.blossom.enabled);
        assert_eq!(
            config.blossom.write_servers,
            vec!["https://write-only.example".to_string()]
        );
    }

    #[test]
    fn tauri_config_registers_htree_as_desktop_deep_link_scheme() {
        let config_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tauri.conf.json");
        let config = std::fs::read_to_string(&config_path).expect("failed to read tauri.conf.json");
        let json: serde_json::Value =
            serde_json::from_str(&config).expect("failed to parse tauri.conf.json");

        let schemes = json
            .pointer("/plugins/deep-link/desktop/schemes")
            .and_then(serde_json::Value::as_array)
            .expect("expected plugins.deep-link.desktop.schemes to be configured");

        assert!(
            schemes.iter().any(|value| value.as_str() == Some("htree")),
            "expected htree deep-link scheme in {:?}",
            config_path
        );
    }

    #[test]
    fn tauri_config_registers_htree_as_mobile_deep_link_scheme() {
        let config_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tauri.conf.json");
        let config = std::fs::read_to_string(&config_path).expect("failed to read tauri.conf.json");
        let json: serde_json::Value =
            serde_json::from_str(&config).expect("failed to parse tauri.conf.json");

        let mobile_domains = json
            .pointer("/plugins/deep-link/mobile")
            .and_then(serde_json::Value::as_array)
            .expect("expected plugins.deep-link.mobile to be configured");

        let has_htree_scheme = mobile_domains.iter().any(|entry| {
            entry
                .get("scheme")
                .and_then(serde_json::Value::as_array)
                .map(|schemes| schemes.iter().any(|value| value.as_str() == Some("htree")))
                .unwrap_or(false)
        });

        assert!(
            has_htree_scheme,
            "expected htree deep-link scheme in mobile config {:?}",
            config_path
        );
    }

    #[test]
    fn android_manifest_registers_htree_view_intent_filter() {
        let manifest_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("gen/android/app/src/main/AndroidManifest.xml");
        if !manifest_path.exists() {
            return;
        }
        let manifest =
            std::fs::read_to_string(&manifest_path).expect("failed to read AndroidManifest.xml");

        assert!(
            manifest.contains("android.intent.action.VIEW"),
            "expected VIEW intent filter in {:?}",
            manifest_path
        );
        assert!(
            manifest.contains("android.intent.category.BROWSABLE"),
            "expected BROWSABLE category in {:?}",
            manifest_path
        );
        assert!(
            manifest.contains("android:scheme=\"htree\""),
            "expected htree scheme in {:?}",
            manifest_path
        );
    }

    #[test]
    fn ios_info_plist_registers_htree_url_scheme() {
        let plist_path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("gen/apple/iris_iOS/Info.plist");
        if !plist_path.exists() {
            return;
        }
        let plist = std::fs::read_to_string(&plist_path).expect("failed to read iOS Info.plist");

        assert!(
            plist.contains("<key>CFBundleURLTypes</key>"),
            "expected CFBundleURLTypes in {:?}",
            plist_path
        );
        assert!(
            plist.contains("<string>htree</string>"),
            "expected htree URL scheme in {:?}",
            plist_path
        );
    }

    #[test]
    fn macos_info_plist_registers_htree_url_scheme() {
        let plist_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Info.plist");
        let plist = std::fs::read_to_string(&plist_path).expect("failed to read macOS Info.plist");

        assert!(
            plist.contains("<key>CFBundleURLTypes</key>"),
            "expected CFBundleURLTypes in {:?}",
            plist_path
        );
        assert!(
            plist.contains("<string>htree</string>"),
            "expected htree URL scheme in {:?}",
            plist_path
        );
    }

    #[test]
    fn macos_info_plist_declares_bluetooth_usage_description() {
        let plist_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Info.plist");
        let plist = std::fs::read_to_string(&plist_path).expect("failed to read macOS Info.plist");

        assert!(
            plist.contains("<key>NSBluetoothAlwaysUsageDescription</key>"),
            "expected NSBluetoothAlwaysUsageDescription in {:?}",
            plist_path
        );
    }

    #[test]
    fn ios_info_plist_declares_bluetooth_usage_description() {
        let plist_path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("gen/apple/iris_iOS/Info.plist");
        if !plist_path.exists() {
            return;
        }

        let plist = std::fs::read_to_string(&plist_path).expect("failed to read iOS Info.plist");

        assert!(
            plist.contains("<key>NSBluetoothAlwaysUsageDescription</key>"),
            "expected NSBluetoothAlwaysUsageDescription in {:?}",
            plist_path
        );
        assert!(
            plist.contains("<string>bluetooth-peripheral</string>"),
            "expected bluetooth-peripheral background mode in {:?}",
            plist_path
        );
    }

    #[test]
    fn android_mobile_bluetooth_plugin_declares_permissions_and_background_service() {
        let manifest_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("plugins/mobile-bluetooth/android/src/main/AndroidManifest.xml");
        if !manifest_path.exists() {
            return;
        }
        let manifest =
            std::fs::read_to_string(&manifest_path).expect("failed to read Android manifest");

        for required in [
            "android.permission.BLUETOOTH",
            "android.permission.BLUETOOTH_ADMIN",
            "android.permission.BLUETOOTH_CONNECT",
            "android.permission.BLUETOOTH_ADVERTISE",
            "android.hardware.bluetooth_le",
            "android.permission.FOREGROUND_SERVICE",
            "android.permission.FOREGROUND_SERVICE_CONNECTED_DEVICE",
            "MobileBluetoothForegroundService",
        ] {
            assert!(
                manifest.contains(required),
                "expected {required:?} in {:?}",
                manifest_path
            );
        }
    }

    #[test]
    fn android_mobile_bluetooth_plugin_requests_runtime_bluetooth_permissions() {
        let plugin_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
            "plugins/mobile-bluetooth/android/src/main/java/to/iris/browser/mobilebluetooth/MobileBluetoothPlugin.kt",
        );
        if !plugin_path.exists() {
            return;
        }

        let plugin =
            std::fs::read_to_string(&plugin_path).expect("failed to read Android Bluetooth plugin");

        for required in [
            "@TauriPlugin(",
            "Manifest.permission.BLUETOOTH_CONNECT",
            "Manifest.permission.BLUETOOTH_ADVERTISE",
            "requestPermissionForAlias(BLUETOOTH_PERMISSION_ALIAS, invoke, \"onBluetoothPermissionResult\")",
            "@PermissionCallback",
            "Bluetooth permission denied",
        ] {
            assert!(
                plugin.contains(required),
                "expected {required:?} in {:?}",
                plugin_path
            );
        }
    }

    #[test]
    fn mobile_bluetooth_plugins_expose_rx_descriptor_for_macos_btleplug() {
        let android_plugin_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
            "plugins/mobile-bluetooth/android/src/main/java/to/iris/browser/mobilebluetooth/MobileBluetoothPlugin.kt",
        );
        if android_plugin_path.exists() {
            let plugin = std::fs::read_to_string(&android_plugin_path)
                .expect("failed to read Android Bluetooth plugin");
            for required in [
                "USER_DESCRIPTION_UUID",
                "00002901-0000-1000-8000-00805f9b34fb",
                "rx.addDescriptor(",
            ] {
                assert!(
                    plugin.contains(required),
                    "expected {required:?} in {:?}",
                    android_plugin_path
                );
            }
        }

        let ios_plugin_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("plugins/mobile-bluetooth/ios/Sources/MobileBluetoothPlugin.swift");
        if ios_plugin_path.exists() {
            let plugin =
                std::fs::read_to_string(&ios_plugin_path).expect("failed to read iOS plugin");
            for required in [
                "userDescriptionUUID",
                "CBUUID(string: \"2901\")",
                "rx.descriptors = [",
                "CBMutableDescriptor(type: userDescriptionUUID, value: \"iris-rx\" as NSString)",
            ] {
                assert!(
                    plugin.contains(required),
                    "expected {required:?} in {:?}",
                    ios_plugin_path
                );
            }
        }
    }
}
