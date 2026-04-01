use std::path::PathBuf;
use std::sync::Arc;

use crate::config::Config;
use crate::socialgraph;
use crate::webrtc::{
    BluetoothConfig, MulticastConfig, PeerClassifier, PeerPool, WebRTCConfig, WifiAwareConfig,
};

fn relay_is_loopback(relay: &str) -> bool {
    relay.contains("://127.0.0.1") || relay.contains("://localhost") || relay.contains("://[::1]")
}

pub fn peer_router_enabled(config: &Config) -> bool {
    config.server.enable_webrtc
        || (config.server.enable_multicast && config.server.max_multicast_peers > 0)
        || (config.server.enable_wifi_aware && config.server.max_wifi_aware_peers > 0)
        || (config.server.enable_bluetooth && config.server.max_bluetooth_peers > 0)
}

pub fn should_start_stun_server(config: &Config) -> bool {
    config.server.enable_webrtc && config.server.stun_port > 0
}

/// Build default WebRTC config from daemon/app config.
pub fn default_webrtc_config(config: &Config) -> WebRTCConfig {
    let active_relays = config.nostr.active_relays();
    let local_only_relays =
        !active_relays.is_empty() && active_relays.iter().all(|relay| relay_is_loopback(relay));
    let relays = if config.server.enable_webrtc {
        active_relays
    } else {
        Vec::new()
    };
    let stun_servers =
        if !config.server.enable_webrtc || (config.server.enable_multicast && local_only_relays) {
            Vec::new()
        } else {
            WebRTCConfig::default().stun_servers
        };

    WebRTCConfig {
        relays,
        signaling_enabled: config.server.enable_webrtc,
        stun_servers,
        multicast: MulticastConfig {
            enabled: config.server.enable_multicast,
            group: config.server.multicast_group.clone(),
            port: config.server.multicast_port,
            max_peers: config.server.max_multicast_peers,
            ..Default::default()
        },
        wifi_aware: WifiAwareConfig {
            enabled: config.server.enable_wifi_aware,
            max_peers: config.server.max_wifi_aware_peers,
            ..Default::default()
        },
        bluetooth: BluetoothConfig {
            enabled: config.server.enable_bluetooth,
            max_peers: config.server.max_bluetooth_peers,
        },
        ..Default::default()
    }
}

/// Build peer classifier used by daemon/runtime startup paths.
pub fn build_peer_classifier(
    data_dir: PathBuf,
    store: Arc<dyn socialgraph::SocialGraphBackend>,
) -> PeerClassifier {
    let contacts_file = data_dir.join("contacts.json");
    Arc::new(move |pubkey_hex: &str| {
        if contacts_file.exists() {
            if let Ok(data) = std::fs::read_to_string(&contacts_file) {
                if let Ok(contacts) = serde_json::from_str::<Vec<String>>(&data) {
                    if contacts.contains(&pubkey_hex.to_string()) {
                        return PeerPool::Follows;
                    }
                }
            }
        }
        if let Ok(pk_bytes) = hex::decode(pubkey_hex) {
            if pk_bytes.len() == 32 {
                let pk: [u8; 32] = pk_bytes.try_into().unwrap();
                if let Some(dist) = socialgraph::get_follow_distance(store.as_ref(), &pk) {
                    if dist <= 2 {
                        return PeerPool::Follows;
                    }
                }
            }
        }
        PeerPool::Other
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_webrtc_config_disables_stun_for_loopback_only_multicast() {
        let mut config = Config::default();
        config.server.enable_multicast = true;
        config.server.max_multicast_peers = 4;
        config.nostr.relays = vec!["ws://127.0.0.1:8080/ws".to_string()];

        let webrtc = default_webrtc_config(&config);
        assert!(webrtc.stun_servers.is_empty());
    }

    #[test]
    fn default_webrtc_config_keeps_stun_for_non_loopback_relays() {
        let mut config = Config::default();
        config.server.enable_multicast = true;
        config.server.max_multicast_peers = 4;
        config.nostr.relays = vec!["wss://relay.example".to_string()];

        let webrtc = default_webrtc_config(&config);
        assert!(!webrtc.stun_servers.is_empty());
    }

    #[test]
    fn default_webrtc_config_maps_bluetooth_limits() {
        let mut config = Config::default();
        config.server.enable_bluetooth = true;
        config.server.max_bluetooth_peers = 3;

        let webrtc = default_webrtc_config(&config);
        assert!(webrtc.signaling_enabled);
        assert!(webrtc.bluetooth.enabled);
        assert_eq!(webrtc.bluetooth.max_peers, 3);
    }

    #[test]
    fn default_webrtc_config_maps_wifi_aware_limits() {
        let mut config = Config::default();
        config.server.enable_wifi_aware = true;
        config.server.max_wifi_aware_peers = 4;

        let webrtc = default_webrtc_config(&config);
        assert!(webrtc.signaling_enabled);
        assert!(webrtc.wifi_aware.enabled);
        assert_eq!(webrtc.wifi_aware.max_peers, 4);
    }

    #[test]
    fn default_webrtc_config_strips_relays_and_stun_when_webrtc_disabled() {
        let mut config = Config::default();
        config.server.enable_webrtc = false;
        config.server.enable_bluetooth = true;
        config.server.max_bluetooth_peers = 2;
        config.server.stun_port = 3478;
        config.nostr.relays = vec!["wss://relay.example".to_string()];

        let webrtc = default_webrtc_config(&config);
        assert!(!webrtc.signaling_enabled);
        assert!(webrtc.relays.is_empty());
        assert!(webrtc.stun_servers.is_empty());
    }

    #[test]
    fn stun_server_only_starts_when_webrtc_is_enabled() {
        let mut config = Config::default();
        config.server.stun_port = 3478;

        assert!(should_start_stun_server(&config));

        config.server.enable_webrtc = false;
        assert!(!should_start_stun_server(&config));
    }

    #[test]
    fn peer_router_enabled_for_wifi_aware_only() {
        let mut config = Config::default();
        config.server.enable_webrtc = false;
        config.server.enable_wifi_aware = true;
        config.server.max_wifi_aware_peers = 2;

        assert!(peer_router_enabled(&config));

        config.server.max_wifi_aware_peers = 0;
        assert!(!peer_router_enabled(&config));
    }
}
