use crate::nip07;
use axum::{
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AutomationUiState {
    pub shell_ready: bool,
    pub current_view: String,
    pub current_url: String,
    pub address_value: String,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub show_dropdown: bool,
    pub child_webview_ready: bool,
    pub child_page_load_state: String,
    pub child_page_load_url: String,
    pub child_document_title: String,
    pub child_body_text: String,
    pub child_media_summary: String,
    pub child_last_error: String,
    pub history_index: i32,
    pub history_length: usize,
    pub window_inner_height: i32,
    pub window_outer_height: i32,
    pub toolbar_height: i32,
    pub child_bounds_top: i32,
    pub child_bounds_height: i32,
    pub child_viewport_width: i32,
    pub child_viewport_height: i32,
    pub pending_nip07_prompt_request_id: String,
    pub pending_nip07_prompt_origin: String,
    pub pending_nip07_prompt_method: String,
}

impl Default for AutomationUiState {
    fn default() -> Self {
        Self {
            shell_ready: false,
            current_view: "launcher".to_string(),
            current_url: String::new(),
            address_value: String::new(),
            can_go_back: false,
            can_go_forward: false,
            show_dropdown: false,
            child_webview_ready: false,
            child_page_load_state: "idle".to_string(),
            child_page_load_url: String::new(),
            child_document_title: String::new(),
            child_body_text: String::new(),
            child_media_summary: String::new(),
            child_last_error: String::new(),
            history_index: -1,
            history_length: 0,
            window_inner_height: 0,
            window_outer_height: 0,
            toolbar_height: 0,
            child_bounds_top: 0,
            child_bounds_height: 0,
            child_viewport_width: 0,
            child_viewport_height: 0,
            pending_nip07_prompt_request_id: String::new(),
            pending_nip07_prompt_origin: String::new(),
            pending_nip07_prompt_method: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AutomationSnapshot {
    pub enabled: bool,
    pub port: Option<u16>,
    #[serde(flatten)]
    pub ui: AutomationUiState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AutomationAction {
    OpenUrl,
    Back,
    Forward,
    Reload,
    Home,
    Settings,
    RespondNip07Prompt,
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AutomationCommand {
    pub action: AutomationAction,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub decision: Option<nip07::Nip07PermissionDecisionAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AutomationNip07ProbeRequest {
    #[serde(default = "default_child_webview_label")]
    pub label: String,
    #[serde(default)]
    pub scenario: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AutomationChildScriptRequest {
    #[serde(default = "default_child_webview_label")]
    pub label: String,
    pub script: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AutomationNip07PermissionResponse {
    pub request_id: String,
    pub decision: nip07::Nip07PermissionDecisionAction,
}

fn default_child_webview_label() -> String {
    "content".to_string()
}

pub struct AutomationState {
    enabled: bool,
    port: RwLock<Option<u16>>,
    ui: RwLock<AutomationUiState>,
}

impl AutomationState {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            port: RwLock::new(None),
            ui: RwLock::new(AutomationUiState::default()),
        }
    }

    pub fn update_ui(&self, ui: AutomationUiState) {
        *self.ui.write() = ui;
    }

    pub fn set_port(&self, port: u16) {
        *self.port.write() = Some(port);
    }

    pub fn snapshot(&self) -> AutomationSnapshot {
        AutomationSnapshot {
            enabled: self.enabled,
            port: *self.port.read(),
            ui: self.ui.read().clone(),
        }
    }
}

pub fn automation_requested() -> bool {
    parse_truthy_env("IRIS_AUTOMATION") || std::env::var("IRIS_AUTOMATION_PORT").is_ok()
}

fn parse_truthy_env(key: &str) -> bool {
    std::env::var(key)
        .ok()
        .map(|value| {
            let normalized = value.trim().to_ascii_lowercase();
            !normalized.is_empty()
                && normalized != "0"
                && normalized != "false"
                && normalized != "no"
                && normalized != "off"
        })
        .unwrap_or(false)
}

fn requested_port() -> u16 {
    std::env::var("IRIS_AUTOMATION_PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(0)
}

fn bind_host() -> String {
    std::env::var("IRIS_AUTOMATION_BIND").unwrap_or_else(|_| "127.0.0.1".to_string())
}

pub fn maybe_start_server<R: Runtime + 'static>(
    app: AppHandle<R>,
    automation: Arc<AutomationState>,
) {
    if !automation.enabled {
        return;
    }

    let bind_addr = format!("{}:{}", bind_host(), requested_port());
    let app_for_routes = app.clone();
    let automation_for_health = automation.clone();
    let automation_for_state = automation.clone();
    let app_for_child_script = app.clone();
    let app_for_nip07_probe = app.clone();
    let app_for_nip07_prompt = app.clone();
    let app_for_nip07_response = app.clone();

    tauri::async_runtime::spawn(async move {
        let listener = match tokio::net::TcpListener::bind(&bind_addr).await {
            Ok(listener) => listener,
            Err(error) => {
                warn!("[automation] failed to bind {}: {}", bind_addr, error);
                return;
            }
        };

        let port = match listener.local_addr() {
            Ok(addr) => addr.port(),
            Err(error) => {
                warn!("[automation] failed to read local addr: {}", error);
                return;
            }
        };

        automation.set_port(port);
        info!(
            "[automation] listening on http://127.0.0.1:{}/automation/state",
            port
        );

        let router = Router::new()
            .route(
                "/automation/health",
                get(move || {
                    let automation = automation_for_health.clone();
                    async move { Json(automation.snapshot()) }
                }),
            )
            .route(
                "/automation/state",
                get(move || {
                    let automation = automation_for_state.clone();
                    async move { Json(automation.snapshot()) }
                }),
            )
            .route(
                "/automation/command",
                post(move |Json(command): Json<AutomationCommand>| {
                    let app = app_for_routes.clone();
                    async move {
                        tauri::async_runtime::spawn(async move {
                            if let Err(error) = app.emit("automation-command", command) {
                                warn!("[automation] failed to emit command: {}", error);
                            }
                        });
                        Ok::<StatusCode, (StatusCode, String)>(StatusCode::ACCEPTED)
                    }
                }),
            )
            .route(
                "/automation/child-script",
                post(move |Json(request): Json<AutomationChildScriptRequest>| {
                    let app = app_for_child_script.clone();
                    async move {
                        nip07::run_webview_script(app, request.label, request.script)
                            .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
                        Ok::<StatusCode, (StatusCode, String)>(StatusCode::ACCEPTED)
                    }
                }),
            )
            .route(
                "/automation/nip07-probe",
                post(move |Json(request): Json<AutomationNip07ProbeRequest>| {
                    let app = app_for_nip07_probe.clone();
                    async move {
                        nip07::run_webview_nip07_probe(
                            app,
                            request.label,
                            request.scenario.unwrap_or_else(|| "probe".to_string()),
                        )
                        .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
                        Ok::<StatusCode, (StatusCode, String)>(StatusCode::ACCEPTED)
                    }
                }),
            )
            .route(
                "/automation/nip07-prompts",
                get(move || {
                    let app = app_for_nip07_prompt.clone();
                    async move {
                        let state = app.try_state::<Arc<nip07::Nip07State>>().ok_or_else(|| {
                            (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "NIP-07 state not initialized".to_string(),
                            )
                        })?;
                        let prompts = state.pending_permission_prompts().await;
                        Ok::<Json<Vec<nip07::Nip07PermissionPrompt>>, (StatusCode, String)>(Json(
                            prompts,
                        ))
                    }
                }),
            )
            .route(
                "/automation/nip07-prompts/respond",
                post(
                    move |Json(request): Json<AutomationNip07PermissionResponse>| {
                        let app = app_for_nip07_response.clone();
                        async move {
                            let state =
                                app.try_state::<Arc<nip07::Nip07State>>().ok_or_else(|| {
                                    (
                                        StatusCode::INTERNAL_SERVER_ERROR,
                                        "NIP-07 state not initialized".to_string(),
                                    )
                                })?;
                            state
                                .resolve_permission_prompt(&request.request_id, request.decision)
                                .await
                                .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
                            Ok::<StatusCode, (StatusCode, String)>(StatusCode::ACCEPTED)
                        }
                    },
                ),
            );

        if let Err(error) = axum::serve(listener, router).await {
            warn!("[automation] server exited with error: {}", error);
        }
    });
}

#[tauri::command]
pub fn automation_update_state<R: Runtime>(
    app: AppHandle<R>,
    snapshot: AutomationUiState,
) -> Result<(), String> {
    let automation = app
        .try_state::<Arc<AutomationState>>()
        .ok_or_else(|| "AutomationState not found".to_string())?;
    automation.update_ui(snapshot);
    Ok(())
}

#[tauri::command]
pub fn automation_get_state<R: Runtime>(app: AppHandle<R>) -> Result<AutomationSnapshot, String> {
    let automation = app
        .try_state::<Arc<AutomationState>>()
        .ok_or_else(|| "AutomationState not found".to_string())?;
    Ok(automation.snapshot())
}

#[tauri::command]
pub fn automation_shutdown<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    app.exit(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn with_env_vars<T>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env lock poisoned");

        let originals: Vec<(&str, Option<String>)> = vars
            .iter()
            .map(|(key, _)| (*key, std::env::var(key).ok()))
            .collect();

        for (key, value) in vars {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }

        let result = f();

        for (key, value) in originals {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }

        result
    }

    #[test]
    fn automation_disabled_by_default() {
        with_env_vars(
            &[("IRIS_AUTOMATION", None), ("IRIS_AUTOMATION_PORT", None)],
            || {
                assert!(!automation_requested());
            },
        );
    }

    #[test]
    fn automation_enabled_when_truthy_flag_set() {
        with_env_vars(
            &[
                ("IRIS_AUTOMATION", Some("true")),
                ("IRIS_AUTOMATION_PORT", None),
            ],
            || {
                assert!(automation_requested());
            },
        );
    }

    #[test]
    fn automation_enabled_when_port_is_configured() {
        with_env_vars(
            &[
                ("IRIS_AUTOMATION", Some("false")),
                ("IRIS_AUTOMATION_PORT", Some("4317")),
            ],
            || {
                assert!(automation_requested());
            },
        );
    }

    #[test]
    fn snapshot_tracks_updated_ui_state_and_port() {
        let automation = AutomationState::new(true);
        automation.set_port(4317);
        automation.update_ui(AutomationUiState {
            shell_ready: true,
            current_view: "webview".to_string(),
            current_url: "https://files.iris.to".to_string(),
            address_value: "files.iris.to".to_string(),
            can_go_back: true,
            can_go_forward: false,
            show_dropdown: false,
            child_webview_ready: true,
            child_page_load_state: "finished".to_string(),
            child_page_load_url: "https://files.iris.to".to_string(),
            child_document_title: "Files".to_string(),
            child_body_text: "hello".to_string(),
            child_media_summary: "thumbs=4/5 visible=3".to_string(),
            child_last_error: String::new(),
            history_index: 0,
            history_length: 1,
            window_inner_height: 720,
            window_outer_height: 800,
            toolbar_height: 48,
            child_bounds_top: 48,
            child_bounds_height: 672,
            child_viewport_width: 1280,
            child_viewport_height: 672,
            pending_nip07_prompt_request_id: String::new(),
            pending_nip07_prompt_origin: String::new(),
            pending_nip07_prompt_method: String::new(),
        });

        let snapshot = automation.snapshot();
        assert!(snapshot.enabled);
        assert_eq!(snapshot.port, Some(4317));
        assert!(snapshot.ui.shell_ready);
        assert_eq!(snapshot.ui.current_view, "webview");
        assert_eq!(snapshot.ui.current_url, "https://files.iris.to");
        assert_eq!(snapshot.ui.address_value, "files.iris.to");
        assert!(snapshot.ui.can_go_back);
        assert!(snapshot.ui.child_webview_ready);
        assert_eq!(snapshot.ui.child_page_load_state, "finished");
        assert_eq!(snapshot.ui.child_document_title, "Files");
        assert_eq!(snapshot.ui.child_media_summary, "thumbs=4/5 visible=3");
        assert_eq!(snapshot.ui.window_inner_height, 720);
        assert_eq!(snapshot.ui.child_bounds_height, 672);
        assert_eq!(snapshot.ui.child_viewport_height, 672);
    }
}
