use git_remote_htree::nostr_client::resolve_identity;
use nostr_sdk::PublicKey;
use tempfile::TempDir;

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

#[test]
fn test_resolve_identity_petname_from_npub_alias() {
    let home = TempDir::new().expect("create temp home");
    let config_dir = home.path().join(".hashtree");
    std::fs::create_dir_all(&config_dir).expect("create .hashtree dir");

    let npub = "npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm";
    std::fs::write(config_dir.join("keys"), format!("{npub} sirius\n")).expect("write keys file");

    let _home_guard = HomeGuard::set(home.path());

    let (pubkey, secret) = resolve_identity("sirius").expect("resolve npub alias");
    let expected = hex::encode(PublicKey::parse(npub).expect("parse npub").to_bytes());

    assert_eq!(pubkey, expected);
    assert!(secret.is_none(), "npub aliases should stay read-only");
    assert!(
        config_dir.join("aliases").exists(),
        "aliases hint file should be created alongside keys"
    );
}
