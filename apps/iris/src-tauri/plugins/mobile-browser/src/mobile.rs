use super::{
    BrowserBoundsRequest, BrowserCreateRequest, BrowserHistoryRequest, BrowserLabelRequest,
    BrowserNavigateRequest, NativeCurrentUrlResponse, ShellOverlayRequest, UrlMapping,
};
use serde::de::DeserializeOwned;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tauri::{
    plugin::{PluginApi, PluginHandle},
    AppHandle, Runtime,
};

#[cfg(target_os = "android")]
const PLUGIN_IDENTIFIER: &str = "to.iris.browser.mobile";

#[cfg(target_os = "ios")]
tauri::ios_plugin_binding!(init_plugin_mobile_browser);

#[derive(Debug)]
pub struct MobileBrowser<R: Runtime> {
    pub(crate) handle: PluginHandle<R>,
    mappings: Arc<Mutex<HashMap<String, UrlMapping>>>,
}

impl<R: Runtime> Clone for MobileBrowser<R> {
    fn clone(&self) -> Self {
        Self {
            handle: self.handle.clone(),
            mappings: self.mappings.clone(),
        }
    }
}

impl<R: Runtime> MobileBrowser<R> {
    pub(crate) fn mappings(&self) -> Arc<Mutex<HashMap<String, UrlMapping>>> {
        self.mappings.clone()
    }

    pub fn create(&self, request: BrowserCreateRequest) -> Result<(), String> {
        let label = request.label.clone();
        self.mappings.lock().unwrap().insert(
            label.clone(),
            UrlMapping {
                actual_url_root: request.actual_url_root.clone(),
                canonical_url_root: request.canonical_url_root.clone(),
                server_url: request.server_url.clone(),
                session_token: request.session_token.clone(),
            },
        );

        if let Err(error) = self.handle.run_mobile_plugin::<()>("create", request) {
            self.mappings.lock().unwrap().remove(&label);
            return Err(error.to_string());
        }

        Ok(())
    }

    pub fn close(&self, label: String) -> Result<(), String> {
        self.mappings.lock().unwrap().remove(&label);
        self.handle
            .run_mobile_plugin::<()>("close", BrowserLabelRequest { label })
            .map_err(|error| error.to_string())
    }

    pub fn navigate(&self, label: String, url: String) -> Result<(), String> {
        let mapped_url = {
            let mapping = self
                .mappings
                .lock()
                .unwrap()
                .get(&label)
                .cloned()
                .unwrap_or_default();
            if url.starts_with("htree://") {
                super::materialize_htree_navigation_url(&url, &mapping).ok_or_else(|| {
                    format!("htree navigation for {label} requires recreating the webview")
                })?
            } else {
                url
            }
        };

        self.handle
            .run_mobile_plugin::<()>(
                "navigate",
                BrowserNavigateRequest {
                    label,
                    url: mapped_url,
                },
            )
            .map_err(|error| error.to_string())
    }

    pub fn set_bounds(&self, request: BrowserBoundsRequest) -> Result<(), String> {
        self.handle
            .run_mobile_plugin::<()>("setBounds", request)
            .map_err(|error| error.to_string())
    }

    pub fn set_shell_overlay(&self, request: ShellOverlayRequest) -> Result<(), String> {
        self.handle
            .run_mobile_plugin::<()>("setShellOverlay", request)
            .map_err(|error| error.to_string())
    }

    pub fn history(&self, label: String, direction: String) -> Result<(), String> {
        self.handle
            .run_mobile_plugin::<()>("history", BrowserHistoryRequest { label, direction })
            .map_err(|error| error.to_string())
    }

    pub fn reload(&self, label: String) -> Result<(), String> {
        self.handle
            .run_mobile_plugin::<()>("reload", BrowserLabelRequest { label })
            .map_err(|error| error.to_string())
    }

    pub fn current_url(&self, label: String) -> Result<String, String> {
        let response = self
            .handle
            .run_mobile_plugin::<NativeCurrentUrlResponse>(
                "currentUrl",
                BrowserLabelRequest {
                    label: label.clone(),
                },
            )
            .map_err(|error| error.to_string())?;

        let url = response
            .url
            .ok_or_else(|| "Webview URL is not available".to_string())?;
        let mapping = self
            .mappings
            .lock()
            .unwrap()
            .get(&label)
            .cloned()
            .unwrap_or_default();

        Ok(super::canonicalize_url(&url, &mapping))
    }
}

pub fn init<R: Runtime, C: DeserializeOwned>(
    _app: &AppHandle<R>,
    api: PluginApi<R, C>,
) -> tauri::Result<MobileBrowser<R>> {
    #[cfg(target_os = "android")]
    let handle = api.register_android_plugin(PLUGIN_IDENTIFIER, "MobileBrowserPlugin")?;
    #[cfg(target_os = "ios")]
    let handle = api
        .register_ios_plugin(init_plugin_mobile_browser)
        .map_err(|error| super::mobile_plugin_setup_error("register iOS plugin", error))?;

    Ok(MobileBrowser {
        handle,
        mappings: Arc::new(Mutex::new(HashMap::new())),
    })
}
