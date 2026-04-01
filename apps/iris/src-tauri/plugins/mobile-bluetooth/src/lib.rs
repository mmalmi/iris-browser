#[cfg(any(target_os = "android", target_os = "ios"))]
use base64::Engine;
#[cfg(any(target_os = "android", target_os = "ios"))]
use serde::{Deserialize, Serialize};
#[cfg(any(target_os = "android", target_os = "ios"))]
use tauri::ipc::Channel;
#[cfg(any(target_os = "android", target_os = "ios"))]
use tauri::ipc::InvokeResponseBody;
use tauri::{
    plugin::{Builder, TauriPlugin},
    Runtime,
};
#[cfg(any(target_os = "android", target_os = "ios"))]
use tauri::{AppHandle, Manager};
use tokio::sync::broadcast;

#[cfg(any(target_os = "android", target_os = "ios"))]
mod mobile;

#[cfg(any(target_os = "android", target_os = "ios"))]
pub use mobile::init as mobile_init;

#[derive(Debug, Clone)]
pub enum MobileBluetoothEvent {
    PeerConnected {
        address: String,
    },
    PeerReady {
        address: String,
    },
    PeerDisconnected {
        address: String,
    },
}

pub struct MobileBluetooth<R: Runtime> {
    pub(crate) handle: tauri::plugin::PluginHandle<R>,
    pub(crate) events: broadcast::Sender<MobileBluetoothEvent>,
}

impl<R: Runtime> Clone for MobileBluetooth<R> {
    fn clone(&self) -> Self {
        Self {
            handle: self.handle.clone(),
            events: self.events.clone(),
        }
    }
}

#[cfg(any(target_os = "android", target_os = "ios"))]
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct StartArgs {
    local_peer_id: String,
}

#[cfg(any(target_os = "android", target_os = "ios"))]
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SendArgs {
    address: String,
    kind: String,
    payload_base64: String,
}

#[cfg(any(target_os = "android", target_os = "ios"))]
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RegisterListenerRequest {
    event: String,
    handler: Channel,
}

#[cfg(any(target_os = "android", target_os = "ios"))]
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddressPayload {
    address: String,
}

#[cfg(any(target_os = "android", target_os = "ios"))]
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerSnapshot {
    pub address: String,
    pub ready: bool,
}

#[cfg(any(target_os = "android", target_os = "ios"))]
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DrainedFrame {
    pub address: String,
    pub kind: String,
    pub payload_base64: String,
}

#[cfg(any(target_os = "android", target_os = "ios"))]
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransportPoll {
    pub peers: Vec<PeerSnapshot>,
    pub frames: Vec<DrainedFrame>,
}

#[cfg(any(target_os = "android", target_os = "ios"))]
fn decode_channel_payload<T: for<'de> Deserialize<'de>>(
    body: InvokeResponseBody,
) -> tauri::Result<T> {
    match body {
        InvokeResponseBody::Json(payload) => {
            serde_json::from_str::<T>(&payload).map_err(Into::into)
        }
        InvokeResponseBody::Raw(payload) => {
            serde_json::from_slice::<T>(&payload).map_err(Into::into)
        }
    }
}

#[cfg(any(target_os = "android", target_os = "ios"))]
fn register_native_listeners<R: Runtime>(
    app: &AppHandle<R>,
    bluetooth: &MobileBluetooth<R>,
) -> tauri::Result<()> {
    let tx = bluetooth.events.clone();
    let handle = bluetooth.handle.clone();
    for event_name in ["peer-connected", "peer-ready", "peer-disconnected"] {
        let tx = tx.clone();
        handle.run_mobile_plugin::<()>(
            "registerListener",
            RegisterListenerRequest {
                event: event_name.to_string(),
                handler: Channel::new(move |body| {
                    match event_name {
                        "peer-connected" => {
                            let payload = decode_channel_payload::<AddressPayload>(body)?;
                            let _ = tx.send(MobileBluetoothEvent::PeerConnected {
                                address: payload.address,
                            });
                        }
                        "peer-ready" => {
                            let payload = decode_channel_payload::<AddressPayload>(body)?;
                            let _ = tx.send(MobileBluetoothEvent::PeerReady {
                                address: payload.address,
                            });
                        }
                        "peer-disconnected" => {
                            let payload = decode_channel_payload::<AddressPayload>(body)?;
                            let _ = tx.send(MobileBluetoothEvent::PeerDisconnected {
                                address: payload.address,
                            });
                        }
                        _ => {}
                    }
                    Ok(())
                }),
            },
        )
        .map_err(|error| {
            tauri::Error::PluginInitialization(
                "iris-mobile-bluetooth".to_string(),
                format!("register native listener {event_name}: {error}"),
            )
        })?;
    }
    let _ = app;
    Ok(())
}

impl<R: Runtime> MobileBluetooth<R> {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    pub fn start(&self, local_peer_id: String) -> Result<(), String> {
        self.handle
            .run_mobile_plugin::<()>("start", StartArgs { local_peer_id })
            .map_err(|error| error.to_string())
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn start(&self, _local_peer_id: String) -> Result<(), String> {
        Err("mobile bluetooth is only available on Android and iOS".to_string())
    }

    #[cfg(any(target_os = "android", target_os = "ios"))]
    pub fn stop(&self) -> Result<(), String> {
        self.handle
            .run_mobile_plugin::<()>("stop", serde_json::json!({}))
            .map_err(|error| error.to_string())
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn stop(&self) -> Result<(), String> {
        Ok(())
    }

    #[cfg(any(target_os = "android", target_os = "ios"))]
    pub fn send_frame(
        &self,
        address: String,
        kind: String,
        payload: Vec<u8>,
    ) -> Result<(), String> {
        self.handle
            .run_mobile_plugin::<()>(
                "send",
                SendArgs {
                    address,
                    kind,
                    payload_base64: base64::engine::general_purpose::STANDARD.encode(payload),
                },
            )
            .map_err(|error| error.to_string())
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn send_frame(
        &self,
        _address: String,
        _kind: String,
        _payload: Vec<u8>,
    ) -> Result<(), String> {
        Err("mobile bluetooth is only available on Android and iOS".to_string())
    }

    #[cfg(any(target_os = "android", target_os = "ios"))]
    pub fn poll_transport(&self) -> Result<TransportPoll, String> {
        self.handle
            .run_mobile_plugin::<TransportPoll>("pollTransport", serde_json::json!({}))
            .map_err(|error| error.to_string())
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn poll_transport(&self) -> Result<Vec<()>, String> {
        Ok(Vec::new())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<MobileBluetoothEvent> {
        self.events.subscribe()
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("iris-mobile-bluetooth").build()
}

#[cfg(any(target_os = "android", target_os = "ios"))]
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("iris-mobile-bluetooth")
        .setup(|app, api| {
            let bluetooth = mobile::init(app, api)?;
            register_native_listeners(app, &bluetooth)?;
            app.manage(bluetooth);
            Ok(())
        })
        .build()
}

#[cfg(any(target_os = "android", target_os = "ios"))]
pub trait MobileBluetoothExt<R: Runtime> {
    fn mobile_bluetooth(&self) -> &MobileBluetooth<R>;
}

#[cfg(any(target_os = "android", target_os = "ios"))]
impl<R: Runtime, T: Manager<R>> MobileBluetoothExt<R> for T {
    fn mobile_bluetooth(&self) -> &MobileBluetooth<R> {
        self.state::<MobileBluetooth<R>>().inner()
    }
}
