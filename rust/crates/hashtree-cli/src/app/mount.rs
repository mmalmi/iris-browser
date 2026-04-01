use anyhow::{Context, Result};
use async_trait::async_trait;
use hashtree_cli::{Config, HashtreeStore, NostrResolverConfig, NostrRootResolver, RootResolver};
use hashtree_fuse::{FsError as FuseFsError, HashtreeFuse, RootPublisher};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use super::mount_publish::{MountPublishQueue, PublishSink, MOUNT_PUBLISH_DEBOUNCE};
use super::mount_target::{
    create_mountpoint_dir, derive_implicit_mountpoint, normalize_mount_target_for_resolution,
};
use super::resolve::{parse_published_target, resolve_cid_input_with_opts, ResolveOptions};
use super::run::build_files_iris_to_url_for_published_target;

struct MountVisibility {
    visibility: hashtree_core::TreeVisibility,
    link_key: Option<[u8; 32]>,
}

fn parse_mount_visibility(
    visibility: Option<String>,
    link_key: Option<String>,
    private: bool,
    fragment: Option<&str>,
) -> Result<MountVisibility> {
    use hashtree_core::TreeVisibility;

    let mut resolved_visibility: Option<TreeVisibility> = None;
    let mut resolved_link_key: Option<[u8; 32]> = None;

    if let Some(fragment) = fragment {
        if fragment == "private" {
            resolved_visibility = Some(TreeVisibility::Private);
        } else if fragment == "link-visible" {
            resolved_visibility = Some(TreeVisibility::LinkVisible);
        } else if let Some(hex_key) = fragment.strip_prefix("k=") {
            resolved_visibility = Some(TreeVisibility::LinkVisible);
            resolved_link_key = Some(
                hashtree_core::key_from_hex(hex_key)
                    .map_err(|e| anyhow::anyhow!("Invalid link key: {}", e))?,
            );
        }
    }

    if let Some(vis) = visibility {
        let parsed = TreeVisibility::from_str(&vis)
            .map_err(|e| anyhow::anyhow!("Invalid visibility: {}", e))?;
        if let Some(existing) = resolved_visibility {
            if existing != parsed {
                anyhow::bail!("Conflicting visibility options");
            }
        }
        resolved_visibility = Some(parsed);
    }

    if let Some(link_key) = link_key {
        let parsed = hashtree_core::key_from_hex(&link_key)
            .map_err(|e| anyhow::anyhow!("Invalid link key: {}", e))?;
        if let Some(existing) = resolved_link_key {
            if existing != parsed {
                anyhow::bail!("Conflicting link key options");
            }
        }
        resolved_link_key = Some(parsed);
        if let Some(existing) = resolved_visibility {
            if existing != TreeVisibility::LinkVisible {
                anyhow::bail!("Link key only applies to link-visible trees");
            }
        }
        resolved_visibility = Some(TreeVisibility::LinkVisible);
    }

    if private {
        if let Some(existing) = resolved_visibility {
            if existing != TreeVisibility::Private {
                anyhow::bail!("Conflicting visibility options");
            }
        }
        resolved_visibility = Some(TreeVisibility::Private);
    }

    let visibility = resolved_visibility.unwrap_or(TreeVisibility::Public);
    if visibility == TreeVisibility::LinkVisible && resolved_link_key.is_none() {
        anyhow::bail!("Link-visible trees require a link key");
    }
    if visibility == TreeVisibility::Private && resolved_link_key.is_some() {
        anyhow::bail!("Private trees cannot use a link key");
    }

    Ok(MountVisibility {
        visibility,
        link_key: resolved_link_key,
    })
}

struct NostrPublishSink {
    resolver: NostrRootResolver,
    key: String,
    visibility: hashtree_core::TreeVisibility,
    link_key: Option<[u8; 32]>,
}

#[async_trait]
impl PublishSink for NostrPublishSink {
    async fn publish(&self, cid: &hashtree_core::Cid) -> Result<()> {
        let published = match self.visibility {
            hashtree_core::TreeVisibility::Public => self.resolver.publish(&self.key, cid).await,
            hashtree_core::TreeVisibility::LinkVisible => {
                let Some(link_key) = self.link_key else {
                    anyhow::bail!("Missing link key");
                };
                self.resolver
                    .publish_shared(&self.key, cid, &link_key)
                    .await
            }
            hashtree_core::TreeVisibility::Private => {
                self.resolver.publish_private(&self.key, cid).await
            }
        }
        .context("Failed to publish mounted root")?;

        if !published {
            anyhow::bail!("Publish returned false");
        }

        Ok(())
    }
}

struct QueueingRootPublisher<Sink, StoreT>
where
    Sink: PublishSink + 'static,
    StoreT: hashtree_core::Store + 'static,
{
    queue: Arc<MountPublishQueue<Sink, StoreT>>,
}

impl<Sink, StoreT> RootPublisher for QueueingRootPublisher<Sink, StoreT>
where
    Sink: PublishSink + 'static,
    StoreT: hashtree_core::Store + 'static,
{
    fn publish(&self, cid: &hashtree_core::Cid) -> Result<(), FuseFsError> {
        self.queue
            .enqueue(cid.clone())
            .map_err(|e| FuseFsError::Publish(e.to_string()))?;
        Ok(())
    }
}

pub(crate) async fn mount_fuse(
    target: String,
    mountpoint: Option<PathBuf>,
    visibility: Option<String>,
    link_key: Option<String>,
    private: bool,
    relays: Option<String>,
    allow_other: bool,
    data_dir: PathBuf,
) -> Result<()> {
    let (mountpoint, implicit_mountpoint) = match mountpoint {
        Some(path) => (path, false),
        None => (
            derive_implicit_mountpoint(&std::env::current_dir()?, &target)?,
            true,
        ),
    };

    let target = target.strip_prefix("htree://").unwrap_or(&target);
    let (base, fragment) = match target.split_once('#') {
        Some((base, fragment)) => (base, Some(fragment)),
        None => (target, None),
    };
    let base = normalize_mount_target_for_resolution(base)?;

    let MountVisibility {
        visibility: mount_visibility,
        link_key: mount_link_key,
    } = parse_mount_visibility(visibility, link_key, private, fragment)?;

    let config = Config::load().unwrap_or_default();
    let relays = if let Some(relays) = relays {
        relays.split(',').map(|s| s.trim().to_string()).collect()
    } else {
        config.nostr.relays.clone()
    };

    let mut opts = ResolveOptions::default();
    opts.link_key = mount_link_key;
    opts.private = mount_visibility == hashtree_core::TreeVisibility::Private;
    opts.relays = Some(relays);

    if opts.private {
        let keys =
            hashtree_cli::config::read_keys().context("Private mounts require a local nsec key")?;
        opts.secret_key = Some(keys);
    }

    let resolved = resolve_cid_input_with_opts(&base, &opts).await?;
    let published_target = parse_published_target(&base);
    let nostr_key = published_target
        .as_ref()
        .map(|target| format!("{}/{}", target.npub, target.tree_name));

    let max_size_bytes = config.storage.max_size_gb * 1024 * 1024 * 1024;
    let store = Arc::new(HashtreeStore::with_options(
        &data_dir,
        config.storage.s3.as_ref(),
        max_size_bytes,
    )?);
    let store_arc = store.store_arc();

    let mut root_cid = resolved.cid.clone();
    if let Some(path) = resolved.path.clone() {
        let tree =
            hashtree_core::HashTree::new(hashtree_core::HashTreeConfig::new(store_arc.clone()));
        let Some(path_cid) = tree.resolve(&root_cid, &path).await? else {
            anyhow::bail!("Path not found: {}", path);
        };
        let is_dir = tree.get_directory_node(&path_cid).await?.is_some();
        if !is_dir {
            anyhow::bail!("Path is not a directory: {}", path);
        }
        root_cid = path_cid;
    }

    let link_key_hex = mount_link_key.map(hex::encode);
    let publish_queue = if let Some(nostr_key) = nostr_key {
        let keys = hashtree_cli::config::read_keys().context("Failed to read nostr keys")?;
        let mut resolver_config = NostrResolverConfig::default();
        if let Some(relays) = opts.relays.clone() {
            resolver_config.relays = relays;
        }
        resolver_config.secret_key = Some(keys.clone());
        let resolver = NostrRootResolver::new(resolver_config)
            .await
            .context("Failed to create nostr resolver")?;

        let published_target = published_target
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Invalid nostr key: {}", nostr_key))?;
        let pubkey_bytes = hashtree_cli::config::parse_npub(&published_target.npub)?;
        if keys.public_key().to_bytes() != pubkey_bytes {
            anyhow::bail!("Nostr key does not match mounted npub");
        }
        let pubkey_hex = hex::encode(pubkey_bytes);
        let mounted_path = published_target
            .path
            .as_deref()
            .map(|path| {
                path.split('/')
                    .filter(|segment| !segment.is_empty())
                    .map(|segment| segment.to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let tree_name = published_target.tree_name.clone();
        let visibility_str = mount_visibility.as_str().to_string();
        let publish_sink = Arc::new(NostrPublishSink {
            resolver,
            key: nostr_key,
            visibility: mount_visibility,
            link_key: mount_link_key,
        });
        let success_store = store.clone();
        let success_hook = Arc::new(move |cid: &hashtree_core::Cid| {
            let key_hex = cid.key.map(hex::encode);
            let updated_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            if let Err(error) = success_store.set_cached_root(
                &pubkey_hex,
                &tree_name,
                &hashtree_core::to_hex(&cid.hash),
                key_hex.as_deref(),
                &visibility_str,
                updated_at,
            ) {
                eprintln!("Failed to cache mounted root publish: {error}");
            }
        });

        let queue = Arc::new(MountPublishQueue::new(
            publish_sink,
            store_arc.clone(),
            resolved.cid.clone(),
            mounted_path,
            MOUNT_PUBLISH_DEBOUNCE,
            Some(success_hook),
        ));

        Some(queue)
    } else {
        None
    };

    println!("mounted {}", mountpoint.display());
    if let Some(target) = published_target.as_ref() {
        println!(
            "  files: {}",
            build_files_iris_to_url_for_published_target(
                &target.npub,
                &target.tree_name,
                target.path.as_deref(),
                link_key_hex.as_deref(),
            )
        );
        println!(
            "  publish: updates debounce for ~{} ms",
            MOUNT_PUBLISH_DEBOUNCE.as_millis()
        );
    }

    let publisher: Option<Arc<dyn RootPublisher>> = publish_queue.as_ref().map(|queue| {
        Arc::new(QueueingRootPublisher {
            queue: queue.clone(),
        }) as Arc<dyn RootPublisher>
    });

    let fs = HashtreeFuse::new_with_publisher(store_arc, root_cid, publisher)?;
    let mut options = vec![
        fuser::MountOption::FSName("hashtree".to_string()),
        fuser::MountOption::DefaultPermissions,
    ];
    if allow_other {
        options.push(fuser::MountOption::AllowOther);
    }

    if implicit_mountpoint {
        create_mountpoint_dir(&mountpoint)?;
    }

    let mount_result = fs.mount(mountpoint.clone(), &options);
    if mount_result.is_err() && implicit_mountpoint {
        let _ = std::fs::remove_dir(&mountpoint);
    }
    mount_result?;
    if let Some(queue) = publish_queue {
        queue.shutdown().await?;
    }
    Ok(())
}
