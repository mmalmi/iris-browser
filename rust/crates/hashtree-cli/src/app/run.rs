use anyhow::{Context, Result};
use clap::Parser;
use hashtree_cli::config::{
    ensure_auth_cookie, ensure_keys, ensure_keys_string, parse_npub, pubkey_bytes,
};
#[cfg(feature = "p2p")]
use hashtree_cli::WebRTCManager;
use hashtree_cli::{
    spawn_background_eviction_task, Config, FetchConfig, Fetcher, HashtreeServer, HashtreeStore,
    NostrKeys, NostrResolverConfig, NostrRootResolver, NostrToBech32, RootResolver,
    BACKGROUND_EVICTION_INTERVAL,
};
use hashtree_core::{Cid, HashTree, HashTreeConfig, NHashData};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::args::{
    Cli, Commands, PrCommands, ReleaseCommands, SocialGraphCommands, StorageCommands,
};
use super::blossom::{background_blossom_push, push_to_blossom};
use super::cashu_delegate::run_cashu_helper;
use super::content::add_directory;
use super::daemonize::{format_daemon_status, spawn_daemon, stop_daemon};
use super::lists::{follow_user, list_following, list_muted, mute_user, update_profile};
#[cfg(feature = "fuse")]
use super::mount::mount_fuse;
use super::nostr_index::{run_socialgraph_index_from_cli, SocialGraphIndexOptions};
use super::peers::{fetch_profile_name, list_peers};
use super::release::publish_release_version;
use super::resolve::resolve_cid_input;
use super::socialgraph::{
    run_socialgraph_filter, run_socialgraph_rebuild_profile_index, run_socialgraph_snapshot,
    run_socialgraph_stats, run_socialgraph_warm,
};
use super::util::chrono_humanize_timestamp;

const IRIS_FILES_WEB_BASE_URL: &str = "https://files.iris.to";
const IRIS_SITES_WEB_BASE_URL: &str = "https://sites.iris.to";

pub(crate) async fn run() -> Result<()> {
    // Install rustls crypto provider (required for TLS connections)
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Initialize tracing (respects RUST_LOG env var)
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    // Get data_dir early to avoid borrow issues in match arms
    let data_dir = cli.data_dir();

    match cli.command {
        Commands::Start {
            addr,
            relays: relays_override,
            daemon,
            log_file,
            pid_file,
        } => {
            if daemon && std::env::var_os("HTREE_DAEMONIZED").is_none() {
                spawn_daemon(
                    &addr,
                    relays_override.as_deref(),
                    cli.data_dir.clone(),
                    log_file.as_ref(),
                    pid_file.as_ref(),
                )?;
                return Ok(());
            }
            // Load or create config
            let mut config = Config::load()?;

            // Override relays if specified on command line
            if let Some(relays_str) = relays_override.as_deref() {
                config.nostr.relays = relays_str
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .collect();
                println!("Using relays from CLI: {:?}", config.nostr.relays);
            }

            // Use CLI data_dir if provided, otherwise use config's data_dir
            let data_dir = cli
                .data_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from(&config.storage.data_dir));

            // Convert max_size_gb to bytes
            let max_size_bytes = config.storage.max_size_gb * 1024 * 1024 * 1024;
            let nostr_db_max_bytes = config
                .nostr
                .db_max_size_gb
                .saturating_mul(1024 * 1024 * 1024);
            let spambox_db_max_bytes = config
                .nostr
                .spambox_max_size_gb
                .saturating_mul(1024 * 1024 * 1024);
            let store = Arc::new(HashtreeStore::with_options(
                &data_dir,
                config.storage.s3.as_ref(),
                max_size_bytes,
            )?);

            // Ensure nsec exists (generate if needed)
            let (keys, was_generated) = ensure_keys()?;
            let pk_bytes = pubkey_bytes(&keys);
            let npub = keys
                .public_key()
                .to_bech32()
                .context("Failed to encode npub")?;

            // Convert allowed_npubs to hex pubkeys for blossom access control
            let mut allowed_pubkeys: HashSet<String> = HashSet::new();
            // Always allow own pubkey
            allowed_pubkeys.insert(hex::encode(pk_bytes));
            // Add configured allowed npubs
            for npub_str in &config.nostr.allowed_npubs {
                if let Ok(pk) = parse_npub(npub_str) {
                    allowed_pubkeys.insert(hex::encode(pk));
                } else {
                    tracing::warn!("Invalid npub in allowed_npubs: {}", npub_str);
                }
            }

            // Initialize the local social graph store.
            let graph_store = hashtree_cli::socialgraph::open_social_graph_store_with_storage(
                &data_dir,
                store.store_arc(),
                Some(nostr_db_max_bytes),
            )
            .context("Failed to initialize social graph store")?;
            graph_store.set_profile_index_overmute_threshold(config.nostr.overmute_threshold);

            // Set social graph root (configured npub or own key)
            let social_graph_root_bytes = if let Some(ref root_npub) = config.nostr.socialgraph_root
            {
                parse_npub(root_npub).unwrap_or(pk_bytes)
            } else {
                pk_bytes
            };
            hashtree_cli::socialgraph::set_social_graph_root(
                &graph_store,
                &social_graph_root_bytes,
            );
            let social_graph_store: Arc<dyn hashtree_cli::socialgraph::SocialGraphBackend> =
                graph_store.clone();

            // Build social graph access control
            let social_graph = Arc::new(hashtree_cli::socialgraph::SocialGraphAccessControl::new(
                Arc::clone(&social_graph_store),
                config.nostr.max_write_distance,
                allowed_pubkeys.clone(),
            ));

            let nostr_relay_config = hashtree_cli::nostr_relay::NostrRelayConfig {
                spambox_db_max_bytes,
                ..Default::default()
            };
            let mut public_event_pubkeys = HashSet::new();
            public_event_pubkeys.insert(hex::encode(pk_bytes));
            let nostr_relay = Arc::new(
                hashtree_cli::nostr_relay::NostrRelay::new(
                    Arc::clone(&social_graph_store),
                    data_dir.clone(),
                    public_event_pubkeys,
                    Some(social_graph.clone()),
                    nostr_relay_config,
                )
                .context("Failed to initialize Nostr relay")?,
            );

            let crawler_spambox = if spambox_db_max_bytes == 0 {
                None
            } else {
                let spam_dir = data_dir.join("socialgraph_spambox");
                match hashtree_cli::socialgraph::open_social_graph_store_at_path(
                    &spam_dir,
                    Some(spambox_db_max_bytes),
                ) {
                    Ok(store) => Some(store),
                    Err(err) => {
                        tracing::warn!("Failed to open social graph spambox for crawler: {}", err);
                        None
                    }
                }
            };
            let crawler_spambox_backend = crawler_spambox
                .clone()
                .map(|store| store as Arc<dyn hashtree_cli::socialgraph::SocialGraphBackend>);

            #[cfg(feature = "p2p")]
            let peer_router_enabled = hashtree_cli::p2p_common::peer_router_enabled(&config);

            // Start STUN server and WebRTC if P2P feature enabled
            #[cfg(feature = "p2p")]
            let (stun_handle, webrtc_handle, webrtc_state) = {
                // Start STUN server if configured
                let stun_handle = if hashtree_cli::p2p_common::should_start_stun_server(&config) {
                    let stun_addr: std::net::SocketAddr =
                        format!("0.0.0.0:{}", config.server.stun_port)
                            .parse()
                            .context("Invalid STUN bind address")?;
                    Some(
                        hashtree_cli::server::stun::start_stun_server(stun_addr)
                            .await
                            .context("Failed to start STUN server")?,
                    )
                } else {
                    None
                };

                // Start WebRTC signaling manager if enabled
                let (webrtc_handle, webrtc_state) = if peer_router_enabled {
                    let webrtc_config = hashtree_cli::p2p_common::default_webrtc_config(&config);
                    let peer_classifier = hashtree_cli::p2p_common::build_peer_classifier(
                        data_dir.clone(),
                        Arc::clone(&social_graph_store),
                    );
                    let cashu_payment_client = if config.cashu.default_mint.is_some()
                        || !config.cashu.accepted_mints.is_empty()
                    {
                        match hashtree_cli::cashu_helper::CashuHelperClient::discover(
                            data_dir.clone(),
                        ) {
                            Ok(client) => Some(Arc::new(client)
                                as Arc<dyn hashtree_cli::cashu_helper::CashuPaymentClient>),
                            Err(err) => {
                                tracing::warn!(
                                    "Cashu settlement helper unavailable; paid retrieval stays disabled: {}",
                                    err
                                );
                                None
                            }
                        }
                    } else {
                        None
                    };
                    let cashu_mint_metadata = if config.cashu.default_mint.is_some()
                        || !config.cashu.accepted_mints.is_empty()
                    {
                        let metadata_path =
                            hashtree_cli::webrtc::cashu_mint_metadata_path(&data_dir);
                        match hashtree_cli::webrtc::CashuMintMetadataStore::load(metadata_path) {
                            Ok(store) => Some(store),
                            Err(err) => {
                                tracing::warn!(
                                    "Failed to load Cashu mint metadata; falling back to in-memory state: {}",
                                    err
                                );
                                Some(hashtree_cli::webrtc::CashuMintMetadataStore::in_memory())
                            }
                        }
                    } else {
                        None
                    };

                    let mut manager = WebRTCManager::new_with_store_and_classifier_and_cashu(
                        keys.clone(),
                        webrtc_config,
                        Arc::clone(&store) as Arc<dyn hashtree_cli::ContentStore>,
                        peer_classifier,
                        hashtree_cli::webrtc::CashuRoutingConfig::from(&config.cashu),
                        cashu_payment_client,
                        cashu_mint_metadata,
                    );
                    manager.set_nostr_relay(nostr_relay.clone());

                    // Get the WebRTC state before spawning (for HTTP handler to query peers)
                    let webrtc_state = manager.state();

                    // Spawn the manager in a background task
                    let handle = tokio::spawn(async move {
                        if let Err(e) = manager.run().await {
                            tracing::error!("Peer router error: {}", e);
                        }
                    });
                    (Some(handle), Some(webrtc_state))
                } else {
                    (None, None)
                };
                (stun_handle, webrtc_handle, webrtc_state)
            };

            #[cfg(not(feature = "p2p"))]
            #[allow(clippy::type_complexity)]
            let (stun_handle, webrtc_handle, webrtc_state): (
                Option<tokio::task::JoinHandle<()>>,
                Option<tokio::task::JoinHandle<()>>,
                Option<Arc<hashtree_cli::webrtc::WebRTCState>>,
            ) = (None, None, None);

            // Combine legacy servers with configured public read servers.
            let upstream_blossom = config.blossom.all_read_servers();
            let active_nostr_relays = config.nostr.active_relays();

            // Set up server with allowed pubkeys for blossom write access
            let mut server = HashtreeServer::new(Arc::clone(&store), addr.clone())
                .with_allowed_pubkeys(allowed_pubkeys.clone())
                .with_max_upload_bytes((config.blossom.max_upload_mb as usize) * 1024 * 1024)
                .with_public_writes(config.server.public_writes)
                .with_upstream_blossom(upstream_blossom)
                .with_nostr_relay_urls(active_nostr_relays);

            // Add social graph to server
            server = server.with_social_graph(social_graph);
            server = server.with_socialgraph_snapshot(
                Arc::clone(&social_graph_store),
                social_graph_root_bytes,
                config.server.socialgraph_snapshot_public,
            );
            server = server.with_nostr_relay(nostr_relay.clone());

            // Add WebRTC peer state for P2P queries from HTTP handler
            if let Some(ref webrtc_state) = webrtc_state {
                server = server.with_webrtc_peers(webrtc_state.clone());
            }

            let background_services_controller = Arc::new(
                hashtree_cli::daemon::EmbeddedBackgroundServicesController::new(
                    keys.clone(),
                    data_dir.clone(),
                    Arc::clone(&store),
                    graph_store.clone(),
                    Arc::clone(&social_graph_store),
                    crawler_spambox_backend,
                    webrtc_state.clone(),
                ),
            );
            background_services_controller
                .apply_config(&config)
                .await
                .context("Failed to start background services")?;

            // Start background eviction task (runs every 5 minutes)
            let eviction_handle = spawn_background_eviction_task(
                Arc::clone(&store),
                BACKGROUND_EVICTION_INTERVAL,
                "daemon",
            );

            // Print startup info
            println!("Starting hashtree daemon on {}", addr);
            println!("Data directory: {}", data_dir.display());
            if was_generated {
                println!("Identity: {} (new)", npub);
            } else {
                println!("Identity: {}", npub);
            }
            if !config.nostr.allowed_npubs.is_empty() {
                println!(
                    "Allowed writers: {} npubs",
                    config.nostr.allowed_npubs.len()
                );
            }
            if config.server.public_writes {
                println!("Public writes: enabled");
            }
            println!("Relays: {} configured", config.nostr.relays.len());
            println!("Git remote: http://{}/git/<pubkey>/<repo>", addr);
            #[cfg(feature = "p2p")]
            if let Some(ref handle) = stun_handle {
                println!("STUN server: {}", handle.addr);
            }
            #[cfg(feature = "p2p")]
            if config.server.enable_webrtc {
                println!("WebRTC: enabled (P2P connections)");
            }
            #[cfg(feature = "p2p")]
            if config.server.enable_multicast && config.server.max_multicast_peers > 0 {
                println!(
                    "Multicast: enabled (max {} peers)",
                    config.server.max_multicast_peers
                );
            }
            #[cfg(feature = "p2p")]
            if config.server.enable_bluetooth && config.server.max_bluetooth_peers > 0 {
                println!(
                    "Bluetooth: enabled (max {} peers)",
                    config.server.max_bluetooth_peers
                );
            }
            println!(
                "Social graph: enabled (social_graph_crawl_depth={}, max_write_distance={})",
                config.nostr.social_graph_crawl_depth, config.nostr.max_write_distance
            );
            println!("Storage limit: {} GB", config.storage.max_size_gb);
            if !config.cashu.accepted_mints.is_empty() {
                println!(
                    "Cashu accepted mints: {}",
                    config.cashu.accepted_mints.len()
                );
                if let Some(default_mint) = &config.cashu.default_mint {
                    println!("Cashu default mint: {}", default_mint);
                }
            }
            if config.sync.enabled {
                let mut sync_features = Vec::new();
                if config.sync.sync_own {
                    sync_features.push("own trees");
                }
                if config.sync.sync_followed {
                    sync_features.push("followed trees");
                }
                println!("Background sync: enabled ({})", sync_features.join(", "));
            }

            if config.server.enable_auth {
                let (username, password) = ensure_auth_cookie()?;
                println!();
                println!("Web UI: http://{}/#{}:{}", addr, username, password);
                server = server.with_auth(username, password);
            } else {
                println!("Web UI: http://{}", addr);
                println!("Auth: disabled");
            }

            server.run().await?;

            // Shutdown social graph crawler
            // Shutdown background eviction
            eviction_handle.abort();

            background_services_controller.shutdown().await;

            // Shutdown WebRTC manager
            #[cfg(feature = "p2p")]
            if let Some(handle) = webrtc_handle {
                handle.abort();
            }

            // Shutdown STUN server
            #[cfg(feature = "p2p")]
            if let Some(handle) = stun_handle {
                handle.shutdown();
            }

            // Suppress unused variable warnings when p2p is disabled
            #[cfg(not(feature = "p2p"))]
            let _ = (stun_handle, webrtc_handle);
        }
        #[cfg(feature = "fuse")]
        Commands::Mount {
            target,
            mountpoint,
            visibility,
            link_key,
            private,
            relays,
            allow_other,
        } => {
            mount_fuse(
                target,
                mountpoint,
                visibility,
                link_key,
                private,
                relays,
                allow_other,
                data_dir,
            )
            .await?;
        }
        Commands::Add {
            path,
            only_hash,
            unencrypted,
            no_ignore,
            publish,
            local,
        } => {
            let is_dir = path.is_dir();

            if only_hash {
                // Use in-memory store for hash-only mode
                use futures::io::AllowStdIo;
                use hashtree_core::store::MemoryStore;
                use hashtree_core::{to_hex, HashTree, HashTreeConfig};
                use std::sync::Arc;

                let store = Arc::new(MemoryStore::new());
                // Use unified API: CHK encryption by default, .public() for raw plaintext blobs
                let config = if unencrypted {
                    HashTreeConfig::new(store.clone()).public()
                } else {
                    HashTreeConfig::new(store.clone())
                };
                let tree = HashTree::new(config);

                if is_dir {
                    // For directories, use the recursive helper
                    let cid = add_directory(&tree, &path, !no_ignore).await?;
                    println!("hash: {}", to_hex(&cid.hash));
                    if let Some(key) = cid.key {
                        println!("key:  {}", to_hex(&key));
                    }
                } else {
                    let file = std::fs::File::open(&path).with_context(|| {
                        format!("Failed to open file for hashing: {}", path.display())
                    })?;
                    let (cid, _size) = tree
                        .put_stream(AllowStdIo::new(file))
                        .await
                        .map_err(|e| anyhow::anyhow!("Failed to hash file: {}", e))?;
                    println!("hash: {}", to_hex(&cid.hash));
                    if let Some(key) = cid.key {
                        println!("key:  {}", to_hex(&key));
                    }
                }
            } else {
                // Store in local hashtree
                use hashtree_core::{
                    from_hex, key_from_hex, nhash_encode, nhash_encode_full, Cid, NHashData,
                };

                let store = HashtreeStore::new(&data_dir)?;
                let site_entry = detect_site_entry_for_path(&path, is_dir);

                // Store and capture cid/hash/key for potential publishing
                let (cid_for_push, hash_hex, key_hex, display_root): (
                    String,
                    String,
                    Option<String>,
                    String,
                ) = if unencrypted {
                    let hash_hex = if is_dir {
                        store
                            .upload_dir_with_options(&path, !no_ignore)
                            .context("Failed to add directory")?
                    } else {
                        store.upload_file(&path).context("Failed to add file")?
                    };
                    let hash = from_hex(&hash_hex).context("Invalid hash")?;
                    let nhash = nhash_encode(&hash)
                        .map_err(|e| anyhow::anyhow!("Failed to encode nhash: {}", e))?;
                    (hash_hex.clone(), hash_hex, None, nhash)
                } else {
                    let cid_str = if is_dir {
                        store
                            .upload_dir_encrypted_with_options(&path, !no_ignore)
                            .context("Failed to add directory")?
                    } else {
                        store
                            .upload_file_encrypted(&path)
                            .context("Failed to add file")?
                    };
                    // Parse cid_str which may be "hash" or "hash:key"
                    let (hash_hex, key_hex) = if let Some((h, k)) = cid_str.split_once(':') {
                        (h.to_string(), Some(k.to_string()))
                    } else {
                        (cid_str.clone(), None)
                    };
                    let hash = from_hex(&hash_hex).context("Invalid hash")?;
                    let key = key_hex
                        .as_ref()
                        .map(|k| key_from_hex(k))
                        .transpose()
                        .map_err(|e| anyhow::anyhow!("Invalid key: {}", e))?;
                    let nhash_data = NHashData {
                        hash,
                        decrypt_key: key,
                    };
                    let nhash = nhash_encode_full(&nhash_data)
                        .map_err(|e| anyhow::anyhow!("Failed to encode nhash: {}", e))?;
                    (cid_str, hash_hex, key_hex, nhash)
                };

                println!("added {}", path.display());
                let display_route = if is_dir {
                    display_root.clone()
                } else {
                    let filename = path
                        .file_name()
                        .map(|name| name.to_string_lossy().to_string())
                        .unwrap_or_default();
                    format!("{display_root}/{filename}")
                };
                println!("  url:   {}", display_route);
                println!(
                    "  files: {}",
                    build_files_iris_to_url_for_add_route(&display_route)
                );
                if let Some(entry_path) = site_entry.as_deref() {
                    let site_route = format!("{display_root}/{entry_path}");
                    println!(
                        "  site:  {}",
                        build_sites_iris_to_url_for_add_route(&site_route)
                    );
                }
                println!("  hash:  {}", hash_hex);
                if let Some(ref k) = key_hex {
                    println!("  key:   {}", k);
                }

                // Index tree for eviction tracking (own content = highest priority)
                // Get user's npub as owner
                let (nsec_str, _) = ensure_keys_string()?;
                let keys = NostrKeys::parse(&nsec_str).context("Failed to parse nsec")?;
                let npub = NostrToBech32::to_bech32(&keys.public_key())
                    .context("Failed to encode npub")?;

                let tree_name = path.file_name().map(|n| n.to_string_lossy().to_string());

                // Build ref_key: "npub/filename"
                let ref_key = tree_name.as_ref().map(|name| format!("{}/{}", npub, name));

                let hash_bytes = from_hex(&hash_hex).context("Invalid hash")?;
                if let Err(e) = store.index_tree(
                    &hash_bytes,
                    &npub,
                    tree_name.as_deref(),
                    hashtree_cli::PRIORITY_OWN,
                    ref_key.as_deref(),
                ) {
                    tracing::warn!("Failed to index tree: {}", e);
                }

                let mut write_servers = Vec::new();
                if !local {
                    let config = Config::load()?;
                    // Combine legacy servers with write_servers for pushing.
                    write_servers = config.blossom.servers.clone();
                    write_servers.extend(config.blossom.write_servers.clone());
                    if !write_servers.is_empty() && publish.is_none() {
                        let push_result =
                            background_blossom_push(&data_dir, &cid_for_push, &write_servers).await;
                        if let Err(e) = push_result {
                            eprintln!("  file server push failed: {}", e);
                        }
                    }
                }

                // Publish to Nostr if --publish was specified.
                if let Some(ref_name) = publish.as_deref() {
                    let config = Config::load()?;

                    // Ensure nsec exists (generate if needed)
                    let (nsec_str, was_generated) = ensure_keys_string()?;

                    // Create Keys using nostr-sdk's version (via NostrKeys re-export)
                    let keys = NostrKeys::parse(&nsec_str).context("Failed to parse nsec")?;
                    let npub = NostrToBech32::to_bech32(&keys.public_key())
                        .context("Failed to encode npub")?;

                    if was_generated {
                        println!("  identity: {} (new)", npub);
                    }

                    let resolver_config = NostrResolverConfig {
                        relays: config.nostr.relays.clone(),
                        resolve_timeout: Duration::from_secs(5),
                        secret_key: Some(keys),
                    };

                    let resolver = NostrRootResolver::new(resolver_config)
                        .await
                        .context("Failed to create Nostr resolver")?;

                    let hash = from_hex(&hash_hex).context("Invalid hash")?;
                    let key = key_hex
                        .as_ref()
                        .map(|k| key_from_hex(k))
                        .transpose()
                        .map_err(|e| anyhow::anyhow!("Invalid key: {}", e))?;
                    let cid = Cid { hash, key };
                    let nostr_key = format!("{}/{}", npub, ref_name);

                    match resolver.publish(&nostr_key, &cid).await {
                        Ok(_) => {
                            println!("  published: {}", nostr_key);
                            println!(
                                "  files: {}",
                                build_files_iris_to_url_for_published_ref(&npub, ref_name)
                            );
                            if let Some(entry_path) = site_entry.as_deref() {
                                println!(
                                    "  site:  {}",
                                    build_sites_iris_to_url_for_published_ref(
                                        &npub, ref_name, entry_path
                                    )
                                );
                                let immutable_site_route = format!("{display_root}/{entry_path}");
                                println!(
                                    "  permalink: {}",
                                    build_sites_iris_to_url_for_add_route(&immutable_site_route)
                                );
                            }
                        }
                        Err(e) => {
                            eprintln!("  publish failed: {}", e);
                        }
                    }

                    let _ = resolver.stop().await;

                    if !local && !write_servers.is_empty() {
                        if let Err(err) =
                            background_blossom_push(&data_dir, &cid_for_push, &write_servers)
                                .await
                                .context("Failed to push content to file servers")
                        {
                            eprintln!("  file server push failed: {}", err);
                        }
                    }
                }
            }
        }
        Commands::Get {
            cid: cid_input,
            output,
        } => {
            use hashtree_cli::{FetchConfig, Fetcher};
            use hashtree_core::{to_hex, Cid};

            // Resolve to Cid (raw bytes, no hex conversion needed for nhash)
            let resolved = resolve_cid_input(&cid_input).await?;
            let cid = resolved.cid;
            let hash_hex = to_hex(&cid.hash);

            let store = Arc::new(HashtreeStore::new(&data_dir)?);
            let fetcher = Fetcher::new(FetchConfig::default());

            // Try to fetch tree from remote if not local
            fetcher.fetch_cid_tree(&store, None, &cid).await?;

            // Check if it's a directory
            let listing = store.get_directory_listing_by_cid(&cid)?;

            // Handle path: nhash/path/to/file.ext
            if let Some(ref path) = resolved.path {
                if listing.is_some() {
                    // nhash points to directory - resolve path within it
                    let resolved_cid = store
                        .resolve_path(&cid, path)?
                        .ok_or_else(|| anyhow::anyhow!("Path not found in directory: {}", path))?;

                    // Fetch the resolved file if needed
                    fetcher.fetch_cid_tree(&store, None, &resolved_cid).await?;

                    // Get the filename from the path
                    let filename = path.rsplit('/').next().unwrap_or(path);
                    let out_path = output.unwrap_or_else(|| PathBuf::from(filename));

                    store.write_file_by_cid(&resolved_cid, &out_path)?;
                    println!("{} -> {}", to_hex(&resolved_cid.hash), out_path.display());
                } else {
                    // nhash points to file - save with the filename from path
                    let filename = path.rsplit('/').next().unwrap_or(path);
                    let out_path = output.unwrap_or_else(|| PathBuf::from(filename));

                    store.write_file_by_cid(&cid, &out_path)?;
                    println!("{} -> {}", hash_hex, out_path.display());
                }
            } else if listing.is_some() {
                // It's a directory - create it and download contents
                let out_dir = output.unwrap_or_else(|| PathBuf::from(&hash_hex));
                std::fs::create_dir_all(&out_dir)?;

                async fn download_dir(
                    store: &Arc<HashtreeStore>,
                    cid: &Cid,
                    dir: &std::path::Path,
                ) -> Result<()> {
                    // Get listing
                    let listing = store.get_directory_listing_by_cid(cid)?;
                    if let Some(listing) = listing {
                        for entry in listing.entries {
                            let entry_path = dir.join(&entry.name);
                            let entry_cid = Cid::parse(&entry.cid)
                                .map_err(|e| anyhow::anyhow!("Invalid CID: {}", e))?;
                            if entry.is_directory {
                                std::fs::create_dir_all(&entry_path)?;
                                Box::pin(download_dir(store, &entry_cid, &entry_path)).await?;
                            } else {
                                store.write_file_by_cid(&entry_cid, &entry_path)?;
                                println!("  {} -> {}", entry.cid, entry_path.display());
                            }
                        }
                    }
                    Ok(())
                }

                println!("Downloading directory to {}", out_dir.display());
                download_dir(&store, &cid, &out_dir).await?;
                println!("Done.");
            } else {
                // Try as a file - stream from store to output path with decryption support.
                let out_path = output.unwrap_or_else(|| PathBuf::from(&hash_hex));
                store.write_file_by_cid(&cid, &out_path)?;
                println!("{} -> {}", hash_hex, out_path.display());
            }
        }
        Commands::Cat { cid: cid_input } => {
            use hashtree_cli::{FetchConfig, Fetcher};
            use hashtree_core::to_hex;

            // Resolve npub/repo or htree:// URLs to CID
            let resolved = resolve_cid_input(&cid_input).await?;
            let cid_hex = to_hex(&resolved.cid.hash);

            let store = Arc::new(HashtreeStore::new(&data_dir)?);

            // Create fetcher (BlossomClient auto-loads servers from config)
            let fetcher = Fetcher::new(FetchConfig::default());

            // Fetch file (local first, then Blossom)
            if let Some(content) = fetcher.fetch_file(&store, None, &resolved.cid.hash).await? {
                use std::io::Write;
                std::io::stdout().write_all(&content)?;
            } else {
                anyhow::bail!("CID not found locally or on remote servers: {}", cid_hex);
            }
        }
        Commands::Pins => {
            let store = HashtreeStore::new(&data_dir)?;
            let pins = store.list_pins_with_names()?;
            if pins.is_empty() {
                println!("No pinned CIDs");
            } else {
                println!("Pinned items ({}):", pins.len());
                for pin in pins {
                    let icon = if pin.is_directory { "dir" } else { "file" };
                    println!("  [{}] {} ({})", icon, pin.name, pin.cid);
                }
            }
        }
        Commands::Pin { cid: cid_input } => {
            // Resolve npub/repo or htree:// URLs to CID
            let resolved = resolve_cid_input(&cid_input).await?;
            let store = HashtreeStore::new(&data_dir)?;
            store.pin(&resolved.cid.hash)?;
            println!("Pinned: {}", format_cid_for_display(&resolved.cid));
        }
        Commands::Unpin { cid: cid_input } => {
            // Resolve npub/repo or htree:// URLs to CID
            let resolved = resolve_cid_input(&cid_input).await?;
            let store = HashtreeStore::new(&data_dir)?;
            store.unpin(&resolved.cid.hash)?;
            println!("Unpinned: {}", format_cid_for_display(&resolved.cid));
        }
        Commands::Info { cid: cid_input } => {
            // Resolve npub/repo or htree:// URLs to CID
            let resolved = resolve_cid_input(&cid_input).await?;
            let store = Arc::new(HashtreeStore::new(&data_dir)?);
            let fetcher = Fetcher::new(FetchConfig::default());
            let target_cid =
                resolve_info_target(&store, &fetcher, &resolved.cid, resolved.path.as_deref())
                    .await?;

            if !print_info_for_cid(&store, &target_cid).await? {
                println!("Hash not found: {}", format_cid_for_display(&target_cid));
            }
        }
        Commands::Stats => {
            let store = HashtreeStore::new(&data_dir)?;
            let stats = store.get_storage_stats()?;
            println!("Storage Statistics:");
            println!("  Total DAGs: {}", stats.total_dags);
            println!("  Pinned DAGs: {}", stats.pinned_dags);
            println!(
                "  Total size: {} bytes ({:.2} KB)",
                stats.total_bytes,
                stats.total_bytes as f64 / 1024.0
            );
        }
        Commands::Status { addr } => {
            let url = format!("http://{}/api/status", addr);
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .context("Failed to build HTTP client")?;
            match client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    let status: serde_json::Value = resp.json().await?;
                    println!("{}", format_daemon_status(&status, true));
                }
                Ok(resp) => {
                    eprintln!("Daemon returned error: {}", resp.status());
                }
                Err(err) if err.is_timeout() => {
                    eprintln!(
                        "Daemon at {} did not respond before the status timeout",
                        addr
                    );
                    eprintln!("Check daemon logs or try again after load subsides");
                }
                Err(_) => {
                    eprintln!("Daemon not running at {}", addr);
                    eprintln!("Start with: htree start");
                }
            }
        }
        Commands::Stop { pid_file } => {
            stop_daemon(pid_file.as_ref())?;
        }
        Commands::Gc => {
            let store = HashtreeStore::new(&data_dir)?;
            println!("Running garbage collection...");
            let gc_stats = store.gc()?;
            println!("Deleted {} DAGs", gc_stats.deleted_dags);
            println!(
                "Freed {} bytes ({:.2} KB)",
                gc_stats.freed_bytes,
                gc_stats.freed_bytes as f64 / 1024.0
            );
        }
        Commands::User { identity } => {
            use hashtree_cli::config::get_keys_path;
            use nostr::nips::nip19::FromBech32;
            use std::fs;

            match identity {
                None => {
                    // Show current identity
                    let (keys, was_generated) = ensure_keys()?;
                    let npub = keys.public_key().to_bech32()?;
                    if was_generated {
                        eprintln!("Generated new identity");
                    }
                    // Try to fetch profile name
                    let config = Config::load()?;
                    let profile_name =
                        fetch_profile_name(&config.nostr.relays, &keys.public_key().to_hex()).await;
                    if let Some(name) = profile_name {
                        println!("{} ({})", npub, name);
                    } else {
                        println!("{}", npub);
                    }
                }
                Some(id) => {
                    // Set identity - accept nsec or derive from input
                    let nsec = if id.starts_with("nsec1") {
                        // Validate it's a valid nsec
                        nostr::SecretKey::from_bech32(&id).context("Invalid nsec")?;
                        id
                    } else {
                        anyhow::bail!("Identity must be an nsec (secret key). Use 'htree user' to see your current npub.");
                    };

                    // Save to keys file
                    let keys_path = get_keys_path();
                    if let Some(parent) = keys_path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(&keys_path, &nsec)?;

                    // Set permissions to 0600
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        fs::set_permissions(&keys_path, fs::Permissions::from_mode(0o600))?;
                    }

                    // Show the new npub
                    let secret_key = nostr::SecretKey::from_bech32(&nsec)?;
                    let keys = nostr::Keys::new(secret_key);
                    let npub = keys.public_key().to_bech32()?;
                    println!("{}", npub);
                }
            }
        }
        Commands::Publish {
            ref_name,
            hash,
            key,
        } => {
            use hashtree_core::{from_hex, key_from_hex, Cid};

            // Load config for relay list
            let config = Config::load()?;

            // Ensure nsec exists (generate if needed)
            let (nsec_str, was_generated) = ensure_keys_string()?;

            // Create Keys using nostr-sdk's version
            let keys = NostrKeys::parse(&nsec_str).context("Failed to parse nsec")?;
            let npub =
                NostrToBech32::to_bech32(&keys.public_key()).context("Failed to encode npub")?;

            if was_generated {
                println!("Identity: {} (new)", npub);
            }

            // Parse hash and optional key
            let hash_bytes = from_hex(&hash).context("Invalid hash (expected hex)")?;
            let key_bytes = key
                .as_ref()
                .map(|k| key_from_hex(k))
                .transpose()
                .map_err(|e| anyhow::anyhow!("Invalid key: {}", e))?;

            let cid = Cid {
                hash: hash_bytes,
                key: key_bytes,
            };

            // Create resolver config with secret key for publishing
            let resolver_config = NostrResolverConfig {
                relays: config.nostr.relays.clone(),
                resolve_timeout: Duration::from_secs(5),
                secret_key: Some(keys),
            };

            // Create resolver
            let resolver = NostrRootResolver::new(resolver_config)
                .await
                .context("Failed to create Nostr resolver")?;

            // Build Nostr key: "npub.../ref_name"
            let nostr_key = format!("{}/{}", npub, ref_name);

            // Publish
            match resolver.publish(&nostr_key, &cid).await {
                Ok(_) => {
                    println!("Published: {}", nostr_key);
                    println!("  hash: {}", hash);
                    if let Some(k) = key {
                        println!("  key:  {}", k);
                    }
                }
                Err(e) => {
                    eprintln!("Publish failed: {}", e);
                    std::process::exit(1);
                }
            }

            // Clean up
            let _ = resolver.stop().await;
        }
        Commands::Release { command } => match command {
            ReleaseCommands::Publish {
                tree_name,
                version_path,
                cid,
                local,
            } => {
                let published =
                    publish_release_version(&data_dir, &tree_name, &version_path, &cid, local)
                        .await?;

                println!(
                    "Published release: htree://{}/{}/{}",
                    published.npub, published.tree_name, published.version_path
                );
                println!(
                    "Latest release:    htree://{}/{}/{}",
                    published.npub, published.tree_name, published.latest_path
                );
            }
        },
        Commands::Follow { npub } => {
            follow_user(&data_dir, &npub, true).await?;
        }
        Commands::Unfollow { npub } => {
            follow_user(&data_dir, &npub, false).await?;
        }
        Commands::Mute { npub, reason } => {
            mute_user(&data_dir, &npub, reason.as_deref(), true).await?;
        }
        Commands::Unmute { npub } => {
            mute_user(&data_dir, &npub, None, false).await?;
        }
        Commands::Following => {
            list_following(&data_dir).await?;
        }
        Commands::Muted => {
            list_muted(&data_dir).await?;
        }
        Commands::Socialgraph { command } => match command {
            SocialGraphCommands::Filter {
                max_distance,
                overmute_threshold,
            } => {
                run_socialgraph_filter(data_dir, max_distance, overmute_threshold)?;
            }
            SocialGraphCommands::Stats => {
                run_socialgraph_stats(data_dir)?;
            }
            SocialGraphCommands::Warm {
                secs,
                crawl_depth,
                full_graph_recrawl,
                relays,
                author_batch_size,
                concurrent_batches,
            } => {
                run_socialgraph_warm(
                    data_dir,
                    secs,
                    crawl_depth,
                    full_graph_recrawl,
                    relays,
                    author_batch_size,
                    concurrent_batches,
                )
                .await?;
            }
            SocialGraphCommands::Snapshot {
                out,
                max_nodes,
                max_edges,
                max_distance,
                max_edges_per_node,
            } => {
                run_socialgraph_snapshot(
                    data_dir,
                    out,
                    max_nodes,
                    max_edges,
                    max_distance,
                    max_edges_per_node,
                )?;
            }
            SocialGraphCommands::RebuildProfileIndex => {
                run_socialgraph_rebuild_profile_index(data_dir)?;
            }
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
            } => {
                let config = Config::load()?;
                let effective_crawl_depth =
                    crawl_depth.unwrap_or(config.nostr.social_graph_crawl_depth);
                let effective_max_follow_distance =
                    max_follow_distance.or(Some(config.nostr.social_graph_crawl_depth));
                run_socialgraph_index_from_cli(
                    data_dir,
                    SocialGraphIndexOptions {
                        warm_graph_for: Duration::from_secs(warm_secs),
                        graph_crawl_depth: effective_crawl_depth,
                        full_graph_recrawl,
                        max_events_seen,
                        max_authors,
                        max_follow_distance: effective_max_follow_distance,
                        max_live_bytes: max_live_mb.saturating_mul(1024 * 1024),
                        author_batch_size,
                        concurrent_batches,
                        per_author_event_limit,
                        per_author_live_bytes,
                        fetch_timeout: Duration::from_secs(fetch_timeout_secs),
                        relay_event_max_bytes,
                        global_relay_scan,
                        author_allowlist_url,
                        negentropy_only,
                        relay_page_size,
                        max_relay_pages,
                        kinds: (!kinds.is_empty()).then_some(kinds),
                        relays: (!relays.is_empty()).then_some(relays),
                    },
                )
                .await?;
            }
        },
        Commands::Profile {
            name,
            about,
            picture,
        } => {
            update_profile(name, about, picture).await?;
        }
        Commands::Push {
            cid: cid_input,
            server,
        } => {
            // Resolve npub/repo or htree:// URLs to CID
            let resolved = resolve_cid_input(&cid_input).await?;
            let cid = resolved.cid.to_string();
            push_to_blossom(&data_dir, &cid, server).await?;
        }
        Commands::Storage { command } => {
            // Load config
            let config = Config::load()?;

            // Use CLI data_dir if provided, otherwise use config's data_dir
            let data_dir = cli
                .data_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from(&config.storage.data_dir));

            let max_size_bytes = config.storage.max_size_gb * 1024 * 1024 * 1024;
            let store =
                HashtreeStore::with_options(&data_dir, config.storage.s3.as_ref(), max_size_bytes)?;

            match command {
                StorageCommands::Stats => {
                    let stats = store.get_storage_stats()?;
                    let by_priority = store.storage_by_priority()?;
                    let tracked = store.tracked_size()?;
                    let trees = store.list_indexed_trees()?;

                    println!("Storage Statistics:");
                    println!(
                        "  Max size:     {} GB ({} bytes)",
                        config.storage.max_size_gb, max_size_bytes
                    );
                    println!(
                        "  Total bytes:  {} ({:.2} GB)",
                        stats.total_bytes,
                        stats.total_bytes as f64 / 1024.0 / 1024.0 / 1024.0
                    );
                    println!(
                        "  Tracked:      {} ({:.2} GB)",
                        tracked,
                        tracked as f64 / 1024.0 / 1024.0 / 1024.0
                    );
                    println!("  Total DAGs:   {}", stats.total_dags);
                    println!("  Pinned DAGs:  {}", stats.pinned_dags);
                    println!("  Indexed trees: {}", trees.len());
                    println!();
                    println!("Usage by priority:");
                    println!(
                        "  Own (255):      {} ({:.2} MB)",
                        by_priority.own,
                        by_priority.own as f64 / 1024.0 / 1024.0
                    );
                    println!(
                        "  Followed (128): {} ({:.2} MB)",
                        by_priority.followed,
                        by_priority.followed as f64 / 1024.0 / 1024.0
                    );
                    println!(
                        "  Other (64):     {} ({:.2} MB)",
                        by_priority.other,
                        by_priority.other as f64 / 1024.0 / 1024.0
                    );

                    let utilization = if max_size_bytes > 0 {
                        (tracked as f64 / max_size_bytes as f64) * 100.0
                    } else {
                        0.0
                    };
                    println!();
                    println!("Utilization: {:.1}%", utilization);
                }
                StorageCommands::Trees => {
                    use hashtree_core::to_hex;
                    let trees = store.list_indexed_trees()?;

                    if trees.is_empty() {
                        println!("No indexed trees");
                    } else {
                        println!("Indexed trees ({}):", trees.len());
                        for (root_hash, meta) in trees {
                            let root_hex = to_hex(&root_hash);
                            let priority_str = match meta.priority {
                                255 => "own",
                                128 => "followed",
                                _ => "other",
                            };
                            let name = meta.name.as_deref().unwrap_or("<unnamed>");
                            let synced = chrono_humanize_timestamp(meta.synced_at);
                            println!(
                                "  {}... {} ({}) - {} - {} bytes - {}",
                                &root_hex[..12],
                                name,
                                priority_str,
                                &meta.owner[..12.min(meta.owner.len())],
                                meta.total_size,
                                synced
                            );
                        }
                    }
                }
                StorageCommands::Evict => {
                    println!("Running eviction...");
                    let freed = store.evict_if_needed()?;
                    if freed > 0 {
                        println!(
                            "Evicted {} bytes ({:.2} MB)",
                            freed,
                            freed as f64 / 1024.0 / 1024.0
                        );
                    } else {
                        println!("No eviction needed (storage under limit)");
                    }
                }
                StorageCommands::Verify { delete, r2 } => {
                    println!("Verifying blob integrity...");
                    if !delete {
                        println!(
                            "(dry-run mode - use --delete to actually remove corrupted entries)"
                        );
                    }
                    println!();

                    // Verify LMDB
                    let lmdb_result = store.verify_lmdb_integrity(delete)?;
                    println!("LMDB verification:");
                    println!("  Total blobs:     {}", lmdb_result.total);
                    println!("  Valid:           {}", lmdb_result.valid);
                    println!("  Corrupted:       {}", lmdb_result.corrupted);
                    if delete {
                        println!("  Deleted:         {}", lmdb_result.deleted);
                    }
                    println!();

                    // Verify R2 if requested
                    if r2 {
                        println!("Verifying R2 storage (this may take a while)...");
                        match store.verify_r2_integrity(delete).await {
                            Ok(r2_result) => {
                                println!("R2 verification:");
                                println!("  Total objects:   {}", r2_result.total);
                                println!("  Valid:           {}", r2_result.valid);
                                println!("  Corrupted:       {}", r2_result.corrupted);
                                if delete {
                                    println!("  Deleted:         {}", r2_result.deleted);
                                }
                            }
                            Err(e) => {
                                println!("R2 verification failed: {}", e);
                            }
                        }
                    }

                    let total_corrupted = lmdb_result.corrupted;
                    if total_corrupted > 0 {
                        println!();
                        if delete {
                            println!(
                                "Cleanup complete. Removed {} corrupted entries.",
                                total_corrupted
                            );
                        } else {
                            println!(
                                "Found {} corrupted entries. Run with --delete to remove them.",
                                total_corrupted
                            );
                        }
                    } else {
                        println!("All blobs verified successfully!");
                    }
                }
            }
        }
        Commands::Peer { addr } => {
            list_peers(&addr).await?;
        }
        Commands::Cashu { command } => {
            run_cashu_helper(&data_dir, &command)?;
        }
        Commands::Pr { command } => match command {
            PrCommands::Create {
                repo,
                title,
                description,
                branch,
                target_branch,
                clone_url,
            } => {
                super::pr::create_pr(
                    repo.as_deref(),
                    &title,
                    description.as_deref(),
                    branch.as_deref(),
                    &target_branch,
                    clone_url.as_deref(),
                )
                .await?;
            }
            PrCommands::List { repo, state } => {
                super::pr::list_prs(repo.as_deref(), state).await?;
            }
        },
        Commands::Repos { owner } => {
            super::repos::list_repos(owner.as_deref()).await?;
        }
    }

    Ok(())
}

pub(crate) fn format_cid_for_display(cid: &Cid) -> String {
    hashtree_core::nhash_encode_full(&NHashData {
        hash: cid.hash,
        decrypt_key: cid.key,
    })
    .unwrap_or_else(|_| cid.to_string())
}

fn encode_hash_route_segment(segment: &str) -> String {
    let mut encoded = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            _ => {
                encoded.push('%');
                encoded.push_str(&format!("{byte:02X}"));
            }
        }
    }
    encoded
}

pub(crate) fn build_files_iris_to_url_for_add_route(route: &str) -> String {
    let segments = route
        .trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(encode_hash_route_segment)
        .collect::<Vec<_>>();

    if segments.is_empty() {
        IRIS_FILES_WEB_BASE_URL.to_string()
    } else {
        format!("{IRIS_FILES_WEB_BASE_URL}/#/{}", segments.join("/"))
    }
}

pub(crate) fn build_files_iris_to_url_for_published_ref(
    owner_npub: &str,
    ref_name: &str,
) -> String {
    build_files_iris_to_url_for_published_target(owner_npub, ref_name, None, None)
}

pub(crate) fn build_files_iris_to_url_for_published_target(
    owner_npub: &str,
    ref_name: &str,
    path: Option<&str>,
    link_key: Option<&str>,
) -> String {
    let owner = encode_hash_route_segment(owner_npub.trim());
    let reference = encode_hash_route_segment(ref_name.trim_matches('/'));
    let mut url = format!("{IRIS_FILES_WEB_BASE_URL}/#/{owner}/{reference}");

    if let Some(path) = path {
        let encoded_path = path
            .trim_matches('/')
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(encode_hash_route_segment)
            .collect::<Vec<_>>()
            .join("/");
        if !encoded_path.is_empty() {
            url.push('/');
            url.push_str(&encoded_path);
        }
    }

    if let Some(link_key) = link_key {
        if !link_key.is_empty() {
            url.push_str("?k=");
            url.push_str(link_key);
        }
    }

    url
}

pub(crate) fn build_sites_iris_to_url_for_add_route(route: &str) -> String {
    let segments = route
        .trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(encode_hash_route_segment)
        .collect::<Vec<_>>();

    if segments.is_empty() {
        IRIS_SITES_WEB_BASE_URL.to_string()
    } else {
        format!("{IRIS_SITES_WEB_BASE_URL}/#/{}", segments.join("/"))
    }
}

pub(crate) fn build_sites_iris_to_url_for_published_ref(
    owner_npub: &str,
    ref_name: &str,
    entry_path: &str,
) -> String {
    let owner = encode_hash_route_segment(owner_npub.trim());
    let reference = encode_hash_route_segment(ref_name.trim_matches('/'));
    let entry = entry_path
        .trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(encode_hash_route_segment)
        .collect::<Vec<_>>()
        .join("/");
    format!("{IRIS_SITES_WEB_BASE_URL}/#/{owner}/{reference}/{entry}?reload=1")
}

pub(crate) fn detect_site_entry_for_path(path: &Path, is_dir: bool) -> Option<String> {
    if is_dir {
        let mut index_htm: Option<String> = None;
        let entries = std::fs::read_dir(path).ok()?;
        for entry in entries.flatten() {
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            if metadata.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            match name.to_ascii_lowercase().as_str() {
                "index.html" => return Some(name),
                "index.htm" => {
                    if index_htm.is_none() {
                        index_htm = Some(name);
                    }
                }
                _ => {}
            }
        }
        return index_htm;
    }

    let name = path.file_name()?.to_string_lossy().to_string();
    match name.to_ascii_lowercase().rsplit_once('.') {
        Some((_, "html" | "htm")) => Some(name),
        _ => None,
    }
}

async fn resolve_info_target(
    store: &Arc<HashtreeStore>,
    fetcher: &Fetcher,
    root_cid: &Cid,
    path: Option<&str>,
) -> Result<Cid> {
    fetcher.fetch_cid_tree(store, None, root_cid).await?;

    let Some(path) = path else {
        return Ok(root_cid.clone());
    };

    if store.get_chunk(&root_cid.hash)?.is_none() {
        return Ok(root_cid.clone());
    }

    let target_cid = store
        .resolve_path(root_cid, path)?
        .ok_or_else(|| anyhow::anyhow!("Path not found in directory: {}", path))?;

    fetcher.fetch_cid_tree(store, None, &target_cid).await?;
    Ok(target_cid)
}

async fn print_info_for_cid(store: &Arc<HashtreeStore>, cid: &Cid) -> Result<bool> {
    use hashtree_core::to_hex;

    if store.get_chunk(&cid.hash)?.is_none() {
        return Ok(false);
    }

    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());
    let total_size = tree
        .get_size_cid(cid)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get size: {}", e))?;
    let is_directory = tree
        .is_dir(cid)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to inspect directory: {}", e))?;
    let node = if is_directory {
        tree.get_directory_node(cid)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get directory node: {}", e))?
    } else {
        tree.get_node(cid)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get tree node: {}", e))?
    };

    println!("Hash: {}", format_cid_for_display(cid));
    println!("Pinned: {}", store.is_pinned(&cid.hash)?);
    println!("Total size: {} bytes", total_size);

    if is_directory {
        println!("Directory: true");
        let entries = tree
            .list_directory(cid)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to list directory: {}", e))?;
        println!("\nDirectory contents:");
        for entry in entries {
            let type_str = if entry.link_type.is_tree() {
                "dir"
            } else {
                "file"
            };
            let entry_cid = Cid {
                hash: entry.hash,
                key: entry.key,
            };
            println!(
                "  [{}] {} -> {} ({} bytes)",
                type_str,
                entry.name,
                format_cid_for_display(&entry_cid),
                entry.size
            );
        }
    } else if let Some(node) = &node {
        let is_chunked = !node.links.is_empty();
        println!("Chunked: {}", is_chunked);

        if is_chunked {
            println!("Chunks: {}", node.links.len());
            println!("\nChunk details:");
            for (i, link) in node.links.iter().enumerate() {
                println!("  [{}] {} ({} bytes)", i, to_hex(&link.hash), link.size);
            }
        }
    } else {
        println!("Chunked: false");
    }

    if let Some(node) = node {
        println!("\nTree node info:");
        println!("  Links: {}", node.links.len());
        for (i, link) in node.links.iter().enumerate() {
            let name = link.name.as_deref().unwrap_or("<unnamed>");
            println!(
                "    [{}] {} -> {} ({} bytes)",
                i,
                name,
                to_hex(&link.hash),
                link.size
            );
        }
    }

    Ok(true)
}
