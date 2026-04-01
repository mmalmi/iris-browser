use super::daemonize::{build_daemon_args, parse_pid, read_pid_file, write_pid_file};
use super::lists::{
    build_mute_list_event, load_mute_entries, update_hex_list_file,
    update_mute_list_file_with_status, MuteEntry, MuteUpdate,
};
use super::resolve::{parse_published_target, resolve_cid_input, ParsedPublishedTarget};
use super::run::{
    build_files_iris_to_url_for_add_route, build_files_iris_to_url_for_published_ref,
    build_files_iris_to_url_for_published_target, build_sites_iris_to_url_for_add_route,
    build_sites_iris_to_url_for_published_ref, detect_site_entry_for_path, format_cid_for_display,
};
use crate::app::args::{CashuCommands, CashuMintCommands, ReleaseCommands, SocialGraphCommands};
use crate::app::args::{Cli, Commands};
use clap::{CommandFactory, Parser};
use hashtree_core::{nhash_decode, Cid};
use nostr::Kind;
use std::path::PathBuf;

fn args_to_strings(args: Vec<std::ffi::OsString>) -> Vec<String> {
    args.into_iter()
        .map(|arg| arg.to_string_lossy().to_string())
        .collect()
}

#[test]
fn test_build_daemon_args_with_overrides() {
    let data_dir = PathBuf::from("data-dir");
    let args = args_to_strings(build_daemon_args(
        "127.0.0.1:8080",
        Some("wss://relay.example"),
        Some(&data_dir),
    ));

    assert_eq!(
        args,
        vec![
            "--addr",
            "127.0.0.1:8080",
            "--relays",
            "wss://relay.example",
            "--data-dir",
            "data-dir",
        ]
    );
}

#[test]
fn test_build_daemon_args_minimal() {
    let args = args_to_strings(build_daemon_args("0.0.0.0:8080", None, None));
    assert_eq!(args, vec!["--addr", "0.0.0.0:8080"]);
}

#[test]
fn test_parse_pid() {
    assert_eq!(parse_pid("123\n").unwrap(), 123);
    assert!(parse_pid("").is_err());
    assert!(parse_pid("abc").is_err());
}

#[test]
fn test_pid_file_roundtrip() {
    let temp_dir = tempfile::tempdir().unwrap();
    let path = temp_dir.path().join("htree.pid");
    write_pid_file(&path, 42).unwrap();
    let pid = read_pid_file(&path).unwrap();
    assert_eq!(pid, 42);
}

#[test]
fn test_update_hex_list_file_add_remove() {
    let temp_dir = tempfile::tempdir().unwrap();
    let path = temp_dir.path().join("mutes.json");
    let pk1 = "aa".repeat(32);
    let pk2 = "bb".repeat(32);

    let list = update_hex_list_file(&path, &pk1, true).unwrap();
    assert_eq!(list, vec![pk1.clone()]);

    let list = update_hex_list_file(&path, &pk1, true).unwrap();
    assert_eq!(list, vec![pk1.clone()]);

    let list = update_hex_list_file(&path, &pk2, true).unwrap();
    assert_eq!(list, vec![pk1.clone(), pk2.clone()]);

    let list = update_hex_list_file(&path, &pk1, false).unwrap();
    assert_eq!(list, vec![pk2.clone()]);
}

#[test]
fn test_build_mute_list_event_tags() {
    let keys = nostr::Keys::generate();
    let pk1 = nostr::Keys::generate().public_key().to_hex();
    let pk2 = nostr::Keys::generate().public_key().to_hex();
    let list = vec![
        MuteEntry {
            pubkey: pk1.clone(),
            reason: Some("spam".to_string()),
        },
        MuteEntry {
            pubkey: pk2.clone(),
            reason: None,
        },
    ];
    let event = build_mute_list_event(&list, &keys).unwrap();

    assert_eq!(event.kind, Kind::Custom(10000));

    let tags: Vec<String> = event
        .tags
        .iter()
        .filter_map(|tag| {
            let slice = tag.as_slice();
            if slice.first().map(|v| v.as_str()) == Some("p") {
                slice.get(1).cloned()
            } else {
                None
            }
        })
        .collect();

    assert_eq!(tags.len(), 2);
    assert!(tags.contains(&pk1));
    assert!(tags.contains(&pk2));

    let reason_tag = event
        .tags
        .iter()
        .find(|tag| tag.as_slice().get(1).map(|v| v.as_str()) == Some(pk1.as_str()))
        .expect("reason tag missing");
    assert_eq!(
        reason_tag.as_slice().get(2).map(|v| v.as_str()),
        Some("spam")
    );
}

#[test]
fn test_update_mute_list_with_reason() {
    let temp_dir = tempfile::tempdir().unwrap();
    let path = temp_dir.path().join("mutes.json");
    let pk1 = "aa".repeat(32);
    let pk2 = "bb".repeat(32);

    let (list, update) =
        update_mute_list_file_with_status(&path, &pk1, Some("spam"), true).unwrap();
    assert_eq!(update, MuteUpdate::Added);
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].reason.as_deref(), Some("spam"));

    let (list, update) =
        update_mute_list_file_with_status(&path, &pk1, Some("abuse"), true).unwrap();
    assert_eq!(update, MuteUpdate::Updated);
    assert_eq!(list[0].reason.as_deref(), Some("abuse"));

    let (_list, update) = update_mute_list_file_with_status(&path, &pk2, None, true).unwrap();
    assert_eq!(update, MuteUpdate::Added);

    let (list, update) = update_mute_list_file_with_status(&path, &pk1, None, false).unwrap();
    assert_eq!(update, MuteUpdate::Removed);
    assert_eq!(list.len(), 1);
}

#[test]
fn test_load_mute_entries_legacy_format() {
    let temp_dir = tempfile::tempdir().unwrap();
    let path = temp_dir.path().join("mutes.json");
    let pk1 = "aa".repeat(32);
    let pk2 = "bb".repeat(32);
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&vec![pk1.clone(), pk2.clone()]).unwrap(),
    )
    .unwrap();

    let entries = load_mute_entries(&path).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].pubkey, pk1);
    assert_eq!(entries[0].reason, None);
}

#[test]
fn test_format_cid_for_display_preserves_decrypt_key() {
    let cid = Cid {
        hash: [0x11; 32],
        key: Some([0x22; 32]),
    };

    let rendered = format_cid_for_display(&cid);
    let decoded = nhash_decode(&rendered).expect("decode rendered nhash");

    assert_eq!(decoded.hash, cid.hash);
    assert_eq!(decoded.decrypt_key, cid.key);
}

#[test]
fn test_build_files_iris_to_url_for_add_route_encodes_path_segments() {
    assert_eq!(
        build_files_iris_to_url_for_add_route("nhash1example/My notes/index.html"),
        "https://files.iris.to/#/nhash1example/My%20notes/index.html"
    );
}

#[test]
fn test_build_files_iris_to_url_for_published_ref_encodes_tree_name_as_single_segment() {
    assert_eq!(
        build_files_iris_to_url_for_published_ref("npub1owner", "apps/iris ui",),
        "https://files.iris.to/#/npub1owner/apps%2Firis%20ui"
    );
}

#[test]
fn test_build_files_iris_to_url_for_published_target_includes_path_and_link_key() {
    assert_eq!(
        build_files_iris_to_url_for_published_target(
            "npub1owner",
            "apps/iris ui",
            Some("docs/Read me.md"),
            Some("001122"),
        ),
        "https://files.iris.to/#/npub1owner/apps%2Firis%20ui/docs/Read%20me.md?k=001122"
    );
}

#[test]
fn test_build_sites_iris_to_url_for_add_route_encodes_path_segments() {
    assert_eq!(
        build_sites_iris_to_url_for_add_route("nhash1example/My notes/index.html"),
        "https://sites.iris.to/#/nhash1example/My%20notes/index.html"
    );
}

#[test]
fn test_build_sites_iris_to_url_for_published_ref_enables_auto_reload() {
    assert_eq!(
        build_sites_iris_to_url_for_published_ref("npub1owner", "apps/iris ui", "index.html"),
        "https://sites.iris.to/#/npub1owner/apps%2Firis%20ui/index.html?reload=1"
    );
}

#[test]
fn test_parse_published_target_decodes_slash_containing_tree_names() {
    assert_eq!(
        parse_published_target(
            "htree://npub1owner/releases%2Fnostr-vpn/v0.3.0/assets/nostr-vpn-v0.3.0-macos-arm64.zip",
        ),
        Some(ParsedPublishedTarget {
            npub: "npub1owner".to_string(),
            tree_name: "releases/nostr-vpn".to_string(),
            path: Some("v0.3.0/assets/nostr-vpn-v0.3.0-macos-arm64.zip".to_string()),
        })
    );
}

#[test]
fn test_detect_site_entry_for_path_finds_html_file() {
    let temp_dir = tempfile::tempdir().unwrap();
    let html_path = temp_dir.path().join("Landing.HTM");
    std::fs::write(&html_path, "<!doctype html>").unwrap();

    assert_eq!(
        detect_site_entry_for_path(&html_path, false),
        Some("Landing.HTM".to_string())
    );
}

#[test]
fn test_detect_site_entry_for_path_finds_directory_index_file() {
    let temp_dir = tempfile::tempdir().unwrap();
    std::fs::write(temp_dir.path().join("INDEX.HTML"), "<!doctype html>").unwrap();
    std::fs::write(temp_dir.path().join("notes.txt"), "not a site").unwrap();

    assert_eq!(
        detect_site_entry_for_path(temp_dir.path(), true),
        Some("INDEX.HTML".to_string())
    );
}

#[test]
fn test_detect_site_entry_for_path_skips_non_site_targets() {
    let temp_dir = tempfile::tempdir().unwrap();
    let text_path = temp_dir.path().join("notes.txt");
    std::fs::write(&text_path, "hello").unwrap();

    assert_eq!(detect_site_entry_for_path(&text_path, false), None);
    assert_eq!(detect_site_entry_for_path(temp_dir.path(), true), None);
}

#[test]
fn test_cli_parses_cashu_topup_and_mint_commands() {
    let cli = Cli::parse_from([
        "htree",
        "cashu",
        "topup",
        "123",
        "--mint",
        "https://mint.example",
    ]);
    match cli.command {
        Commands::Cashu {
            command: CashuCommands::Topup { amount_sat, mint },
        } => {
            assert_eq!(amount_sat, 123);
            assert_eq!(mint.as_deref(), Some("https://mint.example"));
        }
        _ => panic!("expected cashu topup command"),
    }

    let cli = Cli::parse_from([
        "htree",
        "cashu",
        "mint",
        "add",
        "https://mint.example",
        "--default",
    ]);
    match cli.command {
        Commands::Cashu {
            command:
                CashuCommands::Mint {
                    command: CashuMintCommands::Add { url, make_default },
                },
        } => {
            assert_eq!(url, "https://mint.example");
            assert!(make_default);
        }
        _ => panic!("expected cashu mint add command"),
    }
}

#[test]
fn test_cli_parses_release_publish_command() {
    let cli = Cli::parse_from([
        "htree",
        "release",
        "publish",
        "releases/hashtree",
        "releases/v0.2.3",
        "nhash1qqsq9qxpq9qcrsszg2pvxq6rs0zqg3yyc5fc5z0knh0wlh",
        "--local",
    ]);

    match cli.command {
        Commands::Release {
            command:
                ReleaseCommands::Publish {
                    tree_name,
                    version_path,
                    cid,
                    local,
                },
        } => {
            assert_eq!(tree_name, "releases/hashtree");
            assert_eq!(version_path, "releases/v0.2.3");
            assert_eq!(cid, "nhash1qqsq9qxpq9qcrsszg2pvxq6rs0zqg3yyc5fc5z0knh0wlh");
            assert!(local);
        }
        _ => panic!("expected release publish command"),
    }
}

#[cfg(feature = "fuse")]
#[test]
fn test_cli_parses_mount_command_without_explicit_mountpoint() {
    let cli = Cli::parse_from(["htree", "mount", "htree://self/mydir"]);

    match cli.command {
        Commands::Mount {
            target, mountpoint, ..
        } => {
            assert_eq!(target, "htree://self/mydir");
            assert_eq!(mountpoint, None);
        }
        _ => panic!("expected mount command"),
    }
}

#[test]
fn test_cli_help_groups_commands_by_purpose() {
    let mut cmd = Cli::command();
    let help = cmd.render_long_help().to_string();

    assert!(help.contains("Daemon Commands:"));
    assert!(help.contains("Content Commands:"));
    assert!(help.contains("Storage Commands:"));
    assert!(help.contains("Publishing & Git Commands:"));
    assert!(help.contains("Identity & Social Commands:"));
    assert!(help.contains("Wallet Commands:"));
    assert!(help.contains("General Commands:"));
    assert!(!help.contains("\nCommands:\n"));
}

#[test]
fn test_cli_parses_repos_command_default_owner() {
    let cli = Cli::parse_from(["htree", "repos"]);

    match cli.command {
        Commands::Repos { owner } => {
            assert_eq!(owner, None);
        }
        _ => panic!("expected repos command"),
    }
}

#[test]
fn test_cli_parses_repos_command_with_owner() {
    let cli = Cli::parse_from(["htree", "repos", "coworker"]);

    match cli.command {
        Commands::Repos { owner } => {
            assert_eq!(owner.as_deref(), Some("coworker"));
        }
        _ => panic!("expected repos command"),
    }
}

#[test]
fn test_cli_parses_socialgraph_index_command() {
    let cli = Cli::parse_from([
        "htree",
        "socialgraph",
        "index",
        "--warm-secs",
        "15",
        "--crawl-depth",
        "2",
        "--full-graph-recrawl",
        "--max-follow-distance",
        "2",
        "--max-authors",
        "48",
        "--max-live-mb",
        "128",
        "--per-author-event-limit",
        "64",
        "--per-author-live-bytes",
        "65536",
        "--author-batch-size",
        "32",
        "--concurrent-batches",
        "6",
        "--fetch-timeout-secs",
        "7",
        "--relay-event-max-bytes",
        "262144",
        "--global-relay-scan",
        "--author-allowlist-url",
        "https://graph-api.iris.to/allowlist?maxDistance=6",
        "--negentropy-only",
        "--relay-page-size",
        "2000",
        "--max-relay-pages",
        "6",
        "--max-events-seen",
        "1000000",
        "--kind",
        "1",
        "--kind",
        "6",
        "--relay",
        "wss://relay.example",
        "--relay",
        "wss://relay.two",
    ]);

    match cli.command {
        Commands::Socialgraph {
            command:
                SocialGraphCommands::Index {
                    warm_secs,
                    crawl_depth,
                    full_graph_recrawl,
                    max_follow_distance,
                    max_authors,
                    max_live_mb,
                    per_author_event_limit,
                    per_author_live_bytes,
                    author_batch_size,
                    concurrent_batches,
                    fetch_timeout_secs,
                    relay_event_max_bytes,
                    global_relay_scan,
                    author_allowlist_url,
                    negentropy_only,
                    relay_page_size,
                    max_relay_pages,
                    max_events_seen,
                    kinds,
                    relays,
                },
        } => {
            assert_eq!(warm_secs, 15);
            assert_eq!(crawl_depth, Some(2));
            assert!(full_graph_recrawl);
            assert_eq!(max_follow_distance, Some(2));
            assert_eq!(max_authors, 48);
            assert_eq!(max_live_mb, 128);
            assert_eq!(per_author_event_limit, 64);
            assert_eq!(per_author_live_bytes, Some(65_536));
            assert_eq!(author_batch_size, 32);
            assert_eq!(concurrent_batches, 6);
            assert_eq!(fetch_timeout_secs, 7);
            assert_eq!(relay_event_max_bytes, Some(262_144));
            assert!(global_relay_scan);
            assert_eq!(
                author_allowlist_url.as_deref(),
                Some("https://graph-api.iris.to/allowlist?maxDistance=6")
            );
            assert!(negentropy_only);
            assert_eq!(relay_page_size, 2_000);
            assert_eq!(max_relay_pages, 6);
            assert_eq!(max_events_seen, Some(1_000_000));
            assert_eq!(kinds, vec![1, 6]);
            assert_eq!(
                relays,
                vec![
                    "wss://relay.example".to_string(),
                    "wss://relay.two".to_string()
                ]
            );
        }
        _ => panic!("expected socialgraph index command"),
    }
}

#[test]
fn test_cli_add_uses_unencrypted_flag_with_public_alias() {
    let cli = Cli::parse_from(["htree", "add", "site", "--unencrypted"]);
    match cli.command {
        Commands::Add { unencrypted, .. } => assert!(unencrypted),
        _ => panic!("expected add command"),
    }

    let cli = Cli::parse_from(["htree", "add", "site", "--public"]);
    match cli.command {
        Commands::Add { unencrypted, .. } => assert!(unencrypted),
        _ => panic!("expected add command"),
    }
}

#[test]
fn test_cli_parses_socialgraph_rebuild_profile_index_command() {
    let cli = Cli::parse_from(["htree", "socialgraph", "rebuild-profile-index"]);

    match cli.command {
        Commands::Socialgraph {
            command: SocialGraphCommands::RebuildProfileIndex,
        } => {}
        _ => panic!("expected socialgraph rebuild-profile-index command"),
    }
}

#[test]
fn test_cli_parses_socialgraph_warm_command() {
    let cli = Cli::parse_from([
        "htree",
        "socialgraph",
        "warm",
        "--secs",
        "90",
        "--crawl-depth",
        "4",
        "--full-graph-recrawl",
        "--relay",
        "wss://relay.example",
        "--author-batch-size",
        "128",
        "--concurrent-batches",
        "5",
    ]);

    match cli.command {
        Commands::Socialgraph {
            command:
                SocialGraphCommands::Warm {
                    secs,
                    crawl_depth,
                    full_graph_recrawl,
                    relays,
                    author_batch_size,
                    concurrent_batches,
                },
        } => {
            assert_eq!(secs, 90);
            assert_eq!(crawl_depth, Some(4));
            assert!(full_graph_recrawl);
            assert_eq!(relays, vec!["wss://relay.example".to_string()]);
            assert_eq!(author_batch_size, 128);
            assert_eq!(concurrent_batches, 5);
        }
        _ => panic!("expected socialgraph warm command"),
    }
}

#[test]
fn test_cli_parses_socialgraph_stats_command() {
    let cli = Cli::parse_from(["htree", "socialgraph", "stats"]);

    match cli.command {
        Commands::Socialgraph {
            command: SocialGraphCommands::Stats,
        } => {}
        _ => panic!("expected socialgraph stats command"),
    }
}

#[tokio::test]
async fn test_resolve_nhash_with_path_suffix() {
    // nhash for hash [0xaa; 32]
    let nhash = hashtree_core::nhash_encode(&[0xaa; 32]).unwrap();

    // Test nhash without path
    let resolved = resolve_cid_input(&nhash).await.unwrap();
    assert_eq!(resolved.cid.hash, [0xaa; 32]);
    assert!(resolved.path.is_none());

    // Test nhash with single file path suffix
    let with_path = format!("{}/bitcoin.pdf", nhash);
    let resolved = resolve_cid_input(&with_path).await.unwrap();
    assert_eq!(resolved.cid.hash, [0xaa; 32]);
    assert_eq!(resolved.path, Some("bitcoin.pdf".to_string()));

    // Test nhash with nested path suffix
    let with_nested = format!("{}/docs/papers/bitcoin.pdf", nhash);
    let resolved = resolve_cid_input(&with_nested).await.unwrap();
    assert_eq!(resolved.cid.hash, [0xaa; 32]);
    assert_eq!(resolved.path, Some("docs/papers/bitcoin.pdf".to_string()));
}

#[tokio::test]
async fn test_resolve_nhash_with_htree_prefix() {
    let nhash = hashtree_core::nhash_encode(&[0xbb; 32]).unwrap();

    // Test htree:// prefix with path
    let htree_url = format!("htree://{}/file.txt", nhash);
    let resolved = resolve_cid_input(&htree_url).await.unwrap();
    assert_eq!(resolved.cid.hash, [0xbb; 32]);
    assert_eq!(resolved.path, Some("file.txt".to_string()));
}

#[tokio::test]
async fn test_resolve_hex_cid_with_key_and_path() {
    let hash = [0x11; 32];
    let key = [0x22; 32];
    let hash_hex = hashtree_core::to_hex(&hash);
    let key_hex = hashtree_core::to_hex(&key);
    let cid = format!("{}:{}", hash_hex, key_hex);

    let resolved = resolve_cid_input(&cid).await.unwrap();
    assert_eq!(resolved.cid.hash, hash);
    assert_eq!(resolved.cid.key, Some(key));
    assert!(resolved.path.is_none());

    let with_path = format!("{}/dir/file.txt", cid);
    let resolved = resolve_cid_input(&with_path).await.unwrap();
    assert_eq!(resolved.cid.hash, hash);
    assert_eq!(resolved.cid.key, Some(key));
    assert_eq!(resolved.path, Some("dir/file.txt".to_string()));
}

#[tokio::test]
async fn test_resolve_hex_cid_without_key() {
    let hash = [0x33; 32];
    let hash_hex = hashtree_core::to_hex(&hash);
    let resolved = resolve_cid_input(&hash_hex).await.unwrap();
    assert_eq!(resolved.cid.hash, hash);
    assert!(resolved.cid.key.is_none());
}
