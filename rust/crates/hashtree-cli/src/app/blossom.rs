use anyhow::{Context, Result};
use futures::executor::block_on as sync_block_on;
use std::collections::HashSet;
use std::path::PathBuf;

use hashtree_cli::config::ensure_keys_string;
use hashtree_cli::HashtreeStore;
use hashtree_core::{Cid, HashTree, HashTreeConfig, Link};

fn parse_root_cid(cid_str: &str) -> Result<Cid> {
    Cid::parse(cid_str).map_err(|e| anyhow::anyhow!("Invalid CID '{}': {}", cid_str, e))
}

fn child_cid(parent: &Cid, link: &Link) -> Cid {
    let inherits_parent_key = link
        .name
        .as_deref()
        .map(|name| {
            name.starts_with("_chunk_")
                || (name.starts_with('_') && name.chars().count() == 2 && link.link_type.is_tree())
        })
        .unwrap_or(false);

    Cid {
        hash: link.hash,
        key: link.key.or(if inherits_parent_key {
            parent.key
        } else {
            None
        }),
    }
}

fn collect_blocks_for_push(store: &HashtreeStore, root_cid: Cid) -> Result<Vec<Vec<u8>>> {
    let mut blocks_to_push = Vec::new();
    let mut visited: HashSet<[u8; 32]> = HashSet::new();
    let mut queue = vec![root_cid];
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());

    while let Some(cid) = queue.pop() {
        if !visited.insert(cid.hash) {
            continue;
        }

        if let Some(data) = store.get_blob(&cid.hash)? {
            blocks_to_push.push(data);
        }

        let node = sync_block_on(async { tree.get_node(&cid).await })
            .map_err(|e| anyhow::anyhow!("Failed to inspect {}: {}", cid, e))?;

        if let Some(node) = node {
            for link in &node.links {
                if !visited.contains(&link.hash) {
                    queue.push(child_cid(&cid, link));
                }
            }
        }
    }

    Ok(blocks_to_push)
}

/// Push content to Blossom servers.
pub(crate) async fn push_to_blossom(
    data_dir: &PathBuf,
    cid_str: &str,
    server_override: Option<String>,
) -> Result<()> {
    use hashtree_blossom::BlossomClient;
    use nostr::Keys;

    // Ensure nsec exists for signing
    let (nsec_str, _) = ensure_keys_string()?;
    let keys = Keys::parse(&nsec_str).context("Failed to parse nsec")?;

    // Create client (optionally with server override)
    let client = if let Some(server) = server_override {
        BlossomClient::new(keys).with_write_servers(vec![server])
    } else {
        BlossomClient::new(keys)
    };

    if client.write_servers().is_empty() {
        anyhow::bail!(
            "No file servers configured. Use --server or add write_servers to config.toml"
        );
    }

    // Open local store
    let store = HashtreeStore::new(data_dir)?;

    // Collect all blocks to push (walk the DAG)
    println!("Collecting blocks...");
    let root_cid = parse_root_cid(cid_str)?;
    let blocks_to_push = collect_blocks_for_push(&store, root_cid)?;

    println!("Found {} blocks to push", blocks_to_push.len());

    let mut uploaded = 0;
    let mut skipped = 0;
    let mut errors = 0;

    for data in &blocks_to_push {
        match client.upload_if_missing(data).await {
            Ok((_hash, was_uploaded)) => {
                if was_uploaded {
                    uploaded += 1;
                } else {
                    skipped += 1;
                }
            }
            Err(e) => {
                eprintln!("  Upload error: {}", e);
                errors += 1;
            }
        }
    }

    println!(
        "\nUploaded: {}, Skipped: {}, Errors: {}",
        uploaded, skipped, errors
    );
    println!("Done!");
    Ok(())
}

/// Push tree to Blossom servers using BlossomClient.
pub(crate) async fn background_blossom_push(
    data_dir: &PathBuf,
    cid_str: &str,
    servers: &[String],
) -> Result<()> {
    use hashtree_blossom::BlossomClient;
    use nostr::Keys;

    // Ensure nsec exists for signing
    let (nsec_str, _) = ensure_keys_string()?;
    let keys = Keys::parse(&nsec_str).context("Failed to parse nsec")?;

    // Open local store
    let store = HashtreeStore::new(data_dir)?;
    let root_cid = parse_root_cid(cid_str)?;
    let blocks_to_push = collect_blocks_for_push(&store, root_cid)?;

    if blocks_to_push.is_empty() {
        return Ok(());
    }

    let client = if servers.is_empty() {
        BlossomClient::new(keys)
    } else {
        BlossomClient::new(keys).with_write_servers(servers.to_vec())
    };
    let mut total_uploaded = 0;
    let mut total_skipped = 0;
    let mut total_errors = 0;
    let mut last_error = None;

    for data in &blocks_to_push {
        match client.upload_if_missing(data).await {
            Ok((_hash, was_uploaded)) => {
                if was_uploaded {
                    total_uploaded += 1;
                } else {
                    total_skipped += 1;
                }
            }
            Err(e) => {
                tracing::warn!("Blossom upload failed: {}", e);
                total_errors += 1;
                last_error = Some(e.to_string());
            }
        }
    }

    if total_uploaded > 0 || total_skipped > 0 {
        println!(
            "  file servers: {} uploaded, {} already exist",
            total_uploaded, total_skipped
        );
    }

    if total_errors > 0 {
        let detail = last_error
            .as_deref()
            .map(|err| format!(" (last error: {err})"))
            .unwrap_or_default();
        anyhow::bail!(
            "failed to upload {} blob(s) to configured file servers{}",
            total_errors,
            detail
        );
    }

    Ok(())
}
