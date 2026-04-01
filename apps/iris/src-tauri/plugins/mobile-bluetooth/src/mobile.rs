use super::{MobileBluetooth, MobileBluetoothEvent};
use serde::de::DeserializeOwned;
use tauri::{plugin::PluginApi, AppHandle, Runtime};
use tokio::sync::broadcast;

#[cfg(target_os = "android")]
const PLUGIN_IDENTIFIER: &str = "to.iris.browser.mobilebluetooth";

#[cfg(target_os = "ios")]
tauri::ios_plugin_binding!(init_plugin_mobile_bluetooth);

pub fn init<R: Runtime, C: DeserializeOwned>(
    _app: &AppHandle<R>,
    api: PluginApi<R, C>,
) -> tauri::Result<MobileBluetooth<R>> {
    #[cfg(target_os = "android")]
    let handle = api.register_android_plugin(PLUGIN_IDENTIFIER, "MobileBluetoothPlugin")?;
    #[cfg(target_os = "ios")]
    let handle = api
        .register_ios_plugin(init_plugin_mobile_bluetooth)
        .map_err(|error| {
            tauri::Error::PluginInitialization(
                "iris-mobile-bluetooth".to_string(),
                format!("register iOS plugin: {}", error),
            )
        })?;
    let (events, _) = broadcast::channel::<MobileBluetoothEvent>(256);
    Ok(MobileBluetooth { handle, events })
}
