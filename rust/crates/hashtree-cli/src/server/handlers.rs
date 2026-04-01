use super::auth::{AppState, CachedResolvedPathEntry, CachedTreeRootEntry, LookupResult};
use super::mime::get_mime_type;
use super::resolve_virtual_tree_host;
use super::ui::root_page;
use crate::socialgraph;
use crate::webrtc::{
    build_root_filter, pick_latest_event, root_event_from_peer, ConnectionState, PeerRootEvent,
    WebRTCState,
};
use axum::{
    body::Body,
    extract::{ConnectInfo, Multipart, OriginalUri, Path, Query, State},
    http::{header, Response, StatusCode},
    response::{IntoResponse, Json},
};
use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::{self, FuturesUnordered, StreamExt};
use futures::FutureExt;
use hashtree_core::{
    from_hex, nhash_decode, to_hex, Cid, HashTree, HashTreeConfig, HashTreeError, LinkType, Store,
    TreeEntry,
};
use hashtree_resolver::{
    nostr::{NostrResolverConfig, NostrRootResolver},
    RootResolver,
};
use serde::Deserialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

const CID_RANGE_STREAM_CHUNK_SIZE: u64 = 256 * 1024;

pub async fn serve_root() -> impl IntoResponse {
    root_page()
}

pub async fn serve_root_or_virtual_host(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
    headers: axum::http::HeaderMap,
    connect_info: ConnectInfo<std::net::SocketAddr>,
) -> impl IntoResponse {
    let Some(virtual_root) = request_virtual_tree_root(&headers) else {
        return serve_root().await.into_response();
    };

    serve_virtual_tree_host_request(&state, &virtual_root, None, params, headers, connect_info)
        .await
}

pub async fn htree_test() -> impl IntoResponse {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(Body::from("ok"))
        .unwrap()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VirtualTreeRoot {
    Immutable { nhash: String },
    Mutable { npub: String, treename: String },
}

fn request_virtual_tree_root(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(resolve_virtual_tree_host)
}

fn parse_virtual_tree_root(root: &str) -> Option<VirtualTreeRoot> {
    if let Some(parsed) = parse_mutable_htree_request_path(root) {
        if parsed.path.is_none() {
            return Some(VirtualTreeRoot::Mutable {
                npub: parsed.npub,
                treename: parsed.treename,
            });
        }
    }

    let parsed = reqwest::Url::parse(&format!("http://virtual-host{}", root)).ok()?;
    let segments: Vec<String> = parsed
        .path_segments()?
        .map(|segment| segment.to_string())
        .collect();

    match segments.as_slice() {
        [prefix, nhash] if prefix == "htree" && nhash.starts_with("nhash1") => {
            Some(VirtualTreeRoot::Immutable {
                nhash: nhash.clone(),
            })
        }
        [prefix, npub, treename]
            if prefix == "htree" && npub.starts_with("npub1") && !treename.is_empty() =>
        {
            Some(VirtualTreeRoot::Mutable {
                npub: npub.clone(),
                treename: treename.clone(),
            })
        }
        _ => None,
    }
}

fn should_fallback_to_virtual_host_index(
    requested_path: Option<&str>,
    headers: &axum::http::HeaderMap,
) -> bool {
    let accepts_html = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.contains("text/html"))
        .unwrap_or(false);

    if !accepts_html {
        return false;
    }

    let Some(path) = requested_path else {
        return true;
    };

    let tail = path.rsplit('/').next().unwrap_or(path);
    !tail.contains('.')
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedMutableHtreeRequestPath {
    npub: String,
    treename: String,
    path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedTreeRequestPath {
    pubkey: String,
    treename: String,
    path: Option<String>,
}

fn decode_uri_path_segment(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hi = bytes[index + 1] as char;
            let lo = bytes[index + 2] as char;
            if let (Some(hi), Some(lo)) = (hi.to_digit(16), lo.to_digit(16)) {
                decoded.push(((hi << 4) | lo) as u8);
                index += 3;
                continue;
            }
        }

        decoded.push(bytes[index]);
        index += 1;
    }

    String::from_utf8(decoded).unwrap_or_else(|_| segment.to_string())
}

fn parse_mutable_htree_request_path(raw_path: &str) -> Option<ParsedMutableHtreeRequestPath> {
    let parsed = reqwest::Url::parse(&format!("http://htree{}", raw_path)).ok()?;
    let segments: Vec<String> = parsed
        .path_segments()?
        .map(decode_uri_path_segment)
        .collect();

    let parsed = parse_tree_request_segments(&segments, &["htree"])?;
    if !parsed.pubkey.starts_with("npub1") {
        return None;
    }

    Some(ParsedMutableHtreeRequestPath {
        npub: parsed.pubkey,
        treename: parsed.treename,
        path: parsed.path,
    })
}

fn parse_tree_request_segments(
    segments: &[String],
    prefix: &[&str],
) -> Option<ParsedTreeRequestPath> {
    if segments.len() < prefix.len() + 2 {
        return None;
    }
    if !segments
        .iter()
        .zip(prefix.iter())
        .all(|(segment, expected)| segment == expected)
    {
        return None;
    }

    let pubkey = segments.get(prefix.len())?.clone();
    let treename = segments.get(prefix.len() + 1)?.clone();
    if pubkey.is_empty() || treename.is_empty() {
        return None;
    }

    let path = (segments.len() > prefix.len() + 2)
        .then(|| segments[prefix.len() + 2..].join("/"))
        .filter(|value| !value.is_empty());

    Some(ParsedTreeRequestPath {
        pubkey,
        treename,
        path,
    })
}

fn parse_resolve_request_path(raw_path: &str) -> Option<ParsedTreeRequestPath> {
    let parsed = reqwest::Url::parse(&format!("http://htree{}", raw_path)).ok()?;
    let segments: Vec<String> = parsed
        .path_segments()?
        .map(decode_uri_path_segment)
        .collect();
    parse_tree_request_segments(&segments, &["n"])
}

fn parse_api_resolve_request_path(raw_path: &str) -> Option<ParsedTreeRequestPath> {
    let parsed = reqwest::Url::parse(&format!("http://htree{}", raw_path)).ok()?;
    let segments: Vec<String> = parsed
        .path_segments()?
        .map(decode_uri_path_segment)
        .collect();
    parse_tree_request_segments(&segments, &["api", "resolve"])
        .or_else(|| parse_tree_request_segments(&segments, &["api", "nostr", "resolve"]))
}

fn parse_bare_npub_request_path(raw_path: &str) -> Option<ParsedMutableHtreeRequestPath> {
    let parsed = reqwest::Url::parse(&format!("http://htree{}", raw_path)).ok()?;
    let segments: Vec<String> = parsed
        .path_segments()?
        .map(decode_uri_path_segment)
        .collect();
    let parsed = parse_tree_request_segments(&segments, &[])?;
    if !parsed.pubkey.starts_with("npub1") {
        return None;
    }

    Some(ParsedMutableHtreeRequestPath {
        npub: parsed.pubkey,
        treename: parsed.treename,
        path: parsed.path,
    })
}

async fn serve_virtual_tree_host_request(
    state: &AppState,
    virtual_root: &str,
    requested_path: Option<String>,
    params: HashMap<String, String>,
    headers: axum::http::HeaderMap,
    connect_info: ConnectInfo<std::net::SocketAddr>,
) -> Response<Body> {
    let Some(root) = parse_virtual_tree_root(virtual_root) else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from("Not found"))
            .unwrap();
    };

    let initial_response = match &root {
        VirtualTreeRoot::Immutable { nhash } => {
            htree_nhash_impl(
                State(state.clone()),
                nhash.clone(),
                requested_path.clone().filter(|path| !path.is_empty()),
                Query(params.clone()),
                headers.clone(),
                ConnectInfo(connect_info.0),
            )
            .await
        }
        VirtualTreeRoot::Mutable { npub, treename } => {
            htree_npub_impl(
                State(state.clone()),
                npub.clone(),
                treename.clone(),
                requested_path.clone().filter(|path| !path.is_empty()),
                Query(params.clone()),
                headers.clone(),
                ConnectInfo(connect_info.0),
            )
            .await
        }
    };

    if initial_response.status() != StatusCode::NOT_FOUND
        || !should_fallback_to_virtual_host_index(requested_path.as_deref(), &headers)
    {
        return initial_response;
    }

    match root {
        VirtualTreeRoot::Immutable { nhash } => {
            htree_nhash_impl(
                State(state.clone()),
                nhash,
                None,
                Query(params),
                headers,
                connect_info,
            )
            .await
        }
        VirtualTreeRoot::Mutable { npub, treename } => {
            htree_npub_impl(
                State(state.clone()),
                npub,
                treename,
                None,
                Query(params),
                headers,
                connect_info,
            )
            .await
        }
    }
}

pub async fn serve_virtual_host_fallback(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Query(params): Query<HashMap<String, String>>,
    headers: axum::http::HeaderMap,
    connect_info: ConnectInfo<std::net::SocketAddr>,
) -> impl IntoResponse {
    let Some(virtual_root) = request_virtual_tree_root(&headers) else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from("Not found"))
            .unwrap();
    };

    serve_virtual_tree_host_request(
        &state,
        &virtual_root,
        Some(uri.path().trim_start_matches('/').to_string()).filter(|path| !path.is_empty()),
        params,
        headers,
        connect_info,
    )
    .await
}

#[derive(Deserialize)]
pub struct CacheTreeRootRequest {
    #[serde(rename = "npub")]
    pub npub: String,
    #[serde(rename = "treeName")]
    pub tree_name: String,
    pub hash: String,
    pub key: Option<String>,
    pub visibility: Option<String>,
}

#[derive(Deserialize)]
pub struct ClearTreeRootCacheRequest {
    #[serde(rename = "npub")]
    pub npub: String,
    #[serde(rename = "treeName")]
    pub tree_name: String,
    pub key: Option<String>,
    pub visibility: Option<String>,
}

pub async fn cache_tree_root(
    State(state): State<AppState>,
    Json(request): Json<CacheTreeRootRequest>,
) -> impl IntoResponse {
    let hash = match from_hex(&request.hash) {
        Ok(value) => value,
        Err(error) => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(format!("Invalid hash: {}", error)))
                .unwrap();
        }
    };

    let key = match request.key {
        Some(value) => match from_hex(&value) {
            Ok(decoded) => Some(decoded),
            Err(error) => {
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                    .body(Body::from(format!("Invalid key: {}", error)))
                    .unwrap();
            }
        },
        None => None,
    };

    let cid = Cid { hash, key };
    let cache_key = cache_tree_root_key(
        &request.npub,
        &request.tree_name,
        request.visibility.as_deref(),
        cid.key,
    );
    put_cached_tree_root(&state, cache_key, cid, "cache", None);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(Body::from("ok"))
        .unwrap()
}

pub async fn clear_tree_root_cache(
    State(state): State<AppState>,
    Json(request): Json<ClearTreeRootCacheRequest>,
) -> impl IntoResponse {
    let key = match request.key {
        Some(value) => match from_hex(&value) {
            Ok(decoded) => Some(decoded),
            Err(error) => {
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                    .body(Body::from(format!("Invalid key: {}", error)))
                    .unwrap();
            }
        },
        None => None,
    };

    let cache_key = cache_tree_root_key(
        &request.npub,
        &request.tree_name,
        request.visibility.as_deref(),
        key,
    );
    remove_cached_tree_root(&state, &cache_key);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(Body::from("ok"))
        .unwrap()
}

async fn list_directory_json(
    state: &AppState,
    cid: &Cid,
    is_immutable: bool,
    is_localhost: bool,
) -> Response<Body> {
    let store = state.store.store_arc();
    let tree = HashTree::new(HashTreeConfig::new(store).public());
    let entries = match list_directory_with_fetch(state, &tree, cid).await {
        Ok(Some(list)) => list,
        Ok(None) => {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from("Directory not found"))
                .unwrap();
        }
        Err(e) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(format!("Error: {}", e)))
                .unwrap();
        }
    };

    let payload = json!({
        "entries": entries.into_iter().map(|entry| {
            json!({
                "name": entry.name,
                "hash": to_hex(&entry.hash),
                "key": entry.key.map(|key| to_hex(&key)),
                "size": entry.size,
                "type": match entry.link_type {
                    LinkType::Blob => "blob",
                    LinkType::File => "file",
                    LinkType::Dir => "dir",
                },
            })
        }).collect::<Vec<_>>(),
    });

    build_json_response(payload, is_immutable, is_localhost)
}

async fn resolve_npub_root(
    key: &str,
    resolver: &NostrRootResolver,
    share_secret: Option<[u8; 32]>,
) -> Result<Cid, hashtree_resolver::ResolverError> {
    if let Some(secret) = share_secret {
        loop {
            if let Some(cid) = resolver.resolve_shared(key, &secret).await? {
                return Ok(cid);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    resolver.resolve_wait(key).await
}

#[derive(Clone)]
struct ResolvedRoot {
    cid: Cid,
    source: &'static str,
    root_event: Option<PeerRootEvent>,
}

fn peer_root_to_cid(root: &PeerRootEvent) -> Option<Cid> {
    let mut cid = Cid::parse(&root.hash).ok()?;
    if cid.key.is_none() {
        cid.key = root
            .key
            .as_deref()
            .and_then(|key_hex| from_hex(key_hex).ok());
    }
    Some(cid)
}

async fn resolve_root_from_local_relay(
    state: &AppState,
    pubkey: &str,
    treename: &str,
) -> Option<PeerRootEvent> {
    let relay = state.nostr_relay.as_ref()?;
    let filter = build_root_filter(pubkey, treename)?;
    let events = relay.query_events(&filter, 50).await;
    let latest = pick_latest_event(events.iter())?;
    root_event_from_peer(latest, "local-relay", treename)
}

async fn resolve_root_offline(
    state: &AppState,
    pubkey: &str,
    treename: &str,
    link_key: Option<[u8; 32]>,
) -> Option<ResolvedRoot> {
    let cache_key = tree_root_cache_key(pubkey, treename, link_key);
    if let Some(mut cached) = get_cached_tree_root(state, &cache_key) {
        if cached.cid.key.is_none() {
            cached.cid.key = link_key;
        }
        return Some(ResolvedRoot {
            cid: cached.cid,
            source: "cache",
            root_event: cached.root_event,
        });
    }

    resolve_root_without_cache(state, pubkey, treename, link_key).await
}

async fn resolve_root_without_cache(
    state: &AppState,
    pubkey: &str,
    treename: &str,
    link_key: Option<[u8; 32]>,
) -> Option<ResolvedRoot> {
    let cache_key = tree_root_cache_key(pubkey, treename, link_key);
    if let Some(root) = resolve_root_from_local_relay(state, pubkey, treename).await {
        if let Some(mut cid) = peer_root_to_cid(&root) {
            if cid.key.is_none() {
                cid.key = link_key;
            }
            put_cached_tree_root(
                state,
                cache_key.clone(),
                cid.clone(),
                "local-relay",
                Some(root.clone()),
            );
            return Some(ResolvedRoot {
                cid,
                source: "local-relay",
                root_event: Some(root),
            });
        }
    }

    if let Some(ref webrtc_state) = state.webrtc_peers {
        if let Some((source, root)) = webrtc_state
            .resolve_root_from_local_buses_with_source(pubkey, treename, Duration::from_secs(2))
            .await
        {
            if let Some(mut cid) = peer_root_to_cid(&root) {
                if cid.key.is_none() {
                    cid.key = link_key;
                }
                put_cached_tree_root(
                    state,
                    cache_key.clone(),
                    cid.clone(),
                    source,
                    Some(root.clone()),
                );
                return Some(ResolvedRoot {
                    cid,
                    source,
                    root_event: Some(root),
                });
            }
        }

        if let Some(root) = webrtc_state
            .resolve_root_from_peers(pubkey, treename, Duration::from_secs(4))
            .await
        {
            if let Some(mut cid) = peer_root_to_cid(&root) {
                if cid.key.is_none() {
                    cid.key = link_key;
                }
                put_cached_tree_root(state, cache_key, cid.clone(), "webrtc", Some(root.clone()));
                return Some(ResolvedRoot {
                    cid,
                    source: "webrtc",
                    root_event: Some(root),
                });
            }
        }
    }

    None
}

fn tree_root_cache_key(npub: &str, treename: &str, link_key: Option<[u8; 32]>) -> String {
    match link_key {
        Some(key) => format!("{}/{}?k={}", npub, treename, to_hex(&key)),
        None => format!("{}/{}", npub, treename),
    }
}

fn cache_public_tree_root(state: &AppState, npub: &str, treename: &str, cid: &Cid) {
    put_cached_tree_root(
        state,
        cache_tree_root_key(npub, treename, Some("public"), cid.key),
        cid.clone(),
        "nostr",
        None,
    );
}

fn query_flag(params: &HashMap<String, String>, name: &str) -> bool {
    params
        .get(name)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn cache_tree_root_key(
    npub: &str,
    treename: &str,
    visibility: Option<&str>,
    key: Option<[u8; 32]>,
) -> String {
    let visibility = visibility.unwrap_or("public");
    let link_key = if visibility == "public" { None } else { key };
    tree_root_cache_key(npub, treename, link_key)
}

fn cid_cache_key(cid: &Cid) -> String {
    match cid.key {
        Some(key) => format!("{}?k={}", to_hex(&cid.hash), to_hex(&key)),
        None => to_hex(&cid.hash),
    }
}

fn resolved_path_cache_key(root_cid: &Cid, path: &str) -> String {
    format!("{}|{}", cid_cache_key(root_cid), path)
}

fn get_cached_lookup<T: Clone>(
    cache: &std::sync::Arc<std::sync::Mutex<super::auth::TimedLruCache<String, LookupResult<T>>>>,
    key: &str,
) -> Option<Option<T>> {
    cache
        .lock()
        .ok()
        .and_then(|mut cache| cache.get_cloned(&key.to_string()))
        .map(LookupResult::into_option)
}

fn put_cached_lookup<T: Clone>(
    cache: &std::sync::Arc<std::sync::Mutex<super::auth::TimedLruCache<String, LookupResult<T>>>>,
    key: String,
    value: Option<T>,
) {
    if let Ok(mut cache) = cache.lock() {
        let cached = LookupResult::from_option(value);
        let ttl = cached.ttl();
        cache.put(key, cached, ttl);
    }
}

fn cached_resolved_path_entry(entry: &ResolvedPathEntry) -> CachedResolvedPathEntry {
    CachedResolvedPathEntry {
        cid: entry.cid.clone(),
        link_type: entry.link_type,
    }
}

fn get_cached_tree_root(state: &AppState, cache_key: &str) -> Option<CachedTreeRootEntry> {
    state
        .tree_root_cache
        .lock()
        .ok()
        .and_then(|cache| cache.get(cache_key).cloned())
}

fn put_cached_tree_root(
    state: &AppState,
    cache_key: String,
    cid: Cid,
    source: &'static str,
    root_event: Option<PeerRootEvent>,
) {
    if let Ok(mut cache) = state.tree_root_cache.lock() {
        cache.insert(
            cache_key,
            CachedTreeRootEntry {
                cid,
                source,
                root_event,
            },
        );
    }
}

fn remove_cached_tree_root(state: &AppState, cache_key: &str) -> bool {
    state
        .tree_root_cache
        .lock()
        .ok()
        .and_then(|mut cache| cache.remove(cache_key))
        .is_some()
}

const DEFAULT_DIRECTORY_INDEXES: [&str; 2] = ["index.html", "index.htm"];

enum DirectoryTarget {
    File { cid: Cid, path: String },
    DirectoryListing { cid: Cid },
}

struct ResolvedPathEntry {
    cid: Cid,
    link_type: LinkType,
}

async fn resolve_directory_index_path<S: Store>(
    state: &AppState,
    tree: &HashTree<S>,
    root_cid: &Cid,
    requested_path: Option<&str>,
) -> Result<Option<String>, String> {
    let base = requested_path
        .map(|path| path.trim_matches('/'))
        .filter(|path| !path.is_empty());

    for candidate in DEFAULT_DIRECTORY_INDEXES {
        let candidate_path = match base {
            Some(base) => format!("{}/{}", base, candidate),
            None => candidate.to_string(),
        };
        if resolve_path_with_fetch(state, tree, root_cid, &candidate_path)
            .await?
            .is_some()
        {
            return Ok(Some(candidate_path));
        }
    }

    Ok(None)
}

async fn resolve_directory_target<S: Store>(
    state: &AppState,
    tree: &HashTree<S>,
    root_cid: &Cid,
    requested_path: Option<String>,
) -> Result<Option<DirectoryTarget>, String> {
    if let Some(path) = requested_path {
        let entry = match resolve_path_with_fetch(state, tree, root_cid, &path).await? {
            Some(entry) => entry,
            None => return Ok(None),
        };

        if entry.link_type == LinkType::Dir {
            if let Some(index_path) =
                resolve_directory_index_path(state, tree, root_cid, Some(&path)).await?
            {
                let index_entry = resolve_path_with_fetch(state, tree, root_cid, &index_path)
                    .await?
                    .ok_or_else(|| format!("Resolved default path missing: {}", index_path))?;
                return Ok(Some(DirectoryTarget::File {
                    cid: index_entry.cid,
                    path: index_path,
                }));
            }

            return Ok(Some(DirectoryTarget::DirectoryListing { cid: entry.cid }));
        }

        return Ok(Some(DirectoryTarget::File {
            cid: entry.cid,
            path,
        }));
    }

    if let Some(index_path) = resolve_directory_index_path(state, tree, root_cid, None).await? {
        let index_entry = resolve_path_with_fetch(state, tree, root_cid, &index_path)
            .await?
            .ok_or_else(|| format!("Resolved default path missing: {}", index_path))?;
        return Ok(Some(DirectoryTarget::File {
            cid: index_entry.cid,
            path: index_path,
        }));
    }

    Ok(Some(DirectoryTarget::DirectoryListing {
        cid: root_cid.clone(),
    }))
}

/// Try to fetch a blob from WebRTC peers and upstream Blossom servers, caching locally.
/// Returns true if the blob was fetched and cached, false otherwise.
async fn fetch_and_cache_blob(state: &AppState, hash: &[u8]) -> bool {
    let hash_hex = hex::encode(hash);
    tracing::info!(
        "[htree-fetch] Trying to fetch blob {} from upstream",
        &hash_hex[..16.min(hash_hex.len())]
    );

    enum FetchResult {
        WebRtc { data: Vec<u8>, peer_id: String },
        Upstream { data: Vec<u8>, server: String },
    }

    let mut fetches: Vec<BoxFuture<'static, Option<FetchResult>>> = Vec::new();

    if let Some(ref webrtc_state) = state.webrtc_peers {
        tracing::info!(
            "[htree-fetch] Querying mesh peers for {}",
            &hash_hex[..16.min(hash_hex.len())]
        );
        let webrtc_state = webrtc_state.clone();
        let peer_hash_hex = hash_hex.clone();
        fetches.push(
            async move {
                let query_hash_hex = peer_hash_hex.clone();
                await_fetch_task("webrtc", &peer_hash_hex, async move {
                    query_webrtc_peers(&webrtc_state, &query_hash_hex).await
                })
                .await
                .map(|(data, peer_id)| FetchResult::WebRtc { data, peer_id })
            }
            .boxed(),
        );
    }

    if !state.upstream_blossom.is_empty() {
        tracing::info!(
            "[htree-fetch] Querying {} Blossom servers for {}",
            state.upstream_blossom.len(),
            &hash_hex[..16.min(hash_hex.len())]
        );
        let upstream_blossom = state.upstream_blossom.clone();
        let upstream_hash_hex = hash_hex.clone();
        fetches.push(
            async move {
                let query_hash_hex = upstream_hash_hex.clone();
                await_fetch_task("upstream", &upstream_hash_hex, async move {
                    query_upstream_blossom(&upstream_blossom, &query_hash_hex).await
                })
                .await
                .map(|(data, server)| FetchResult::Upstream { data, server })
            }
            .boxed(),
        );
    } else {
        tracing::info!("[htree-fetch] No upstream Blossom servers configured");
    }

    if let Some(result) = first_available_fetch(fetches).await {
        match result {
            FetchResult::WebRtc { data, peer_id } => {
                tracing::info!(
                    "[htree-fetch] Got {} bytes from peer {} for {}",
                    data.len(),
                    peer_id,
                    &hash_hex[..16.min(hash_hex.len())]
                );
                if let Err(e) = state.store.put_blob(&data) {
                    tracing::warn!("[htree-fetch] Failed to cache peer data: {}", e);
                }
                return true;
            }
            FetchResult::Upstream { data, server } => {
                tracing::info!(
                    "[htree-fetch] Got {} bytes from upstream {} for {}",
                    data.len(),
                    server,
                    &hash_hex[..16.min(hash_hex.len())]
                );
                if let Err(e) = state.store.put_blob(&data) {
                    tracing::warn!("[htree-fetch] Failed to cache upstream data: {}", e);
                }
                return true;
            }
        }
    }

    if !state.upstream_blossom.is_empty() {
        tracing::info!(
            "[htree-fetch] No upstream had {}",
            &hash_hex[..16.min(hash_hex.len())]
        );
    }

    false
}

async fn await_fetch_task<F, T>(source: &str, hash_hex: &str, future: F) -> Option<T>
where
    F: std::future::Future<Output = Option<T>>,
{
    match std::panic::AssertUnwindSafe(future).catch_unwind().await {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!(
                "[htree-fetch] {} fetch task panicked for {}",
                source,
                &hash_hex[..16.min(hash_hex.len())],
            );
            None
        }
    }
}

async fn first_available_fetch<T>(futures: Vec<BoxFuture<'static, Option<T>>>) -> Option<T> {
    let mut pending = FuturesUnordered::new();
    for future in futures {
        pending.push(future);
    }

    while let Some(result) = pending.next().await {
        if let Some(value) = result {
            return Some(value);
        }
    }

    None
}

async fn ensure_blob_available(state: &AppState, hash: &[u8; 32]) -> Result<bool, String> {
    if state.store.blob_exists(hash).map_err(|e| e.to_string())? {
        return Ok(true);
    }

    let hash_hex = to_hex(hash);
    let fetch = {
        let mut inflight = state.inflight_blob_fetches.lock().await;
        if let Some(existing) = inflight.get(&hash_hex) {
            existing.clone()
        } else {
            let state = state.clone();
            let hash_bytes = *hash;
            let hash_hex_for_task = hash_hex.clone();
            let fetch = async move {
                let fetched = fetch_and_cache_blob(&state, &hash_bytes).await;
                state
                    .inflight_blob_fetches
                    .lock()
                    .await
                    .remove(&hash_hex_for_task);
                fetched
            }
            .boxed()
            .shared();
            inflight.insert(hash_hex.clone(), fetch.clone());
            fetch
        }
    };

    if fetch.await {
        return Ok(true);
    }

    state.store.blob_exists(hash).map_err(|e| e.to_string())
}

async fn fetch_missing_chunk(
    state: &AppState,
    seen_missing: &mut HashSet<String>,
    missing: &str,
) -> Result<bool, String> {
    if !seen_missing.insert(missing.to_string()) {
        return Err(format!("Repeated missing chunk {}", missing));
    }

    let hash =
        from_hex(missing).map_err(|e| format!("Invalid missing chunk hash {}: {}", missing, e))?;
    Ok(fetch_and_cache_blob(state, &hash).await)
}

async fn list_directory_with_fetch<S: Store>(
    state: &AppState,
    tree: &HashTree<S>,
    cid: &Cid,
) -> Result<Option<Vec<TreeEntry>>, String> {
    let cache_key = cid_cache_key(cid);
    if let Some(cached) = get_cached_lookup(&state.directory_listing_cache, &cache_key) {
        return Ok(cached);
    }

    let mut seen_missing = HashSet::new();

    loop {
        if !ensure_blob_available(state, &cid.hash).await? {
            put_cached_lookup(&state.directory_listing_cache, cache_key.clone(), None);
            return Ok(None);
        }

        match tree.list_directory(cid).await {
            Ok(entries) => {
                put_cached_lookup(
                    &state.directory_listing_cache,
                    cache_key.clone(),
                    Some(entries.clone()),
                );
                return Ok(Some(entries));
            }
            Err(HashTreeError::MissingChunk(missing)) => {
                if !fetch_missing_chunk(state, &mut seen_missing, &missing).await? {
                    put_cached_lookup(&state.directory_listing_cache, cache_key.clone(), None);
                    return Ok(None);
                }
            }
            Err(err) => return Err(err.to_string()),
        }
    }
}

async fn resolve_path_with_fetch<S: Store>(
    state: &AppState,
    tree: &HashTree<S>,
    root_cid: &Cid,
    path: &str,
) -> Result<Option<ResolvedPathEntry>, String> {
    let cache_key = resolved_path_cache_key(root_cid, path);
    if let Some(cached) = get_cached_lookup(&state.resolved_path_cache, &cache_key) {
        return Ok(cached.map(|entry| ResolvedPathEntry {
            cid: entry.cid,
            link_type: entry.link_type,
        }));
    }

    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if parts.is_empty() {
        let entry = ResolvedPathEntry {
            cid: root_cid.clone(),
            link_type: LinkType::Dir,
        };
        put_cached_lookup(
            &state.resolved_path_cache,
            cache_key,
            Some(cached_resolved_path_entry(&entry)),
        );
        return Ok(Some(entry));
    }

    let mut current_cid = root_cid.clone();
    let mut current_link_type = LinkType::Dir;

    for part in parts {
        let entries = match list_directory_with_fetch(state, tree, &current_cid).await? {
            Some(entries) => entries,
            None => {
                put_cached_lookup(&state.resolved_path_cache, cache_key, None);
                return Ok(None);
            }
        };

        let Some(entry) = entries.into_iter().find(|entry| entry.name == part) else {
            put_cached_lookup(&state.resolved_path_cache, cache_key, None);
            return Ok(None);
        };

        current_link_type = entry.link_type;
        current_cid = Cid {
            hash: entry.hash,
            key: entry.key,
        };
    }

    let entry = ResolvedPathEntry {
        cid: current_cid,
        link_type: current_link_type,
    };
    put_cached_lookup(
        &state.resolved_path_cache,
        cache_key,
        Some(cached_resolved_path_entry(&entry)),
    );
    Ok(Some(entry))
}

async fn get_cid_with_fetch<S: Store>(
    state: &AppState,
    tree: &HashTree<S>,
    cid: &Cid,
) -> Result<Option<Vec<u8>>, String> {
    let mut seen_missing = HashSet::new();

    loop {
        if !ensure_blob_available(state, &cid.hash).await? {
            return Ok(None);
        }

        match tree.get(cid, None).await {
            Ok(data) => return Ok(data),
            Err(HashTreeError::MissingChunk(missing)) => {
                if !fetch_missing_chunk(state, &mut seen_missing, &missing).await? {
                    return Ok(None);
                }
            }
            Err(err) => return Err(err.to_string()),
        }
    }
}

async fn read_file_range_cid_with_fetch<S: Store>(
    state: &AppState,
    tree: &HashTree<S>,
    cid: &Cid,
    start: u64,
    end: Option<u64>,
) -> Result<Option<Vec<u8>>, String> {
    let mut seen_missing = HashSet::new();

    loop {
        if !ensure_blob_available(state, &cid.hash).await? {
            return Ok(None);
        }

        match tree.read_file_range_cid(cid, start, end).await {
            Ok(data) => return Ok(data),
            Err(HashTreeError::MissingChunk(missing)) => {
                if !fetch_missing_chunk(state, &mut seen_missing, &missing).await? {
                    return Ok(None);
                }
            }
            Err(err) => return Err(err.to_string()),
        }
    }
}

fn stream_file_range_cid_with_fetch(
    state: AppState,
    cid: Cid,
    start: u64,
    end_inclusive: u64,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> {
    stream::unfold(
        (state, cid, start, end_inclusive, false),
        |(state, cid, offset, end_inclusive, finished)| async move {
            if finished || offset > end_inclusive {
                return None;
            }

            let chunk_end_inclusive = offset
                .saturating_add(CID_RANGE_STREAM_CHUNK_SIZE - 1)
                .min(end_inclusive);
            let chunk_end_exclusive = chunk_end_inclusive.saturating_add(1);
            let tree = HashTree::new(HashTreeConfig::new(state.store.store_arc()).public());

            match read_file_range_cid_with_fetch(
                &state,
                &tree,
                &cid,
                offset,
                Some(chunk_end_exclusive),
            )
            .await
            {
                Ok(Some(data)) if !data.is_empty() => Some((
                    Ok(Bytes::from(data)),
                    (
                        state,
                        cid,
                        chunk_end_inclusive.saturating_add(1),
                        end_inclusive,
                        false,
                    ),
                )),
                Ok(Some(_)) | Ok(None) => Some((
                    Err(std::io::Error::other("CID range returned no data")),
                    (state, cid, end_inclusive, end_inclusive, true),
                )),
                Err(err) => Some((
                    Err(std::io::Error::other(err)),
                    (state, cid, end_inclusive, end_inclusive, true),
                )),
            }
        },
    )
}

async fn get_size_cid_with_fetch<S: Store>(
    state: &AppState,
    tree: &HashTree<S>,
    cid: &Cid,
) -> Result<Option<u64>, String> {
    let cache_key = cid_cache_key(cid);
    if let Some(cached) = get_cached_lookup(&state.cid_size_cache, &cache_key) {
        return Ok(cached);
    }

    let mut seen_missing = HashSet::new();

    loop {
        if !ensure_blob_available(state, &cid.hash).await? {
            put_cached_lookup(&state.cid_size_cache, cache_key.clone(), None);
            return Ok(None);
        }

        match tree.get_size_cid(cid).await {
            Ok(size) => {
                put_cached_lookup(&state.cid_size_cache, cache_key.clone(), Some(size));
                return Ok(Some(size));
            }
            Err(HashTreeError::MissingChunk(missing)) => {
                if !fetch_missing_chunk(state, &mut seen_missing, &missing).await? {
                    put_cached_lookup(&state.cid_size_cache, cache_key.clone(), None);
                    return Ok(None);
                }
            }
            Err(err) => return Err(err.to_string()),
        }
    }
}

async fn root_is_directory_with_fetch<S: Store>(
    state: &AppState,
    tree: &HashTree<S>,
    cid: &Cid,
) -> Result<bool, String> {
    if !ensure_blob_available(state, &cid.hash).await? {
        return Ok(false);
    }

    match tree.get_node(cid).await.map_err(|e| e.to_string())? {
        Some(node) if node.node_type == LinkType::Dir => Ok(true),
        Some(node) if node.node_type == LinkType::File => {
            let mut seen_missing = HashSet::new();
            loop {
                match tree.is_dir(cid).await {
                    Ok(is_dir) => return Ok(is_dir),
                    Err(HashTreeError::MissingChunk(missing)) => {
                        if !fetch_missing_chunk(state, &mut seen_missing, &missing).await? {
                            return Ok(false);
                        }
                    }
                    Err(err) => return Err(err.to_string()),
                }
            }
        }
        Some(_) | None => Ok(false),
    }
}

#[cfg(test)]
async fn await_webrtc_peer_response<F>(
    future: F,
    hash_hex: &str,
    timeout: Duration,
) -> Option<(Vec<u8>, String)>
where
    F: std::future::Future<Output = Option<(Vec<u8>, String)>>,
{
    match tokio::time::timeout(timeout, future).await {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!(
                "[htree-fetch] Mesh peer query timed out for {}",
                &hash_hex[..16.min(hash_hex.len())]
            );
            None
        }
    }
}

async fn htree_nhash_impl(
    State(state): State<AppState>,
    nhash: String,
    path: Option<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: axum::http::HeaderMap,
    connect_info: axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> Response<Body> {
    let is_localhost = connect_info.0.ip().is_loopback();

    let nhash_data = match nhash_decode(&nhash) {
        Ok(data) => data,
        Err(e) => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(format!("Invalid nhash: {}", e)))
                .unwrap();
        }
    };

    let mut cid = Cid {
        hash: nhash_data.hash,
        key: nhash_data.decrypt_key,
    };

    if cid.key.is_none() {
        if let Some(k) = parse_hex_key(params.get("k")) {
            cid.key = Some(k);
        }
    }

    let mut effective_path = path.filter(|p| !p.is_empty());

    let store = state.store.store_arc();
    let tree = HashTree::new(HashTreeConfig::new(store).public());

    if let Some(requested_path) = effective_path.clone() {
        if is_thumbnail_request(&requested_path) {
            match resolve_thumbnail_path(&state, &tree, &cid, &requested_path).await {
                Ok(Some(resolved_path)) => {
                    effective_path = Some(resolved_path);
                }
                Ok(None) => {}
                Err(e) => {
                    return Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                        .body(Body::from(format!("Error: {}", e)))
                        .unwrap();
                }
            }
        }
    }

    let is_dir = match root_is_directory_with_fetch(&state, &tree, &cid).await {
        Ok(value) => value,
        Err(e) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(format!("Error: {}", e)))
                .unwrap();
        }
    };

    if is_dir {
        match resolve_directory_target(&state, &tree, &cid, effective_path.clone()).await {
            Ok(Some(DirectoryTarget::File { cid: entry, path })) => {
                return serve_cid_with_range(
                    &state,
                    &entry,
                    headers,
                    true,
                    is_localhost,
                    Some(&path),
                )
                .await;
            }
            Ok(Some(DirectoryTarget::DirectoryListing { cid: listing_cid })) => {
                return list_directory_json(&state, &listing_cid, true, is_localhost).await;
            }
            Ok(None) => {
                return Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                    .body(Body::from("File not found"))
                    .unwrap();
            }
            Err(e) => {
                return Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                    .body(Body::from(format!("Error: {}", e)))
                    .unwrap();
            }
        }
    }

    if let Some(path) = effective_path.clone() {
        if path.contains('/') {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from("Not found"))
                .unwrap();
        }
    }

    serve_cid_with_range(
        &state,
        &cid,
        headers,
        true,
        is_localhost,
        effective_path.as_deref(),
    )
    .await
}

pub async fn htree_nhash(
    State(state): State<AppState>,
    Path(nhash): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: axum::http::HeaderMap,
    connect_info: axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> impl IntoResponse {
    let full = format!("nhash1{}", nhash);
    htree_nhash_impl(
        State(state),
        full,
        None,
        Query(params),
        headers,
        connect_info,
    )
    .await
}

pub async fn htree_nhash_path(
    State(state): State<AppState>,
    Path((nhash, path)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    headers: axum::http::HeaderMap,
    connect_info: axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> impl IntoResponse {
    let full = format!("nhash1{}", nhash);
    htree_nhash_impl(
        State(state),
        full,
        Some(path),
        Query(params),
        headers,
        connect_info,
    )
    .await
}

const THUMBNAIL_PATTERNS: &[&str] = &[
    "thumbnail.jpg",
    "thumbnail.webp",
    "thumbnail.png",
    "thumbnail.jpeg",
];

const VIDEO_EXTENSIONS: &[&str] = &[".mp4", ".webm", ".mkv", ".mov", ".avi", ".m4v"];

fn is_video_filename(name: &str) -> bool {
    name.starts_with("video.") || VIDEO_EXTENSIONS.iter().any(|ext| name.ends_with(ext))
}

fn is_metadata_filename(name: &str) -> bool {
    name.ends_with(".json") || name.ends_with(".txt")
}

fn is_image_filename(name: &str) -> bool {
    let normalized = name.trim().to_ascii_lowercase();
    normalized.ends_with(".jpg")
        || normalized.ends_with(".jpeg")
        || normalized.ends_with(".png")
        || normalized.ends_with(".webp")
}

fn find_thumbnail_entry_name(entries: &[TreeEntry]) -> Option<String> {
    for pattern in THUMBNAIL_PATTERNS {
        if entries.iter().any(|entry| entry.name == *pattern) {
            return Some((*pattern).to_string());
        }
    }

    entries
        .iter()
        .find(|entry| is_image_filename(&entry.name))
        .map(|entry| entry.name.clone())
}

fn is_thumbnail_request(path: &str) -> bool {
    path == "thumbnail" || path.ends_with("/thumbnail")
}

async fn resolve_thumbnail_path<S: Store>(
    state: &AppState,
    tree: &HashTree<S>,
    root: &Cid,
    path: &str,
) -> Result<Option<String>, String> {
    if !is_thumbnail_request(path) {
        return Ok(None);
    }

    let cache_key = resolved_path_cache_key(root, path);
    if let Some(cached) = get_cached_lookup(&state.thumbnail_path_cache, &cache_key) {
        return Ok(cached);
    }

    let dir_path = if path == "thumbnail" {
        ""
    } else {
        path.strip_suffix("/thumbnail").unwrap_or("")
    };

    let dir_entry = if dir_path.is_empty() {
        Some(root.clone())
    } else {
        resolve_path_with_fetch(state, tree, root, dir_path)
            .await?
            .map(|entry| entry.cid)
    };
    let Some(dir_entry) = dir_entry else {
        put_cached_lookup(&state.thumbnail_path_cache, cache_key, None);
        return Ok(None);
    };

    let Some(entries) = list_directory_with_fetch(state, tree, &dir_entry).await? else {
        put_cached_lookup(&state.thumbnail_path_cache, cache_key, None);
        return Ok(None);
    };

    if let Some(thumbnail_name) = find_thumbnail_entry_name(&entries) {
        let resolved = if dir_path.is_empty() {
            thumbnail_name
        } else {
            format!("{}/{}", dir_path, thumbnail_name)
        };
        put_cached_lookup(
            &state.thumbnail_path_cache,
            cache_key,
            Some(resolved.clone()),
        );
        return Ok(Some(resolved));
    }

    let has_video_file = entries.iter().any(|e| is_video_filename(&e.name));
    if !has_video_file && !entries.is_empty() {
        let mut sorted: Vec<_> = entries.iter().collect();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));

        for entry in sorted.into_iter().take(3) {
            if is_metadata_filename(&entry.name) {
                continue;
            }

            let sub_cid = Cid {
                hash: entry.hash,
                key: entry.key,
            };
            let sub_entries = match list_directory_with_fetch(state, tree, &sub_cid).await? {
                Some(entries) => entries,
                None => continue,
            };

            if let Some(thumbnail_name) = find_thumbnail_entry_name(&sub_entries) {
                let prefix = if dir_path.is_empty() {
                    entry.name.clone()
                } else {
                    format!("{}/{}", dir_path, entry.name)
                };
                let resolved = format!("{}/{}", prefix, thumbnail_name);
                put_cached_lookup(
                    &state.thumbnail_path_cache,
                    cache_key,
                    Some(resolved.clone()),
                );
                return Ok(Some(resolved));
            }
        }
    }

    put_cached_lookup(&state.thumbnail_path_cache, cache_key, None);
    Ok(None)
}

async fn htree_npub_impl(
    State(state): State<AppState>,
    npub: String,
    treename: String,
    path: Option<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: axum::http::HeaderMap,
    connect_info: axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> Response<Body> {
    let is_localhost = connect_info.0.ip().is_loopback();
    let key = format!("{}/{}", npub, treename);
    let link_key = parse_hex_key(params.get("k"));
    let resolved =
        if let Some(resolved) = resolve_root_offline(&state, &npub, &treename, link_key).await {
            resolved.cid
        } else {
            let resolver = match NostrRootResolver::new(resolver_config(&state)).await {
                Ok(r) => r,
                Err(e) => {
                    return Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                        .body(Body::from(format!("Failed to create resolver: {}", e)))
                        .unwrap();
                }
            };

            let cid = match tokio::time::timeout(
                HTTP_RESOLVER_TIMEOUT,
                resolve_npub_root(&key, &resolver, link_key),
            )
            .await
            {
                Ok(Ok(cid)) => cid,
                Ok(Err(e)) => {
                    let _ = resolver.stop().await;
                    return Response::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                        .body(Body::from(format!("Resolution failed: {}", e)))
                        .unwrap();
                }
                Err(_) => {
                    let _ = resolver.stop().await;
                    return Response::builder()
                        .status(StatusCode::GATEWAY_TIMEOUT)
                        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                        .body(Body::from("Resolution timeout"))
                        .unwrap();
                }
            };
            let _ = resolver.stop().await;
            put_cached_tree_root(
                &state,
                tree_root_cache_key(&npub, &treename, link_key),
                cid.clone(),
                "nostr",
                None,
            );
            cid
        };

    let mut cid = resolved;
    if cid.key.is_none() {
        if let Some(k) = link_key {
            cid.key = Some(k);
        }
    }

    let store = state.store.store_arc();
    let tree = HashTree::new(HashTreeConfig::new(store).public());

    let mut effective_path = path.filter(|p| !p.is_empty());
    if let Some(path) = effective_path.clone() {
        if path == "thumbnail" || path.ends_with("/thumbnail") {
            match resolve_thumbnail_path(&state, &tree, &cid, &path).await {
                Ok(Some(resolved_path)) => {
                    effective_path = Some(resolved_path);
                }
                Ok(None) => {}
                Err(e) => {
                    return Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                        .body(Body::from(format!("Error: {}", e)))
                        .unwrap();
                }
            }
        }
    }

    let is_dir = match root_is_directory_with_fetch(&state, &tree, &cid).await {
        Ok(value) => value,
        Err(e) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(format!("Error: {}", e)))
                .unwrap();
        }
    };
    if is_dir {
        match resolve_directory_target(&state, &tree, &cid, effective_path.clone()).await {
            Ok(Some(DirectoryTarget::File { cid: entry, path })) => {
                return serve_cid_with_range(
                    &state,
                    &entry,
                    headers,
                    false,
                    is_localhost,
                    Some(&path),
                )
                .await;
            }
            Ok(Some(DirectoryTarget::DirectoryListing { cid: listing_cid })) => {
                return list_directory_json(&state, &listing_cid, false, is_localhost).await;
            }
            Ok(None) => {
                return Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                    .body(Body::from("File not found"))
                    .unwrap();
            }
            Err(e) => {
                return Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                    .body(Body::from(format!("Error: {}", e)))
                    .unwrap();
            }
        }
    }

    serve_cid_with_range(
        &state,
        &cid,
        headers,
        false,
        is_localhost,
        effective_path.as_deref(),
    )
    .await
}

pub async fn htree_npub(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Path((npub, treename)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    headers: axum::http::HeaderMap,
    connect_info: axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> impl IntoResponse {
    let parsed = parse_mutable_htree_request_path(uri.path());
    let full = parsed
        .as_ref()
        .map(|entry| entry.npub.clone())
        .unwrap_or_else(|| format!("npub1{}", npub));
    let resolved_treename = parsed
        .as_ref()
        .map(|entry| entry.treename.clone())
        .unwrap_or(treename);
    htree_npub_impl(
        State(state),
        full,
        resolved_treename,
        None,
        Query(params),
        headers,
        connect_info,
    )
    .await
}

pub async fn htree_npub_path(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Path((npub, treename, path)): Path<(String, String, String)>,
    Query(params): Query<HashMap<String, String>>,
    headers: axum::http::HeaderMap,
    connect_info: axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> impl IntoResponse {
    let parsed = parse_mutable_htree_request_path(uri.path());
    let full = parsed
        .as_ref()
        .map(|entry| entry.npub.clone())
        .unwrap_or_else(|| format!("npub1{}", npub));
    let resolved_treename = parsed
        .as_ref()
        .map(|entry| entry.treename.clone())
        .unwrap_or(treename);
    let resolved_path = parsed.and_then(|entry| entry.path).or(Some(path));
    htree_npub_impl(
        State(state),
        full,
        resolved_treename,
        resolved_path,
        Query(params),
        headers,
        connect_info,
    )
    .await
}

/// Cache-Control header for immutable content-addressed data (1 year)
const IMMUTABLE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
const CORP_CROSS_ORIGIN: &str = "cross-origin";
const CROSS_ORIGIN_RESOURCE_POLICY_HEADER: &str = "cross-origin-resource-policy";

/// Source of blob data for X-Source header
#[derive(Debug, Clone)]
enum BlobSource {
    Local,
    WebRtcPeer { peer_id: String },
    Upstream { server: String },
}

impl BlobSource {
    fn to_header_value(&self) -> String {
        match self {
            BlobSource::Local => "local".to_string(),
            BlobSource::WebRtcPeer { peer_id } => format!("webrtc:{}", peer_id),
            BlobSource::Upstream { server } => format!("upstream:{}", server),
        }
    }
}

/// Build a blob response with optional X-Source header (only for localhost)
fn build_blob_response(data: Vec<u8>, source: BlobSource, is_localhost: bool) -> Response<Body> {
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, data.len())
        .header(header::CACHE_CONTROL, IMMUTABLE_CACHE_CONTROL)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(CROSS_ORIGIN_RESOURCE_POLICY_HEADER, CORP_CROSS_ORIGIN);

    if is_localhost {
        builder = builder.header("X-Source", source.to_header_value());
    }

    builder.body(Body::from(data)).unwrap()
}

fn parse_hex_key(value: Option<&String>) -> Option<[u8; 32]> {
    let hex = value?;
    if hex.len() != 64 {
        return None;
    }
    from_hex(hex).ok()
}

fn content_type_for_path(path: Option<&str>) -> &'static str {
    let filename = path.and_then(|p| p.rsplit('/').next()).unwrap_or("");
    if filename.is_empty() {
        return "application/octet-stream";
    }
    get_mime_type(filename)
}

fn build_json_response(
    payload: serde_json::Value,
    is_immutable: bool,
    is_localhost: bool,
) -> Response<Body> {
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(CROSS_ORIGIN_RESOURCE_POLICY_HEADER, CORP_CROSS_ORIGIN);
    if is_immutable {
        builder = builder.header(header::CACHE_CONTROL, IMMUTABLE_CACHE_CONTROL);
    }
    if is_localhost {
        builder = builder.header("X-Source", "local");
    }
    builder.body(Body::from(payload.to_string())).unwrap()
}

enum ParsedByteRange {
    Satisfiable { start: u64, end_inclusive: u64 },
    Unsatisfiable,
}

fn parse_byte_range(range_header: &str, total_size: u64) -> Option<ParsedByteRange> {
    let bytes_range = range_header.strip_prefix("bytes=")?;
    if bytes_range.contains(',') {
        return None;
    }

    let (start_part, end_part) = bytes_range.split_once('-')?;
    if total_size == 0 {
        return Some(ParsedByteRange::Unsatisfiable);
    }

    if start_part.is_empty() {
        let suffix_len = end_part.parse::<u64>().ok()?;
        if suffix_len == 0 {
            return Some(ParsedByteRange::Unsatisfiable);
        }
        let clamped_suffix_len = suffix_len.min(total_size);
        return Some(ParsedByteRange::Satisfiable {
            start: total_size - clamped_suffix_len,
            end_inclusive: total_size - 1,
        });
    }

    let start = start_part.parse::<u64>().ok()?;
    if start >= total_size {
        return Some(ParsedByteRange::Unsatisfiable);
    }

    let end_inclusive = if end_part.is_empty() {
        total_size - 1
    } else {
        let parsed_end = end_part.parse::<u64>().ok()?;
        if parsed_end < start {
            return Some(ParsedByteRange::Unsatisfiable);
        }
        parsed_end.min(total_size - 1)
    };

    Some(ParsedByteRange::Satisfiable {
        start,
        end_inclusive,
    })
}

async fn serve_cid_with_range(
    state: &AppState,
    cid: &Cid,
    headers: axum::http::HeaderMap,
    is_immutable: bool,
    is_localhost: bool,
    filename_hint: Option<&str>,
) -> Response<Body> {
    let store = state.store.store_arc();
    let tree = HashTree::new(HashTreeConfig::new(store).public());
    let content_type = content_type_for_path(filename_hint);

    let range_header = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    if let Some(range_str) = range_header {
        let total_size = match get_size_cid_with_fetch(state, &tree, cid).await {
            Ok(Some(size)) => size,
            Ok(None) => {
                return Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                    .body(Body::from("Not found"))
                    .unwrap();
            }
            Err(e) => {
                return Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                    .body(Body::from(format!("Error: {}", e)))
                    .unwrap();
            }
        };

        if let Some(parsed_range) = parse_byte_range(range_str, total_size) {
            let (start, end_inclusive) = match parsed_range {
                ParsedByteRange::Satisfiable {
                    start,
                    end_inclusive,
                } => (start, end_inclusive),
                ParsedByteRange::Unsatisfiable => {
                    return Response::builder()
                        .status(StatusCode::RANGE_NOT_SATISFIABLE)
                        .header(header::CONTENT_TYPE, "text/plain")
                        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                        .body(Body::from("Range not satisfiable"))
                        .unwrap();
                }
            };

            let end_exclusive = end_inclusive.saturating_add(1);
            let content_length = end_exclusive.saturating_sub(start) as usize;
            let content_range = format!("bytes {}-{}/{}", start, end_inclusive, total_size);
            let body = Body::from_stream(stream_file_range_cid_with_fetch(
                state.clone(),
                cid.clone(),
                start,
                end_inclusive,
            ));

            let mut builder = Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_TYPE, content_type)
                .header(header::CONTENT_LENGTH, content_length)
                .header(header::CONTENT_RANGE, content_range)
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header(CROSS_ORIGIN_RESOURCE_POLICY_HEADER, CORP_CROSS_ORIGIN);
            if is_immutable {
                builder = builder.header(header::CACHE_CONTROL, IMMUTABLE_CACHE_CONTROL);
            }
            if is_localhost {
                builder = builder.header("X-Source", "local");
            }
            return builder.body(body).unwrap();
        }
    }

    let data = match get_cid_with_fetch(state, &tree, cid).await {
        Ok(Some(data)) => data,
        Ok(None) => {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from("Not found"))
                .unwrap();
        }
        Err(e) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(format!("Error: {}", e)))
                .unwrap();
        }
    };

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, data.len())
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(CROSS_ORIGIN_RESOURCE_POLICY_HEADER, CORP_CROSS_ORIGIN);
    if is_immutable {
        builder = builder.header(header::CACHE_CONTROL, IMMUTABLE_CACHE_CONTROL);
    }
    if is_localhost {
        builder = builder.header("X-Source", "local");
    }

    builder.body(Body::from(data)).unwrap()
}

/// Internal content serving (shared by CID and blossom routes)
///
/// `is_immutable`: if true, adds Cache-Control: immutable header.
/// Use true for content-addressed routes (hash, nhash, blossom SHA256).
/// Use false for mutable routes (npub/ref_name) where the reference can change.
/// `is_localhost`: if true, adds X-Source header for debugging.
async fn serve_content_internal(
    state: &AppState,
    hash: &[u8; 32],
    headers: axum::http::HeaderMap,
    is_immutable: bool,
    is_localhost: bool,
) -> Response<Body> {
    let store = &state.store;

    // Always return raw bytes - no conversion to JSON/HTML
    // This is required for Blossom protocol compatibility

    // Try as file
    // Check for Range header
    let range_header = headers.get(header::RANGE).and_then(|v| v.to_str().ok());

    if let Some(range_str) = range_header {
        // Content type - hashtree doesn't store filenames, so default to octet-stream
        let content_type = "application/octet-stream";

        match store.get_file_chunk_metadata(hash) {
            Ok(Some(metadata)) => {
                let total_size = metadata.total_size;
                if let Some(parsed_range) = parse_byte_range(range_str, total_size) {
                    let (start, end_actual) = match parsed_range {
                        ParsedByteRange::Satisfiable {
                            start,
                            end_inclusive,
                        } => (start, end_inclusive),
                        ParsedByteRange::Unsatisfiable => {
                            return Response::builder()
                                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                                .header(header::CONTENT_TYPE, "text/plain")
                                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                                .body(Body::from("Range not satisfiable"))
                                .unwrap()
                                .into_response();
                        }
                    };

                    let content_length = end_actual - start + 1;
                    let content_range = format!("bytes {}-{}/{}", start, end_actual, total_size);

                    if metadata.is_chunked {
                        match state
                            .store
                            .clone()
                            .stream_file_range_chunks_owned(hash, start, end_actual)
                        {
                            Ok(Some(chunks_iter)) => {
                                let stream =
                                    stream::iter(chunks_iter).map(|result| result.map(Bytes::from));

                                let mut builder = Response::builder()
                                    .status(StatusCode::PARTIAL_CONTENT)
                                    .header(header::CONTENT_TYPE, content_type)
                                    .header(header::CONTENT_LENGTH, content_length)
                                    .header(header::CONTENT_RANGE, content_range)
                                    .header(header::ACCEPT_RANGES, "bytes")
                                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*");
                                if is_immutable {
                                    builder = builder
                                        .header(header::CACHE_CONTROL, IMMUTABLE_CACHE_CONTROL);
                                }
                                if is_localhost {
                                    builder = builder.header("X-Source", "local");
                                }
                                return builder
                                    .body(Body::from_stream(stream))
                                    .unwrap()
                                    .into_response();
                            }
                            Ok(None) => {
                                return Response::builder()
                                    .status(StatusCode::NOT_FOUND)
                                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                                    .body(Body::from("File not found"))
                                    .unwrap()
                                    .into_response();
                            }
                            Err(e) => {
                                return Response::builder()
                                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                                    .body(Body::from(format!("Error: {}", e)))
                                    .unwrap()
                                    .into_response();
                            }
                        }
                    } else {
                        match store.get_file_range(hash, start, Some(end_actual)) {
                            Ok(Some((range_content, _))) => {
                                let mut builder = Response::builder()
                                    .status(StatusCode::PARTIAL_CONTENT)
                                    .header(header::CONTENT_TYPE, content_type)
                                    .header(header::CONTENT_LENGTH, range_content.len())
                                    .header(header::CONTENT_RANGE, content_range)
                                    .header(header::ACCEPT_RANGES, "bytes")
                                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*");
                                if is_immutable {
                                    builder = builder
                                        .header(header::CACHE_CONTROL, IMMUTABLE_CACHE_CONTROL);
                                }
                                if is_localhost {
                                    builder = builder.header("X-Source", "local");
                                }
                                return builder
                                    .body(Body::from(range_content))
                                    .unwrap()
                                    .into_response();
                            }
                            Ok(None) => {
                                return Response::builder()
                                    .status(StatusCode::NOT_FOUND)
                                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                                    .body(Body::from("File not found"))
                                    .unwrap()
                                    .into_response();
                            }
                            Err(e) => {
                                return Response::builder()
                                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                                    .body(Body::from(format!("Error: {}", e)))
                                    .unwrap()
                                    .into_response();
                            }
                        }
                    }
                }
            }
            Ok(None) => {
                return Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                    .body(Body::from("File not found"))
                    .unwrap()
                    .into_response();
            }
            Err(e) => {
                return Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                    .body(Body::from(format!("Error: {}", e)))
                    .unwrap()
                    .into_response();
            }
        }
    }

    // Fall back to full file
    match store.get_file(hash) {
        Ok(Some(content)) => {
            // Content type - hashtree doesn't store filenames, so default to octet-stream
            let content_type = "application/octet-stream";

            let mut builder = Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, content_type)
                .header(header::CONTENT_LENGTH, content.len())
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*");
            if is_immutable {
                builder = builder.header(header::CACHE_CONTROL, IMMUTABLE_CACHE_CONTROL);
            }
            if is_localhost {
                builder = builder.header("X-Source", "local");
            }
            builder.body(Body::from(content)).unwrap().into_response()
        }
        Ok(None) => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from("Not found"))
            .unwrap()
            .into_response(),
        Err(e) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from(format!("Error: {}", e)))
            .unwrap()
            .into_response(),
    }
}

/// Serve content by CID or blossom SHA256 hash
/// Tries CID first, then falls back to blossom lookup if input looks like SHA256
/// If not found locally, queries connected WebSocket/WebRTC peers
pub async fn serve_content_or_blob(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: axum::http::HeaderMap,
    connect_info: axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> impl IntoResponse {
    if let Some(virtual_root) = request_virtual_tree_root(&headers) {
        return serve_virtual_tree_host_request(
            &state,
            &virtual_root,
            Some(id.clone()),
            params,
            headers,
            connect_info,
        )
        .await
        .into_response();
    }

    let is_localhost = connect_info.0.ip().is_loopback();
    let _client_ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| connect_info.0.ip().to_string());
    // Parse potential extension for blossom
    let (hash_part, _ext) = if let Some(dot_pos) = id.rfind('.') {
        (&id[..dot_pos], Some(&id[dot_pos..]))
    } else {
        (id.as_str(), None)
    };

    // Check if it looks like a SHA256 hash (64 hex chars)
    let is_sha256 = hash_part.len() == 64 && hash_part.chars().all(|c| c.is_ascii_hexdigit());

    // Try raw blob lookup first (for hashtree chunks / git objects)
    // This takes priority over file tree serving to avoid returning reassembled
    // file content when the caller expects raw chunk data
    if is_sha256 {
        let hash_hex = hash_part.to_lowercase();
        if let Ok(hash_bytes) = from_hex(&hash_hex) {
            if let Ok(Some(data)) = state.store.get_blob(&hash_bytes) {
                return build_blob_response(data, BlobSource::Local, is_localhost).into_response();
            }
        }
    }

    // Try file tree lookup (serves reassembled file content)
    // (hashtree hashes are 64 hex chars, same as blossom SHA256)
    if let Ok(hash) = from_hex(&id) {
        if state
            .store
            .get_file_chunk_metadata(&hash)
            .ok()
            .flatten()
            .is_some()
        {
            return serve_content_internal(&state, &hash, headers, true, is_localhost).await;
        }
    }

    // Not found locally - try querying connected WebRTC peers
    if is_sha256 {
        let hash_hex = hash_part.to_lowercase();

        // Try WebRTC peers first
        if let Some(ref webrtc_state) = state.webrtc_peers {
            tracing::info!(
                "Hash {} not found locally, querying WebRTC peers",
                &hash_hex[..16.min(hash_hex.len())]
            );

            // Query connected mesh peers
            if let Some((data, peer_id)) = query_webrtc_peers(webrtc_state, &hash_hex).await {
                // Cache locally for future requests
                if let Err(e) = state.store.put_blob(&data) {
                    tracing::warn!("Failed to cache peer data: {}", e);
                }

                return build_blob_response(data, BlobSource::WebRtcPeer { peer_id }, is_localhost)
                    .into_response();
            }
        }

        // Try upstream Blossom servers
        if !state.upstream_blossom.is_empty() {
            tracing::info!(
                "Hash {} not found via WebRTC, trying upstream Blossom",
                &hash_hex[..16.min(hash_hex.len())]
            );

            if let Some((data, server)) =
                query_upstream_blossom(&state.upstream_blossom, &hash_hex).await
            {
                // Cache locally for future requests
                if let Err(e) = state.store.put_blob(&data) {
                    tracing::warn!("Failed to cache upstream data: {}", e);
                }

                return build_blob_response(data, BlobSource::Upstream { server }, is_localhost)
                    .into_response();
            }
        }
    }

    // Not found anywhere
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(Body::from("Not found"))
        .unwrap()
        .into_response()
}

/// Serve content by npub/ref_name (Nostr resolver)
/// Route: /npub1... (the "npub1" prefix is matched by the route, :rest captures pubkey remainder + /ref)
pub async fn serve_npub(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Path(rest): Path<String>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    // Reconstruct full key: "npub1" + rest (e.g., "abc.../mydata")
    let parsed = parse_bare_npub_request_path(uri.path());
    let key = parsed
        .as_ref()
        .map(|entry| format!("{}/{}", entry.npub, entry.treename))
        .unwrap_or_else(|| format!("npub1{}", rest));

    // Validate format: must have a / for ref name
    if !key.contains('/') {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from("Missing ref name: use /npub1.../ref_name"))
            .unwrap()
            .into_response();
    }

    let resolver = match NostrRootResolver::new(resolver_config(&state)).await {
        Ok(r) => r,
        Err(e) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(format!("Failed to create resolver: {}", e)))
                .unwrap()
                .into_response();
        }
    };

    // npub routes are mutable - the reference can change over time
    match tokio::time::timeout(HTTP_RESOLVER_TIMEOUT, resolver.resolve_wait(&key)).await {
        Ok(Ok(cid)) => {
            let cache_entry = parsed
                .as_ref()
                .map(|entry| (entry.npub.as_str(), entry.treename.as_str()))
                .or_else(|| key.split_once('/'));
            if let Some((pubkey, treename)) = cache_entry {
                cache_public_tree_root(&state, pubkey, treename, &cid);
            }
            let _ = resolver.stop().await;
            serve_content_internal(&state, &cid.hash, headers, false, false).await
        }
        Ok(Err(e)) => {
            let _ = resolver.stop().await;
            Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(format!("Resolution failed: {}", e)))
                .unwrap()
                .into_response()
        }
        Err(_) => {
            let _ = resolver.stop().await;
            Response::builder()
                .status(StatusCode::GATEWAY_TIMEOUT)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from("Resolution timeout"))
                .unwrap()
                .into_response()
        }
    }
}

pub async fn upload_file(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let store = &state.store;
    let mut temp_file_path: Option<std::path::PathBuf> = None;
    let mut file_name_final: Option<String> = None;
    let temp_dir = tempfile::tempdir().unwrap();

    while let Some(mut field) = multipart.next_field().await.unwrap_or(None) {
        let name = field.name().unwrap_or("").to_string();

        if name == "file" {
            let file_name = field.file_name().unwrap_or("upload").to_string();
            let temp_file = temp_dir.path().join(&file_name);

            // Stream directly to disk instead of loading into memory
            let mut file = tokio::fs::File::create(&temp_file).await.unwrap();

            while let Some(chunk) = field.next().await {
                if let Ok(data) = chunk {
                    file.write_all(&data).await.unwrap();
                }
            }

            file.flush().await.unwrap();
            temp_file_path = Some(temp_file);
            file_name_final = Some(file_name);
            break;
        }
    }

    let (temp_file, file_name) = match (temp_file_path, file_name_final) {
        (Some(path), Some(name)) => (path, name),
        _ => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("No file provided"))
                .unwrap();
        }
    };

    // Use streaming upload for files > 10MB
    let file_size = std::fs::metadata(&temp_file)
        .ok()
        .map(|m| m.len())
        .unwrap_or(0);
    let use_streaming = file_size > 10 * 1024 * 1024;

    let cid_result = if use_streaming {
        // Streaming upload with progress callbacks
        let file = std::fs::File::open(&temp_file).unwrap();
        store.upload_file_stream(file, file_name, |_intermediate_cid| {
            // Could log progress here or publish to websocket
        })
    } else {
        // Regular upload for small files
        store.upload_file(&temp_file)
    };

    // Upload and get CID
    match cid_result {
        Ok(cid) => {
            let json = json!({
                "success": true,
                "cid": cid
            });
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json.to_string()))
                .unwrap()
        }
        Err(e) => {
            let json = json!({
                "success": false,
                "error": e.to_string()
            });
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json.to_string()))
                .unwrap()
        }
    }
}

pub async fn list_pins(State(state): State<AppState>) -> impl IntoResponse {
    let store = &state.store;
    match store.list_pins_with_names() {
        Ok(pins) => Json(json!({
            "pins": pins.iter().map(|p| json!({
                "cid": p.cid,
                "name": p.name,
                "is_directory": p.is_directory
            })).collect::<Vec<_>>()
        })),
        Err(e) => Json(json!({
            "error": e.to_string()
        })),
    }
}

pub async fn pin_cid(State(state): State<AppState>, Path(cid): Path<String>) -> impl IntoResponse {
    let hash = match from_hex(&cid) {
        Ok(h) => h,
        Err(e) => {
            return Json(json!({
                "success": false,
                "error": format!("Invalid CID format: {}", e)
            }))
        }
    };
    let store = &state.store;
    match store.pin(&hash) {
        Ok(_) => Json(json!({
            "success": true,
            "cid": cid
        })),
        Err(e) => Json(json!({
            "success": false,
            "error": e.to_string()
        })),
    }
}

pub async fn unpin_cid(
    State(state): State<AppState>,
    Path(cid): Path<String>,
) -> impl IntoResponse {
    let hash = match from_hex(&cid) {
        Ok(h) => h,
        Err(e) => {
            return Json(json!({
                "success": false,
                "error": format!("Invalid CID format: {}", e)
            }))
        }
    };
    let store = &state.store;
    match store.unpin(&hash) {
        Ok(_) => Json(json!({
            "success": true,
            "cid": cid
        })),
        Err(e) => Json(json!({
            "success": false,
            "error": e.to_string()
        })),
    }
}

pub async fn storage_stats(State(state): State<AppState>) -> impl IntoResponse {
    let store = &state.store;
    match store.get_storage_stats() {
        Ok(stats) => Json(json!({
            "total_dags": stats.total_dags,
            "pinned_dags": stats.pinned_dags,
            "total_bytes": stats.total_bytes,
        })),
        Err(e) => Json(json!({
            "error": e.to_string()
        })),
    }
}

/// Health check endpoint - minimal overhead, just returns ok
pub async fn health_check() -> impl IntoResponse {
    // Minimal health check - if we can respond, we're alive
    // Storage checks would be heavier and DDoS-able
    Response::builder()
        .status(StatusCode::OK)
        .body(Body::from("ok"))
        .unwrap()
}

fn bluetooth_transport_enabled() -> bool {
    crate::config::Config::load()
        .map(|config| config.server.enable_bluetooth && config.server.max_bluetooth_peers > 0)
        .unwrap_or(true)
}

fn peer_transport_visible(entry: &crate::webrtc::PeerEntry, bluetooth_enabled: bool) -> bool {
    bluetooth_enabled || entry.transport != crate::webrtc::PeerTransport::Bluetooth
}

fn peer_transport_counts(
    peers: &std::collections::HashMap<String, crate::webrtc::PeerEntry>,
    bluetooth_enabled: bool,
) -> serde_json::Value {
    use crate::webrtc::PeerTransport;

    let webrtc = peers
        .values()
        .filter(|entry| peer_transport_visible(entry, bluetooth_enabled))
        .filter(|entry| entry.transport == PeerTransport::WebRtc)
        .count();
    let bluetooth = peers
        .values()
        .filter(|entry| peer_transport_visible(entry, bluetooth_enabled))
        .filter(|entry| entry.transport == PeerTransport::Bluetooth)
        .count();
    json!({
        "webrtc": webrtc,
        "bluetooth": bluetooth,
    })
}

fn peer_entry_json(id: &str, entry: &crate::webrtc::PeerEntry) -> serde_json::Value {
    let rtc_state = entry
        .peer
        .as_ref()
        .and_then(|p| p.as_webrtc().map(|peer| format!("{:?}", peer.state())));
    let signal_paths: Vec<_> = entry
        .signal_paths
        .iter()
        .map(|path| path.to_string())
        .collect();

    json!({
        "id": id,
        "peer_id": entry.peer_id.to_string(),
        "pubkey": entry.peer_id.pubkey.clone(),
        "state": format!("{:?}", entry.state),
        "rtc_state": rtc_state,
        "pool": format!("{:?}", entry.pool),
        "transport": entry.transport.to_string(),
        "signal_paths": signal_paths,
        "connected": entry.state == crate::webrtc::ConnectionState::Connected,
        "has_data_channel": entry.peer.as_ref().map(|p| p.is_ready()).unwrap_or(false),
        "bytes_sent": entry.bytes_sent,
        "bytes_received": entry.bytes_received,
    })
}

/// Get connected mesh peers
pub async fn webrtc_peers(State(state): State<AppState>) -> impl IntoResponse {
    let Some(ref webrtc_state) = state.webrtc_peers else {
        return Json(json!({
            "enabled": false,
            "transport_counts": {
                "webrtc": 0,
                "bluetooth": 0
            },
            "peers": []
        }));
    };

    let peers = webrtc_state.peers.read().await;
    let bluetooth_enabled = bluetooth_transport_enabled();
    let (mesh_received, mesh_forwarded, mesh_dropped_duplicate) = webrtc_state.get_mesh_stats();
    let peer_list: Vec<_> = peers
        .iter()
        .filter(|(_, entry)| peer_transport_visible(entry, bluetooth_enabled))
        .map(|(id, entry)| peer_entry_json(id, entry))
        .collect();

    Json(json!({
        "enabled": true,
        "total": peer_list.len(),
        "connected": peer_list.iter().filter(|p| p["connected"].as_bool().unwrap_or(false)).count(),
        "with_data_channel": peer_list.iter().filter(|p| p["has_data_channel"].as_bool().unwrap_or(false)).count(),
        "transport_counts": peer_transport_counts(&peers, bluetooth_enabled),
        "mesh_received": mesh_received,
        "mesh_forwarded": mesh_forwarded,
        "mesh_dropped_duplicate": mesh_dropped_duplicate,
        "peers": peer_list
    }))
}

/// Daemon status endpoint - localhost only
pub async fn daemon_status(
    State(state): State<AppState>,
    connect_info: axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> impl IntoResponse {
    // Only allow localhost
    let ip = connect_info.0.ip();
    if !ip.is_loopback() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "localhost only"})),
        )
            .into_response();
    }

    let bluetooth_received_events = match state.nostr_relay.as_ref() {
        Some(relay) => relay.bluetooth_received_events(100).await,
        None => Vec::new(),
    };

    // Mesh peers
    let mesh = if let Some(ref webrtc_state) = state.webrtc_peers {
        let peers = webrtc_state.peers.read().await;
        let bluetooth_enabled = bluetooth_transport_enabled();
        let connected = peers
            .values()
            .filter(|entry| peer_transport_visible(entry, bluetooth_enabled))
            .filter(|e| e.state == ConnectionState::Connected)
            .count();
        let with_data_channel = peers
            .values()
            .filter(|entry| peer_transport_visible(entry, bluetooth_enabled))
            .filter(|e| {
                e.state == ConnectionState::Connected
                    && e.peer.as_ref().map(|p| p.is_ready()).unwrap_or(false)
            })
            .count();
        let (bytes_sent, bytes_received) = webrtc_state.get_bandwidth();
        let (mesh_received, mesh_forwarded, mesh_dropped_duplicate) = webrtc_state.get_mesh_stats();
        // Per-peer stats
        let peer_stats: Vec<_> = peers
            .iter()
            .filter(|(_, entry)| peer_transport_visible(entry, bluetooth_enabled))
            .map(|(id, entry)| peer_entry_json(id, entry))
            .collect();
        json!({
            "enabled": true,
            "total_peers": peer_stats.len(),
            "connected": connected,
            "with_data_channel": with_data_channel,
            "transport_counts": peer_transport_counts(&peers, bluetooth_enabled),
            "bytes_sent": bytes_sent,
            "bytes_received": bytes_received,
            "mesh_received": mesh_received,
            "mesh_forwarded": mesh_forwarded,
            "mesh_dropped_duplicate": mesh_dropped_duplicate,
            "bluetooth_received_events": bluetooth_received_events,
            "peers": peer_stats,
        })
    } else {
        json!({
            "enabled": false,
            "bluetooth_received_events": bluetooth_received_events,
        })
    };

    // Upstream servers
    let upstream = json!({
        "blossom_servers": state.upstream_blossom.len(),
        "nostr_relays": state.nostr_relay_urls.len(),
    });
    let (relay_bytes_sent, relay_bytes_received) = state.ws_relay.upstream_relay_bandwidth();
    let relay = json!({
        "enabled": !state.nostr_relay_urls.is_empty(),
        "bytes_sent": relay_bytes_sent,
        "bytes_received": relay_bytes_received,
    });

    Json(json!({
        "status": "running",
        "mesh": mesh.clone(),
        "webrtc": mesh,
        "relay": relay,
        "upstream": upstream,
    }))
    .into_response()
}

pub async fn garbage_collect(State(state): State<AppState>) -> impl IntoResponse {
    let store = &state.store;
    match store.gc() {
        Ok(gc_stats) => Json(json!({
            "deleted_dags": gc_stats.deleted_dags,
            "freed_bytes": gc_stats.freed_bytes
        })),
        Err(e) => Json(json!({
            "error": e.to_string()
        })),
    }
}

pub async fn socialgraph_stats(State(state): State<AppState>) -> impl IntoResponse {
    match &state.social_graph {
        Some(sg) => {
            let stats = sg.stats();
            Json(json!(stats))
        }
        None => Json(json!({
            "enabled": false,
            "message": "Social graph not active"
        })),
    }
}

#[derive(Debug, Deserialize)]
pub struct SocialGraphSnapshotQuery {
    #[serde(rename = "maxNodes")]
    pub max_nodes: Option<usize>,
    #[serde(rename = "maxEdges")]
    pub max_edges: Option<usize>,
    #[serde(rename = "maxDistance")]
    pub max_distance: Option<u32>,
    #[serde(rename = "maxEdgesPerNode")]
    pub max_edges_per_node: Option<usize>,
}

pub async fn socialgraph_snapshot(
    State(state): State<AppState>,
    Query(params): Query<SocialGraphSnapshotQuery>,
    connect_info: axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> impl IntoResponse {
    let ip = connect_info.0.ip();
    if !state.socialgraph_snapshot_public && !ip.is_loopback() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "localhost only"})),
        )
            .into_response();
    }

    let social_graph_store = match &state.social_graph_store {
        Some(store) => Arc::clone(store),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "social graph not initialized"})),
            )
                .into_response();
        }
    };
    let root = match state.social_graph_root {
        Some(root) => root,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "social graph root missing"})),
            )
                .into_response();
        }
    };

    let options = socialgraph::snapshot::SnapshotOptions {
        max_nodes: params.max_nodes,
        max_edges: params.max_edges,
        max_distance: params.max_distance,
        max_edges_per_node: params.max_edges_per_node,
    };

    let chunks = match tokio::task::spawn_blocking(move || {
        socialgraph::snapshot::build_snapshot_chunks(social_graph_store.as_ref(), &root, &options)
    })
    .await
    {
        Ok(Ok(chunks)) => chunks,
        Ok(Err(err)) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(format!("Error generating snapshot: {err}")))
                .unwrap();
        }
        Err(err) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(format!("Error generating snapshot: {err}")))
                .unwrap();
        }
    };

    let stream = stream::iter(chunks.into_iter().map(Ok::<Bytes, std::io::Error>));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"social-graph.bin\"",
        )
        .header(
            header::CACHE_CONTROL,
            "public, max-age=60, stale-while-revalidate=60",
        )
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(Body::from_stream(stream))
        .unwrap()
}

pub async fn follow_distance(
    State(state): State<AppState>,
    Path(pubkey): Path<String>,
) -> impl IntoResponse {
    if pubkey.len() != 64 || !pubkey.chars().all(|c| c.is_ascii_hexdigit()) {
        return Json(json!({
            "error": "Invalid pubkey format (expected 64 hex chars)"
        }));
    }

    match &state.social_graph {
        Some(sg) => {
            let allowed = sg.check_write_access(&pubkey);
            Json(json!({
                "pubkey": pubkey,
                "write_access": allowed,
            }))
        }
        None => Json(json!({
            "pubkey": pubkey,
            "error": "Social graph not active",
        })),
    }
}

/// Timeout for HTTP resolver requests
const HTTP_RESOLVER_TIMEOUT: Duration = Duration::from_secs(10);

/// Create resolver config with HTTP timeout
fn resolver_config(state: &AppState) -> NostrResolverConfig {
    let mut config = NostrResolverConfig {
        resolve_timeout: HTTP_RESOLVER_TIMEOUT,
        ..Default::default()
    };
    if !state.nostr_relay_urls.is_empty() {
        config.relays = state.nostr_relay_urls.clone();
    }
    config
}

/// Resolve npub/treename to hash and serve content
/// Route: /n/:pubkey/:treename or /n/:pubkey/:treename/*path
pub async fn resolve_and_serve(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Path(params): Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let (fallback_pubkey, fallback_treename) = params;
    let parsed = parse_resolve_request_path(uri.path());
    let pubkey = parsed
        .as_ref()
        .map(|entry| entry.pubkey.clone())
        .unwrap_or(fallback_pubkey);
    let treename = parsed
        .as_ref()
        .map(|entry| entry.treename.clone())
        .unwrap_or(fallback_treename);
    let key = format!("{}/{}", pubkey, treename);

    if let Some(resolved) = resolve_root_offline(&state, &pubkey, &treename, None).await {
        return serve_content_internal(&state, &resolved.cid.hash, headers, false, false).await;
    }

    let resolver = match NostrRootResolver::new(resolver_config(&state)).await {
        Ok(r) => r,
        Err(e) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(
                    json!({
                        "error": format!("Failed to create resolver: {}", e),
                        "key": key
                    })
                    .to_string(),
                ))
                .unwrap()
                .into_response();
        }
    };

    // Use resolve_wait with timeout - waits for key to appear
    // This is a mutable route (npub/treename can change over time)
    match tokio::time::timeout(HTTP_RESOLVER_TIMEOUT, resolver.resolve_wait(&key)).await {
        Ok(Ok(cid)) => {
            cache_public_tree_root(&state, &pubkey, &treename, &cid);
            let _ = resolver.stop().await;
            serve_content_internal(&state, &cid.hash, headers, false, false).await
        }
        Ok(Err(e)) => {
            let _ = resolver.stop().await;
            Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(
                    json!({
                        "error": e.to_string(),
                        "key": key
                    })
                    .to_string(),
                ))
                .unwrap()
                .into_response()
        }
        Err(_) => {
            let _ = resolver.stop().await;
            Response::builder()
                .status(StatusCode::GATEWAY_TIMEOUT)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(
                    json!({
                        "error": "Resolution timeout",
                        "key": key
                    })
                    .to_string(),
                ))
                .unwrap()
                .into_response()
        }
    }
}

/// API endpoint to resolve npub/treename to hash (returns JSON)
/// Tries relays first, then WebRTC peers if available.
pub async fn resolve_to_hash(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Path(params): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let (fallback_pubkey, fallback_treename) = params;
    let parsed = parse_api_resolve_request_path(uri.path());
    let pubkey = parsed
        .as_ref()
        .map(|entry| entry.pubkey.clone())
        .unwrap_or(fallback_pubkey);
    let treename = parsed
        .as_ref()
        .map(|entry| entry.treename.clone())
        .unwrap_or(fallback_treename);
    let key = format!("{}/{}", pubkey, treename);
    let refresh = query_flag(&query, "refresh")
        || query_flag(&query, "force")
        || query_flag(&query, "skipCache");

    if !refresh {
        if let Some(resolved) = resolve_root_offline(&state, &pubkey, &treename, None).await {
            let mut payload = json!({
                "key": key,
                "hash": to_hex(&resolved.cid.hash),
                "cid": resolved.cid.to_string(),
                "source": resolved.source,
            });
            if let Some(root) = resolved.root_event {
                payload["peer"] = json!(root.peer_id);
                payload["event_id"] = json!(root.event_id);
                payload["created_at"] = json!(root.created_at);
                payload["key_tag"] = json!(root.key);
                payload["encryptedKey"] = json!(root.encrypted_key);
                payload["selfEncryptedKey"] = json!(root.self_encrypted_key);
            }
            return Json(payload);
        }
    }

    if let Some(resolved) = resolve_root_without_cache(&state, &pubkey, &treename, None).await {
        let mut payload = json!({
            "key": key,
            "hash": to_hex(&resolved.cid.hash),
            "cid": resolved.cid.to_string(),
            "source": resolved.source,
        });
        if let Some(root) = resolved.root_event {
            payload["peer"] = json!(root.peer_id);
            payload["event_id"] = json!(root.event_id);
            payload["created_at"] = json!(root.created_at);
            payload["key_tag"] = json!(root.key);
            payload["encryptedKey"] = json!(root.encrypted_key);
            payload["selfEncryptedKey"] = json!(root.self_encrypted_key);
        }
        return Json(payload);
    }

    let resolver = match NostrRootResolver::new(resolver_config(&state)).await {
        Ok(r) => r,
        Err(e) => {
            return Json(json!({
                "error": format!("Failed to create resolver: {}", e),
                "key": key
            }));
        }
    };

    let relay_result =
        match tokio::time::timeout(HTTP_RESOLVER_TIMEOUT, resolver.resolve_wait(&key)).await {
            Ok(Ok(cid)) => {
                cache_public_tree_root(&state, &pubkey, &treename, &cid);
                Some(Json(json!({
                    "key": key,
                    "hash": to_hex(&cid.hash),
                    "cid": cid.to_string(),
                    "source": "nostr",
                })))
            }
            Ok(Err(_)) | Err(_) => None,
        };

    let _ = resolver.stop().await;
    if let Some(result) = relay_result {
        return result;
    }

    Json(json!({
        "error": "Resolution failed via relays, multicast, and peers",
        "key": key
    }))
}

/// List all trees for a pubkey
pub async fn list_trees(
    State(state): State<AppState>,
    Path(pubkey): Path<String>,
) -> impl IntoResponse {
    let resolver = match NostrRootResolver::new(resolver_config(&state)).await {
        Ok(r) => r,
        Err(e) => {
            return Json(json!({
                "error": format!("Failed to create resolver: {}", e),
                "pubkey": pubkey
            }));
        }
    };

    // list() uses the configured timeout internally
    let result = match resolver.list(&pubkey).await {
        Ok(entries) => Json(json!({
            "pubkey": pubkey,
            "trees": entries.iter().map(|e| json!({
                "name": e.key.split('/').next_back().unwrap_or(&e.key),
                "hash": to_hex(&e.cid.hash),
                "cid": e.cid.to_string()
            })).collect::<Vec<_>>()
        })),
        Err(e) => Json(json!({
            "error": e.to_string(),
            "pubkey": pubkey
        })),
    };

    let _ = resolver.stop().await;
    result
}

/// Query connected mesh peers for content by hash
/// Returns the first successful response with peer_id, or None if no peer has it
async fn query_webrtc_peers(
    webrtc_state: &Arc<WebRTCState>,
    hash_hex: &str,
) -> Option<(Vec<u8>, String)> {
    if let Some((data, peer_id)) = webrtc_state.request_from_peers_with_source(hash_hex).await {
        tracing::info!(
            "Got {} bytes from peer {} for hash {}",
            data.len(),
            peer_id,
            &hash_hex[..16.min(hash_hex.len())]
        );
        return Some((data, peer_id));
    }

    tracing::debug!(
        "No connected mesh peer returned hash {}",
        &hash_hex[..16.min(hash_hex.len())]
    );
    None
}

/// Query upstream Blossom servers for content by hash
/// Returns the first successful response with server URL, or None if not found
async fn query_upstream_blossom(servers: &[String], hash_hex: &str) -> Option<(Vec<u8>, String)> {
    use sha2::{Digest, Sha256};

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .ok()?;

    let mut pending = FuturesUnordered::new();
    for server in servers {
        let client = client.clone();
        let server = server.clone();
        let hash_hex = hash_hex.to_string();
        pending.push(async move {
            let url = format!("{}/{}.bin", server.trim_end_matches('/'), hash_hex);
            tracing::debug!("Trying upstream Blossom: {}", url);

            match client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => match resp.bytes().await {
                    Ok(bytes) => {
                        let mut hasher = Sha256::new();
                        hasher.update(&bytes);
                        let computed = hex::encode(hasher.finalize());

                        if computed == hash_hex {
                            tracing::info!(
                                "Got {} bytes from upstream {} for hash {}",
                                bytes.len(),
                                server,
                                &hash_hex[..16.min(hash_hex.len())]
                            );
                            Some((bytes.to_vec(), server))
                        } else {
                            tracing::warn!(
                                "Hash mismatch from {}: expected {}, got {}",
                                server,
                                &hash_hex[..16.min(hash_hex.len())],
                                &computed[..16.min(computed.len())]
                            );
                            None
                        }
                    }
                    Err(err) => {
                        tracing::debug!("Upstream {} body read error: {}", server, err);
                        None
                    }
                },
                Ok(resp) => {
                    tracing::debug!("Upstream {} returned {}", server, resp.status());
                    None
                }
                Err(e) => {
                    tracing::debug!("Upstream {} error: {}", server, e);
                    None
                }
            }
        });
    }

    while let Some(result) = pending.next().await {
        if result.is_some() {
            return result;
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nostr_relay::{NostrRelay, NostrRelayConfig};
    use crate::socialgraph;
    use crate::storage::HashtreeStore;
    use crate::webrtc::{
        ConnectionState, PeerDirection, PeerEntry, PeerPool, PeerSignalPath, PeerTransport,
        WebRTCState,
    };
    use axum::{
        body::{to_bytes, Body},
        extract::{Path as AxumPath, State as AxumState},
        response::IntoResponse,
        routing::get,
        Router,
    };
    use hashtree_core::DirEntry;
    use http_body_util::BodyExt;
    use nostr::{
        nips::nip19::ToBech32, Alphabet, EventBuilder, Keys, Kind, SingleLetterTag, Tag, TagKind,
        Timestamp,
    };
    use sha2::Digest;
    use std::{
        collections::{BTreeSet, HashSet},
        net::SocketAddr,
        time::Instant,
    };
    use tempfile::TempDir;
    use tokio::time::timeout;

    #[derive(Clone)]
    struct UpstreamBlobTestState {
        store: Arc<HashtreeStore>,
        requested_ids: Arc<std::sync::Mutex<Vec<String>>>,
    }

    async fn serve_blob_for_test(
        AxumState(store): AxumState<Arc<HashtreeStore>>,
        AxumPath(id): AxumPath<String>,
    ) -> Response<Body> {
        let id = id.strip_suffix(".bin").unwrap_or(&id).to_string();
        let Ok(hash) = from_hex(&id) else {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("invalid hash"))
                .unwrap();
        };

        match store.get_blob(&hash) {
            Ok(Some(data)) => Response::builder()
                .status(StatusCode::OK)
                .body(Body::from(data))
                .unwrap(),
            Ok(None) => Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from("missing"))
                .unwrap(),
            Err(err) => Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from(err.to_string()))
                .unwrap(),
        }
    }

    async fn serve_blob_with_request_log_for_test(
        AxumState(state): AxumState<UpstreamBlobTestState>,
        AxumPath(id): AxumPath<String>,
    ) -> Response<Body> {
        state.requested_ids.lock().unwrap().push(id.clone());
        let id = id.strip_suffix(".bin").unwrap_or(&id).to_string();
        let Ok(hash) = from_hex(&id) else {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("invalid hash"))
                .unwrap();
        };

        match state.store.get_blob(&hash) {
            Ok(Some(data)) => Response::builder()
                .status(StatusCode::OK)
                .body(Body::from(data))
                .unwrap(),
            Ok(None) => Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from("missing"))
                .unwrap(),
            Err(err) => Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from(err.to_string()))
                .unwrap(),
        }
    }

    fn test_app_state(store: Arc<HashtreeStore>, upstream_blossom: Vec<String>) -> AppState {
        AppState {
            store,
            auth: None,
            webrtc_peers: None,
            ws_relay: Arc::new(crate::server::auth::WsRelayState::new()),
            max_upload_bytes: 5 * 1024 * 1024,
            public_writes: true,
            allowed_pubkeys: HashSet::new(),
            upstream_blossom,
            social_graph: None,
            social_graph_store: None,
            social_graph_root: None,
            socialgraph_snapshot_public: false,
            nostr_relay: None,
            nostr_relay_urls: Vec::new(),
            tree_root_cache: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            inflight_blob_fetches: Arc::new(tokio::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            directory_listing_cache: Arc::new(std::sync::Mutex::new(
                crate::server::new_lookup_cache(),
            )),
            resolved_path_cache: Arc::new(std::sync::Mutex::new(crate::server::new_lookup_cache())),
            thumbnail_path_cache: Arc::new(
                std::sync::Mutex::new(crate::server::new_lookup_cache()),
            ),
            cid_size_cache: Arc::new(std::sync::Mutex::new(crate::server::new_lookup_cache())),
        }
    }

    async fn sample_webrtc_state() -> Arc<WebRTCState> {
        let state = Arc::new(WebRTCState::new());
        let peer_id = crate::webrtc::PeerId::new(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string(),
        );
        let peer_key = peer_id.to_string();
        let signal_paths = BTreeSet::from([PeerSignalPath::Relay, PeerSignalPath::Multicast]);
        state.peers.write().await.insert(
            peer_key.clone(),
            PeerEntry {
                peer_id,
                direction: PeerDirection::Outbound,
                state: ConnectionState::Connected,
                last_seen: Instant::now(),
                peer: None,
                pool: PeerPool::Follows,
                transport: PeerTransport::WebRtc,
                signal_paths,
                bytes_sent: 64,
                bytes_received: 128,
            },
        );
        state.record_sent(&peer_key, 16).await;
        state.record_received(&peer_key, 32).await;
        state
    }

    async fn test_nostr_relay(dir: &TempDir, allowed_pubkey: String) -> Arc<NostrRelay> {
        let graph_store =
            socialgraph::open_social_graph_store_with_mapsize(dir.path(), Some(128 * 1024 * 1024))
                .unwrap();
        let backend: Arc<dyn socialgraph::SocialGraphBackend> = graph_store.clone();
        let mut allowed = HashSet::new();
        allowed.insert(allowed_pubkey.clone());
        let access = Arc::new(socialgraph::SocialGraphAccessControl::new(
            Arc::clone(&backend),
            0,
            allowed,
        ));

        Arc::new(
            NostrRelay::new(
                backend,
                dir.path().join("relay"),
                HashSet::from([allowed_pubkey]),
                Some(access),
                NostrRelayConfig {
                    spambox_db_max_bytes: 0,
                    ..Default::default()
                },
            )
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn test_query_upstream_blossom_no_servers() {
        let servers: Vec<String> = vec![];
        let result = query_upstream_blossom(&servers, "abc123").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn await_webrtc_peer_response_returns_success() {
        let result = await_webrtc_peer_response(
            async { Some((b"ok".to_vec(), "peer-a".to_string())) },
            "abcd1234",
            Duration::from_millis(10),
        )
        .await;

        assert_eq!(result, Some((b"ok".to_vec(), "peer-a".to_string())));
    }

    #[tokio::test]
    async fn webrtc_peers_reports_transport_and_signal_paths() {
        let temp = TempDir::new().unwrap();
        let store = Arc::new(HashtreeStore::new(temp.path()).unwrap());
        let mut state = test_app_state(store, vec![]);
        state.webrtc_peers = Some(sample_webrtc_state().await);

        let response = webrtc_peers(AxumState(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["enabled"], true);
        assert_eq!(json["transport_counts"]["webrtc"], 1);
        assert_eq!(json["transport_counts"]["bluetooth"], 0);
        assert_eq!(json["peers"][0]["transport"], "webrtc");
        assert_eq!(json["peers"][0]["bytes_sent"], 80);
        assert_eq!(json["peers"][0]["bytes_received"], 160);
        assert_eq!(
            json["peers"][0]["signal_paths"],
            json!(["relay", "multicast"])
        );
    }

    #[tokio::test]
    async fn daemon_status_exposes_mesh_alias_with_transport_metadata() {
        let temp = TempDir::new().unwrap();
        let store = Arc::new(HashtreeStore::new(temp.path()).unwrap());
        let mut state = test_app_state(store, vec![]);
        state.webrtc_peers = Some(sample_webrtc_state().await);
        state.nostr_relay_urls = vec![
            "wss://relay.damus.io".to_string(),
            "wss://nos.lol".to_string(),
        ];
        state.ws_relay.note_upstream_relay_send(512);
        state.ws_relay.note_upstream_relay_receive(1024);

        let response = daemon_status(
            AxumState(state),
            axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 21417))),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["mesh"]["enabled"], true);
        assert_eq!(json["mesh"]["transport_counts"]["webrtc"], 1);
        assert_eq!(json["mesh"]["bytes_sent"], 16);
        assert_eq!(json["mesh"]["bytes_received"], 32);
        assert_eq!(json["mesh"]["peers"][0]["transport"], "webrtc");
        assert_eq!(json["webrtc"], json["mesh"]);
        assert_eq!(json["relay"]["enabled"], true);
        assert_eq!(json["relay"]["bytes_sent"], 512);
        assert_eq!(json["relay"]["bytes_received"], 1024);
        assert_eq!(json["upstream"]["nostr_relays"], 2);
    }

    #[tokio::test]
    async fn await_webrtc_peer_response_times_out() {
        let result = await_webrtc_peer_response(
            std::future::pending::<Option<(Vec<u8>, String)>>(),
            "abcd1234",
            Duration::from_millis(10),
        )
        .await;

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn first_available_fetch_prefers_fast_success() {
        let result = first_available_fetch(vec![
            async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                Some("slow")
            }
            .boxed(),
            async {
                tokio::time::sleep(Duration::from_millis(5)).await;
                Some("fast")
            }
            .boxed(),
        ])
        .await;

        assert_eq!(result, Some("fast"));
    }

    #[tokio::test]
    async fn first_available_fetch_skips_empty_results() {
        let result = first_available_fetch(vec![
            async { None::<&'static str> }.boxed(),
            async {
                tokio::time::sleep(Duration::from_millis(5)).await;
                Some("available")
            }
            .boxed(),
        ])
        .await;

        assert_eq!(result, Some("available"));
    }

    #[tokio::test]
    async fn await_fetch_task_returns_result() {
        let result = await_fetch_task("test", "abc123", async { Some(7usize) }).await;
        assert_eq!(result, Some(7));
    }

    #[tokio::test]
    async fn await_fetch_task_recovers_from_panic() {
        let result: Option<usize> = await_fetch_task("test", "abc123", async move {
            panic!("boom");
        })
        .await;

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_query_upstream_blossom_invalid_server() {
        let servers = vec!["http://localhost:99999".to_string()];
        let result = query_upstream_blossom(&servers, "abc123").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_query_upstream_blossom_hash_format() {
        // Test with valid SHA256 hash format but non-existent server
        let servers = vec!["http://localhost:99999".to_string()];
        let hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let result = query_upstream_blossom(&servers, hash).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_query_upstream_blossom_uses_bin_suffix() {
        let temp_dir = TempDir::new().unwrap();
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
        let requested_ids = Arc::new(std::sync::Mutex::new(Vec::new()));

        let data = b"hello blossom";
        store.put_blob(data).unwrap();
        let hash_hex = hex::encode(sha2::Sha256::digest(data));

        let upstream_router = Router::new()
            .route("/:id", get(serve_blob_with_request_log_for_test))
            .with_state(UpstreamBlobTestState {
                store: store.clone(),
                requested_ids: requested_ids.clone(),
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = listener.local_addr().unwrap();
        let upstream_server =
            tokio::spawn(async move { axum::serve(listener, upstream_router).await.unwrap() });

        let result = query_upstream_blossom(&[format!("http://{}", upstream_addr)], &hash_hex)
            .await
            .expect("fetch blob");
        assert_eq!(result.0, data);
        assert_eq!(result.1, format!("http://{}", upstream_addr));
        assert_eq!(
            requested_ids.lock().unwrap().as_slice(),
            &[format!("{}.bin", hash_hex)]
        );

        upstream_server.abort();
    }

    #[tokio::test]
    async fn query_upstream_blossom_uses_first_server_that_responds() {
        let temp_dir = TempDir::new().unwrap();
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
        let requested_ids = Arc::new(std::sync::Mutex::new(Vec::new()));

        let data = b"parallel blossom";
        store.put_blob(data).unwrap();
        let hash_hex = hex::encode(sha2::Sha256::digest(data));

        let slow_router = Router::new().route(
            "/:id",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(11)).await;
                StatusCode::GATEWAY_TIMEOUT
            }),
        );
        let slow_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let slow_addr = slow_listener.local_addr().unwrap();
        let slow_server =
            tokio::spawn(async move { axum::serve(slow_listener, slow_router).await.unwrap() });

        let fast_router = Router::new()
            .route("/:id", get(serve_blob_with_request_log_for_test))
            .with_state(UpstreamBlobTestState {
                store: store.clone(),
                requested_ids: requested_ids.clone(),
            });
        let fast_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fast_addr = fast_listener.local_addr().unwrap();
        let fast_server =
            tokio::spawn(async move { axum::serve(fast_listener, fast_router).await.unwrap() });

        let result = timeout(
            Duration::from_secs(3),
            query_upstream_blossom(
                &[
                    format!("http://{}", slow_addr),
                    format!("http://{}", fast_addr),
                ],
                &hash_hex,
            ),
        )
        .await
        .expect("parallel upstream query completed")
        .expect("fetch blob");

        assert_eq!(result.0, data);
        assert_eq!(result.1, format!("http://{}", fast_addr));
        assert_eq!(
            requested_ids.lock().unwrap().as_slice(),
            &[format!("{}.bin", hash_hex)]
        );

        slow_server.abort();
        fast_server.abort();
    }

    #[tokio::test]
    async fn ensure_blob_available_coalesces_concurrent_upstream_fetches() {
        let source_dir = TempDir::new().unwrap();
        let source_store =
            Arc::new(HashtreeStore::new(source_dir.path().join("source-db")).unwrap());
        let requested_ids = Arc::new(std::sync::Mutex::new(Vec::new()));
        let data = b"shared-upstream-blob";
        source_store.put_blob(data).unwrap();
        let hash = from_hex(&hex::encode(sha2::Sha256::digest(data))).unwrap();

        let upstream_router = Router::new()
            .route("/:id", get(serve_blob_with_request_log_for_test))
            .with_state(UpstreamBlobTestState {
                store: source_store.clone(),
                requested_ids: requested_ids.clone(),
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = listener.local_addr().unwrap();
        let upstream_server =
            tokio::spawn(async move { axum::serve(listener, upstream_router).await.unwrap() });

        let local_dir = TempDir::new().unwrap();
        let local_store = Arc::new(HashtreeStore::new(local_dir.path().join("local-db")).unwrap());
        let state = test_app_state(
            local_store.clone(),
            vec![format!("http://{}", upstream_addr)],
        );

        let (first, second, third) = tokio::join!(
            ensure_blob_available(&state, &hash),
            ensure_blob_available(&state, &hash),
            ensure_blob_available(&state, &hash),
        );

        assert_eq!(first.unwrap(), true);
        assert_eq!(second.unwrap(), true);
        assert_eq!(third.unwrap(), true);
        assert_eq!(
            requested_ids.lock().unwrap().as_slice(),
            &[format!("{}.bin", hex::encode(hash))]
        );
        assert!(local_store.get_blob(&hash).unwrap().is_some());

        upstream_server.abort();
    }

    #[tokio::test]
    async fn resolve_thumbnail_path_prefers_root_thumbnail() {
        let temp_dir = TempDir::new().unwrap();
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()));
        let state = test_app_state(store.clone(), Vec::new());

        let (thumb_cid, _size) = tree.put(b"thumb").await.unwrap();
        let root_cid = tree
            .put_directory(vec![
                DirEntry::from_cid("thumbnail.jpg", &thumb_cid).with_link_type(LinkType::File)
            ])
            .await
            .unwrap();

        let resolved = resolve_thumbnail_path(&state, &tree, &root_cid, "thumbnail")
            .await
            .unwrap();
        assert_eq!(resolved.as_deref(), Some("thumbnail.jpg"));
    }

    #[tokio::test]
    async fn resolve_thumbnail_path_accepts_generic_image_names() {
        let temp_dir = TempDir::new().unwrap();
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()));
        let state = test_app_state(store.clone(), Vec::new());

        let (thumb_cid, _size) = tree.put(b"thumb").await.unwrap();
        let root_cid = tree
            .put_directory(vec![
                DirEntry::from_cid("cover.jpeg", &thumb_cid).with_link_type(LinkType::File)
            ])
            .await
            .unwrap();

        let resolved = resolve_thumbnail_path(&state, &tree, &root_cid, "thumbnail")
            .await
            .unwrap();
        assert_eq!(resolved.as_deref(), Some("cover.jpeg"));
    }

    #[tokio::test]
    async fn resolve_thumbnail_path_falls_back_to_subdir() {
        let temp_dir = TempDir::new().unwrap();
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()));
        let state = test_app_state(store.clone(), Vec::new());

        let (thumb_cid, _size) = tree.put(b"thumb").await.unwrap();
        let subdir_cid = tree
            .put_directory(vec![
                DirEntry::from_cid("thumbnail.png", &thumb_cid).with_link_type(LinkType::File)
            ])
            .await
            .unwrap();

        let (meta_cid, _size) = tree.put(b"{}").await.unwrap();
        let root_cid = tree
            .put_directory(vec![
                DirEntry::from_cid("clip", &subdir_cid).with_link_type(LinkType::Dir),
                DirEntry::from_cid("meta.json", &meta_cid).with_link_type(LinkType::File),
            ])
            .await
            .unwrap();

        let resolved = resolve_thumbnail_path(&state, &tree, &root_cid, "thumbnail")
            .await
            .unwrap();
        assert_eq!(resolved.as_deref(), Some("clip/thumbnail.png"));
    }

    #[tokio::test]
    async fn resolve_thumbnail_path_fetches_missing_subdir_from_upstream() {
        let source_dir = TempDir::new().unwrap();
        let source_store =
            Arc::new(HashtreeStore::new(source_dir.path().join("source-db")).unwrap());
        let source_tree = HashTree::new(HashTreeConfig::new(source_store.store_arc()));

        let (thumb_cid, _size) = source_tree.put(b"thumb").await.unwrap();
        let subdir_cid = source_tree
            .put_directory(vec![
                DirEntry::from_cid("thumbnail.jpg", &thumb_cid).with_link_type(LinkType::File)
            ])
            .await
            .unwrap();
        let root_cid = source_tree
            .put_directory(vec![
                DirEntry::from_cid("clip", &subdir_cid).with_link_type(LinkType::Dir)
            ])
            .await
            .unwrap();

        let upstream_router = Router::new()
            .route("/:id", get(serve_blob_for_test))
            .with_state(source_store.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = listener.local_addr().unwrap();
        let upstream_server =
            tokio::spawn(async move { axum::serve(listener, upstream_router).await.unwrap() });

        let local_dir = TempDir::new().unwrap();
        let local_store = Arc::new(HashtreeStore::new(local_dir.path().join("local-db")).unwrap());
        let state = test_app_state(
            local_store.clone(),
            vec![format!("http://{}", upstream_addr)],
        );
        let local_tree = HashTree::new(HashTreeConfig::new(local_store.store_arc()));

        let resolved = resolve_thumbnail_path(&state, &local_tree, &root_cid, "thumbnail")
            .await
            .unwrap();
        assert_eq!(resolved.as_deref(), Some("clip/thumbnail.jpg"));

        upstream_server.abort();
    }

    #[tokio::test]
    async fn resolve_directory_target_prefers_root_index() {
        let temp_dir = TempDir::new().unwrap();
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()));
        let state = test_app_state(store.clone(), Vec::new());

        let (index_cid, _size) = tree.put(b"<html>ok</html>").await.unwrap();
        let root_cid = tree
            .put_directory(vec![
                DirEntry::from_cid("index.html", &index_cid).with_link_type(LinkType::File)
            ])
            .await
            .unwrap();

        let target = resolve_directory_target(&state, &tree, &root_cid, None)
            .await
            .expect("resolve")
            .expect("target");

        match target {
            DirectoryTarget::File { cid, path } => {
                assert_eq!(cid, index_cid);
                assert_eq!(path, "index.html");
            }
            DirectoryTarget::DirectoryListing { .. } => panic!("expected file target"),
        }
    }

    #[tokio::test]
    async fn resolve_directory_target_prefers_subdir_index() {
        let temp_dir = TempDir::new().unwrap();
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()));
        let state = test_app_state(store.clone(), Vec::new());

        let (index_cid, _size) = tree.put(b"<html>nested</html>").await.unwrap();
        let subdir_cid = tree
            .put_directory(vec![
                DirEntry::from_cid("index.html", &index_cid).with_link_type(LinkType::File)
            ])
            .await
            .unwrap();
        let root_cid = tree
            .put_directory(vec![
                DirEntry::from_cid("video", &subdir_cid).with_link_type(LinkType::Dir)
            ])
            .await
            .unwrap();

        let target = resolve_directory_target(&state, &tree, &root_cid, Some("video".to_string()))
            .await
            .expect("resolve")
            .expect("target");

        match target {
            DirectoryTarget::File { cid, path } => {
                assert_eq!(cid, index_cid);
                assert_eq!(path, "video/index.html");
            }
            DirectoryTarget::DirectoryListing { .. } => panic!("expected file target"),
        }
    }

    #[tokio::test]
    async fn resolve_directory_target_lists_directory_without_index() {
        let temp_dir = TempDir::new().unwrap();
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()));
        let state = test_app_state(store.clone(), Vec::new());

        let (file_cid, _size) = tree.put(b"asset").await.unwrap();
        let root_cid = tree
            .put_directory(vec![
                DirEntry::from_cid("asset.txt", &file_cid).with_link_type(LinkType::File)
            ])
            .await
            .unwrap();

        let target = resolve_directory_target(&state, &tree, &root_cid, None)
            .await
            .expect("resolve")
            .expect("target");

        match target {
            DirectoryTarget::DirectoryListing { cid } => assert_eq!(cid, root_cid),
            DirectoryTarget::File { .. } => panic!("expected directory listing"),
        }
    }

    #[test]
    fn content_type_for_path_uses_extension() {
        assert_eq!(content_type_for_path(Some("dir/video.mp4")), "video/mp4");
        assert_eq!(content_type_for_path(Some("image.jpeg")), "image/jpeg");
        assert_eq!(content_type_for_path(None), "application/octet-stream");
    }

    #[test]
    fn parse_mutable_htree_request_path_decodes_tree_names_with_slashes() {
        assert_eq!(
            parse_mutable_htree_request_path(
                "/htree/npub1example/releases%2Fnostr-vpn/v0.3.0/assets/nostr-vpn-v0.3.0-macos-arm64.zip"
            ),
            Some(ParsedMutableHtreeRequestPath {
                npub: "npub1example".to_string(),
                treename: "releases/nostr-vpn".to_string(),
                path: Some("v0.3.0/assets/nostr-vpn-v0.3.0-macos-arm64.zip".to_string()),
            })
        );
    }

    #[test]
    fn parse_api_resolve_request_path_decodes_tree_names_with_slashes() {
        assert_eq!(
            parse_api_resolve_request_path("/api/resolve/npub1example/releases%2Fnostr-vpn"),
            Some(ParsedTreeRequestPath {
                pubkey: "npub1example".to_string(),
                treename: "releases/nostr-vpn".to_string(),
                path: None,
            })
        );
    }

    #[test]
    fn parse_bare_npub_request_path_decodes_tree_names_with_slashes() {
        assert_eq!(
            parse_bare_npub_request_path("/npub1example/releases%2Fnostr-vpn/latest"),
            Some(ParsedMutableHtreeRequestPath {
                npub: "npub1example".to_string(),
                treename: "releases/nostr-vpn".to_string(),
                path: Some("latest".to_string()),
            })
        );
    }

    #[tokio::test]
    async fn htree_nhash_path_fetches_nested_assets_from_upstream_tree() {
        let source_dir = TempDir::new().unwrap();
        let source_store =
            Arc::new(HashtreeStore::new(source_dir.path().join("source-db")).unwrap());

        let site_dir = source_dir.path().join("site");
        let assets_dir = site_dir.join("assets");
        std::fs::create_dir_all(&assets_dir).unwrap();

        let index_html = r#"
<!doctype html>
<html>
  <head><script type="module" src="./assets/main.js"></script></head>
  <body>ok</body>
</html>
"#;
        let main_js = "export const big = '".to_string() + &"x".repeat(2_500_000) + "';\n";

        std::fs::write(site_dir.join("index.html"), index_html).unwrap();
        std::fs::write(assets_dir.join("main.js"), &main_js).unwrap();

        let root_hash = source_store
            .upload_dir_with_options(&site_dir, true)
            .expect("upload site");
        let root_hash_bytes = from_hex(&root_hash).expect("hex root hash");
        let nhash = hashtree_core::nhash_encode(&root_hash_bytes).expect("encode nhash");
        let route_nhash = nhash.strip_prefix("nhash1").expect("nhash prefix");

        let upstream_router = Router::new()
            .route("/:id", get(serve_blob_for_test))
            .with_state(source_store.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = listener.local_addr().unwrap();
        let _server = tokio::spawn(async move {
            axum::serve(listener, upstream_router).await.unwrap();
        });

        let target_dir = TempDir::new().unwrap();
        let target_store =
            Arc::new(HashtreeStore::new(target_dir.path().join("target-db")).unwrap());
        let state = test_app_state(target_store, vec![format!("http://{}", upstream_addr)]);

        let response = htree_nhash_path(
            State(state),
            Path((route_nhash.to_string(), "assets/main.js".to_string())),
            Query(HashMap::new()),
            axum::http::HeaderMap::new(),
            axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43123))),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(CROSS_ORIGIN_RESOURCE_POLICY_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some(CORP_CROSS_ORIGIN)
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.as_ref(), main_js.as_bytes());
    }

    #[tokio::test]
    async fn htree_nhash_path_resolves_thumbnail_alias() {
        let temp_dir = TempDir::new().unwrap();
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());

        let thumb_bytes = vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46];
        let (thumb_cid, _) = tree.put(&thumb_bytes).await.unwrap();
        let root_cid = tree
            .put_directory(vec![
                DirEntry::from_cid("thumbnail.jpg", &thumb_cid).with_link_type(LinkType::File)
            ])
            .await
            .unwrap();

        let nhash = hashtree_core::nhash_encode(&root_cid.hash).expect("encode nhash");
        let route_nhash = nhash.strip_prefix("nhash1").expect("nhash prefix");

        let response = htree_nhash_path(
            State(test_app_state(store, Vec::new())),
            Path((route_nhash.to_string(), "thumbnail".to_string())),
            Query(HashMap::new()),
            axum::http::HeaderMap::new(),
            axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43123))),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.as_ref(), thumb_bytes.as_slice());
    }

    #[test]
    fn parse_byte_range_supports_suffix_requests() {
        match parse_byte_range("bytes=-500", 1000) {
            Some(ParsedByteRange::Satisfiable {
                start,
                end_inclusive,
            }) => {
                assert_eq!(start, 500);
                assert_eq!(end_inclusive, 999);
            }
            _ => panic!("expected satisfiable suffix range"),
        }
    }

    #[test]
    fn parse_byte_range_clamps_large_suffix_requests() {
        match parse_byte_range("bytes=-5000", 1000) {
            Some(ParsedByteRange::Satisfiable {
                start,
                end_inclusive,
            }) => {
                assert_eq!(start, 0);
                assert_eq!(end_inclusive, 999);
            }
            _ => panic!("expected satisfiable suffix range"),
        }
    }

    #[tokio::test]
    async fn serve_cid_with_range_honors_suffix_ranges() {
        let temp_dir = TempDir::new().unwrap();
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
        let state = test_app_state(store.clone(), Vec::new());
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());
        let data = b"0123456789";
        let (cid, _) = tree.put(data).await.unwrap();

        let mut headers = axum::http::HeaderMap::new();
        headers.insert(header::RANGE, header::HeaderValue::from_static("bytes=-4"));

        let response =
            serve_cid_with_range(&state, &cid, headers, false, false, Some("clip.mp4")).await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_RANGE)
                .and_then(|value| value.to_str().ok()),
            Some("bytes 6-9/10")
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.as_ref(), b"6789");
    }

    #[tokio::test]
    async fn serve_cid_with_range_streams_large_explicit_ranges() {
        let temp_dir = TempDir::new().unwrap();
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
        let state = test_app_state(store.clone(), Vec::new());
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());
        let data: Vec<u8> = (0..(5 * 1024 * 1024 + 17))
            .map(|i| (i % 251) as u8)
            .collect();
        let (cid, _) = tree.put(&data).await.unwrap();

        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            header::RANGE,
            header::HeaderValue::from_str(&format!("bytes=0-{}", data.len() - 1)).unwrap(),
        );

        let response =
            serve_cid_with_range(&state, &cid, headers, true, false, Some("clip.mp4")).await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);

        let mut body = response.into_body();
        let first_frame = timeout(Duration::from_secs(1), body.frame())
            .await
            .expect("first body frame should arrive quickly")
            .expect("body should yield a frame")
            .expect("body frame should be ok");
        let first_chunk = first_frame
            .into_data()
            .expect("first frame should contain bytes");
        assert_eq!(first_chunk.len(), CID_RANGE_STREAM_CHUNK_SIZE as usize);
    }

    fn copy_blob_between_stores(
        source_store: &Arc<HashtreeStore>,
        target_store: &Arc<HashtreeStore>,
        hash: &[u8; 32],
    ) {
        let data = source_store
            .get_blob(hash)
            .unwrap()
            .unwrap_or_else(|| panic!("missing blob {}", to_hex(hash)));
        target_store.put_blob(&data).unwrap();
    }

    #[tokio::test]
    async fn htree_npub_path_range_fetches_missing_nested_file_from_upstream() {
        let source_dir = TempDir::new().unwrap();
        let source_store =
            Arc::new(HashtreeStore::new(source_dir.path().join("source-db")).unwrap());
        let source_tree = HashTree::new(HashTreeConfig::new(source_store.store_arc()));

        let video_data: Vec<u8> = (0..(3 * 1024 * 1024 + 137))
            .map(|i| (i % 251) as u8)
            .collect();
        let (video_cid, _) = source_tree.put(&video_data).await.unwrap();
        let child_dir_cid = source_tree
            .put_directory(vec![
                DirEntry::from_cid("video.mp4", &video_cid).with_link_type(LinkType::File)
            ])
            .await
            .unwrap();
        let root_cid = source_tree
            .put_directory(vec![DirEntry::from_cid(
                "video_1767136282070",
                &child_dir_cid,
            )
            .with_link_type(LinkType::Dir)])
            .await
            .unwrap();

        let upstream_router = Router::new()
            .route("/:id", get(serve_blob_for_test))
            .with_state(source_store.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = listener.local_addr().unwrap();
        let upstream_server =
            tokio::spawn(async move { axum::serve(listener, upstream_router).await.unwrap() });

        let local_dir = TempDir::new().unwrap();
        let local_store = Arc::new(HashtreeStore::new(local_dir.path().join("local-db")).unwrap());

        // Simulate a warm playlist lookup: directory nodes are local, the media file is not.
        copy_blob_between_stores(&source_store, &local_store, &root_cid.hash);
        copy_blob_between_stores(&source_store, &local_store, &child_dir_cid.hash);

        let state = test_app_state(
            local_store.clone(),
            vec![format!("http://{}", upstream_addr)],
        );
        put_cached_tree_root(
            &state,
            tree_root_cache_key("npub1example", "videos/Music", None),
            root_cid.clone(),
            "cache",
            None,
        );

        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            header::RANGE,
            header::HeaderValue::from_static("bytes=0-1023"),
        );

        let response = htree_npub_impl(
            State(state),
            "npub1example".to_string(),
            "videos/Music".to_string(),
            Some("video_1767136282070/video.mp4".to_string()),
            Query(HashMap::new()),
            headers,
            axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43123))),
        )
        .await;

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.as_ref(), &video_data[..1024]);

        upstream_server.abort();
    }

    #[tokio::test]
    async fn htree_npub_path_range_fetches_missing_nested_file_chunks_from_upstream() {
        let source_dir = TempDir::new().unwrap();
        let source_store =
            Arc::new(HashtreeStore::new(source_dir.path().join("source-db")).unwrap());
        let source_tree = HashTree::new(HashTreeConfig::new(source_store.store_arc()));

        let video_data: Vec<u8> = (0..(5 * 1024 * 1024 + 17))
            .map(|i| 255 - (i % 251) as u8)
            .collect();
        let (video_cid, _) = source_tree.put(&video_data).await.unwrap();
        let child_dir_cid = source_tree
            .put_directory(vec![
                DirEntry::from_cid("video.mp4", &video_cid).with_link_type(LinkType::File)
            ])
            .await
            .unwrap();
        let root_cid = source_tree
            .put_directory(vec![DirEntry::from_cid(
                "video_1767136255334",
                &child_dir_cid,
            )
            .with_link_type(LinkType::Dir)])
            .await
            .unwrap();

        let upstream_router = Router::new()
            .route("/:id", get(serve_blob_for_test))
            .with_state(source_store.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = listener.local_addr().unwrap();
        let upstream_server =
            tokio::spawn(async move { axum::serve(listener, upstream_router).await.unwrap() });

        let local_dir = TempDir::new().unwrap();
        let local_store = Arc::new(HashtreeStore::new(local_dir.path().join("local-db")).unwrap());

        // Simulate a warmer cache: the file tree is local, but its encrypted chunks are not.
        copy_blob_between_stores(&source_store, &local_store, &root_cid.hash);
        copy_blob_between_stores(&source_store, &local_store, &child_dir_cid.hash);
        copy_blob_between_stores(&source_store, &local_store, &video_cid.hash);

        let state = test_app_state(
            local_store.clone(),
            vec![format!("http://{}", upstream_addr)],
        );
        put_cached_tree_root(
            &state,
            tree_root_cache_key("npub1example", "videos/Music", None),
            root_cid.clone(),
            "cache",
            None,
        );

        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            header::RANGE,
            header::HeaderValue::from_static("bytes=0-1023"),
        );

        let response = htree_npub_impl(
            State(state),
            "npub1example".to_string(),
            "videos/Music".to_string(),
            Some("video_1767136255334/video.mp4".to_string()),
            Query(HashMap::new()),
            headers,
            axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43123))),
        )
        .await;

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.as_ref(), &video_data[..1024]);

        upstream_server.abort();
    }

    #[tokio::test]
    async fn htree_npub_path_uses_original_uri_for_encoded_tree_names() {
        let temp_dir = TempDir::new().unwrap();
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());

        let asset_bytes = b"nostr-vpn-macos-zip".to_vec();
        let (asset_cid, _) = tree.put(&asset_bytes).await.unwrap();
        let assets_dir = tree
            .put_directory(vec![DirEntry::from_cid(
                "nostr-vpn-v0.3.0-macos-arm64.zip",
                &asset_cid,
            )
            .with_link_type(LinkType::File)])
            .await
            .unwrap();
        let version_dir = tree
            .put_directory(vec![
                DirEntry::from_cid("assets", &assets_dir).with_link_type(LinkType::Dir)
            ])
            .await
            .unwrap();
        let root_cid = tree
            .put_directory(vec![
                DirEntry::from_cid("v0.3.0", &version_dir).with_link_type(LinkType::Dir)
            ])
            .await
            .unwrap();

        let state = test_app_state(store, Vec::new());
        put_cached_tree_root(
            &state,
            tree_root_cache_key("npub1example", "releases/nostr-vpn", None),
            root_cid.clone(),
            "cache",
            None,
        );

        let response = htree_npub_path(
            State(state),
            OriginalUri(
                "/htree/npub1example/releases%2Fnostr-vpn/v0.3.0/assets/nostr-vpn-v0.3.0-macos-arm64.zip"
                    .parse()
                    .unwrap(),
            ),
            Path((
                "example".to_string(),
                "releases%2Fnostr-vpn".to_string(),
                "v0.3.0/assets/nostr-vpn-v0.3.0-macos-arm64.zip".to_string(),
            )),
            Query(HashMap::new()),
            axum::http::HeaderMap::new(),
            axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43123))),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.as_ref(), asset_bytes.as_slice());
    }

    #[tokio::test]
    async fn serve_content_internal_honors_suffix_ranges() {
        let temp_dir = TempDir::new().unwrap();
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
        let state = test_app_state(store.clone(), Vec::new());
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());
        let data = b"abcdefghij";
        let (cid, _) = tree.put(data).await.unwrap();

        let mut headers = axum::http::HeaderMap::new();
        headers.insert(header::RANGE, header::HeaderValue::from_static("bytes=-3"));

        let response = serve_content_internal(&state, &cid.hash, headers, true, false).await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_RANGE)
                .and_then(|value| value.to_str().ok()),
            Some("bytes 7-9/10")
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.as_ref(), b"hij");
    }

    #[tokio::test]
    async fn cache_tree_root_seeds_mutable_root_cache() {
        let temp_dir = TempDir::new().unwrap();
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
        let state = test_app_state(store, Vec::new());

        let response = cache_tree_root(
            State(state.clone()),
            Json(CacheTreeRootRequest {
                npub: "npub1example".to_string(),
                tree_name: "video".to_string(),
                hash: "988db3f24dc222715f1c1e1fa5876690d3147122243d72d85fd44283867cd61a"
                    .to_string(),
                key: None,
                visibility: Some("public".to_string()),
            }),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let cached = get_cached_tree_root(&state, "npub1example/video").expect("cached cid");
        assert_eq!(
            to_hex(&cached.cid.hash),
            "988db3f24dc222715f1c1e1fa5876690d3147122243d72d85fd44283867cd61a"
        );
        assert!(cached.cid.key.is_none());
    }

    #[tokio::test]
    async fn resolve_root_offline_accepts_npub_owner_for_local_relay_events() {
        let temp_dir = TempDir::new().unwrap();
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
        let keys = Keys::generate();
        let relay = test_nostr_relay(&temp_dir, keys.public_key().to_hex()).await;
        let state = AppState {
            nostr_relay: Some(relay.clone()),
            ..test_app_state(store, Vec::new())
        };
        let hash_hex = "ab".repeat(32);
        let tree_name = "offline-tree";
        let event = EventBuilder::new(
            Kind::Custom(30078),
            "",
            [
                Tag::identifier(tree_name.to_string()),
                Tag::custom(
                    TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::L)),
                    vec!["hashtree".to_string()],
                ),
                Tag::custom(TagKind::Custom("hash".into()), vec![hash_hex.clone()]),
            ],
        )
        .to_event(&keys)
        .unwrap();
        relay.ingest_trusted_event(event.clone()).await.unwrap();

        let resolved = resolve_root_offline(
            &state,
            &keys.public_key().to_bech32().unwrap(),
            tree_name,
            None,
        )
        .await
        .expect("offline root should resolve from local relay with npub");

        assert_eq!(resolved.source, "local-relay");
        assert_eq!(to_hex(&resolved.cid.hash), hash_hex);
        assert_eq!(
            resolved
                .root_event
                .as_ref()
                .map(|root| root.event_id.as_str()),
            Some(event.id.to_hex().as_str())
        );
    }

    #[test]
    fn resolver_config_prefers_state_relay_urls() {
        let temp_dir = TempDir::new().unwrap();
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
        let mut state = test_app_state(store, Vec::new());
        state.nostr_relay_urls = vec![
            "wss://temp.iris.to".to_string(),
            "wss://upload.iris.to/nostr".to_string(),
        ];

        let config = resolver_config(&state);

        assert_eq!(config.relays, state.nostr_relay_urls);
        assert_eq!(config.resolve_timeout, HTTP_RESOLVER_TIMEOUT);
    }

    #[tokio::test]
    async fn resolve_to_hash_refresh_skips_cached_root() {
        let temp_dir = TempDir::new().unwrap();
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
        let state = test_app_state(store, Vec::new());
        let hash_hex = "11".repeat(32);
        let cid = Cid::parse(&hash_hex).expect("valid cid");
        put_cached_tree_root(
            &state,
            tree_root_cache_key("npub1example", "video", None),
            cid,
            "cache",
            None,
        );

        let cached = resolve_to_hash(
            State(state.clone()),
            OriginalUri("/api/resolve/npub1example/video".parse().unwrap()),
            Path(("npub1example".to_string(), "video".to_string())),
            Query(HashMap::new()),
        )
        .await
        .into_response();
        let cached_body = to_bytes(cached.into_body(), usize::MAX).await.unwrap();
        let cached_json: serde_json::Value = serde_json::from_slice(&cached_body).unwrap();
        assert_eq!(cached_json["hash"], hash_hex);
        assert_eq!(cached_json["source"], "cache");

        let refresh = resolve_to_hash(
            State(state),
            OriginalUri("/api/resolve/npub1example/video".parse().unwrap()),
            Path(("npub1example".to_string(), "video".to_string())),
            Query(HashMap::from([("refresh".to_string(), "1".to_string())])),
        )
        .await
        .into_response();
        let refresh_body = to_bytes(refresh.into_body(), usize::MAX).await.unwrap();
        let refresh_json: serde_json::Value = serde_json::from_slice(&refresh_body).unwrap();
        assert!(refresh_json.get("error").is_some());
        assert_eq!(refresh_json["key"], "npub1example/video");
    }

    #[tokio::test]
    async fn resolve_to_hash_refresh_uses_local_relay_before_relays() {
        let temp_dir = TempDir::new().unwrap();
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
        let keys = Keys::generate();
        let relay = test_nostr_relay(&temp_dir, keys.public_key().to_hex()).await;
        let tree_name = "video";
        let cached_hash = "11".repeat(32);
        let refreshed_hash = "22".repeat(32);

        let event = EventBuilder::new(
            Kind::Custom(30078),
            "",
            [
                Tag::identifier(tree_name.to_string()),
                Tag::custom(
                    TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::L)),
                    vec!["hashtree".to_string()],
                ),
                Tag::custom(TagKind::Custom("hash".into()), vec![refreshed_hash.clone()]),
            ],
        )
        .to_event(&keys)
        .unwrap();
        relay.ingest_trusted_event(event.clone()).await.unwrap();

        let state = AppState {
            nostr_relay: Some(relay),
            ..test_app_state(store, Vec::new())
        };
        put_cached_tree_root(
            &state,
            tree_root_cache_key(&keys.public_key().to_bech32().unwrap(), tree_name, None),
            Cid::parse(&cached_hash).expect("valid cached cid"),
            "cache",
            None,
        );

        let refresh = resolve_to_hash(
            State(state),
            OriginalUri(
                format!(
                    "/api/resolve/{}/{}",
                    keys.public_key().to_bech32().unwrap(),
                    tree_name
                )
                .parse()
                .unwrap(),
            ),
            Path((
                keys.public_key().to_bech32().unwrap(),
                tree_name.to_string(),
            )),
            Query(HashMap::from([("refresh".to_string(), "1".to_string())])),
        )
        .await
        .into_response();
        let refresh_body = to_bytes(refresh.into_body(), usize::MAX).await.unwrap();
        let refresh_json: serde_json::Value = serde_json::from_slice(&refresh_body).unwrap();
        assert_eq!(refresh_json["hash"], refreshed_hash);
        assert_eq!(refresh_json["source"], "local-relay");
        assert_eq!(refresh_json["event_id"], event.id.to_hex());
    }

    #[tokio::test]
    async fn htree_npub_path_thumbnail_does_not_fall_back_to_historical_root() {
        let temp_dir = TempDir::new().unwrap();
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());
        let keys = Keys::generate();
        let relay = test_nostr_relay(&temp_dir, keys.public_key().to_hex()).await;

        let thumb_bytes = vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46];
        let (thumb_cid, _) = tree.put(&thumb_bytes).await.unwrap();
        let historical_root = tree
            .put_directory(vec![
                DirEntry::from_cid("thumbnail.jpg", &thumb_cid).with_link_type(LinkType::File)
            ])
            .await
            .unwrap();
        let (video_cid, _) = tree.put(b"video-data").await.unwrap();
        let current_root = tree
            .put_directory(vec![
                DirEntry::from_cid("video.mp4", &video_cid).with_link_type(LinkType::File)
            ])
            .await
            .unwrap();

        let tree_name = "videos/Mine Bombers in-game music";
        let historical_event = EventBuilder::new(
            Kind::Custom(30078),
            "",
            [
                Tag::identifier(tree_name.to_string()),
                Tag::custom(
                    TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::L)),
                    vec!["hashtree".to_string()],
                ),
                Tag::custom(
                    TagKind::Custom("hash".into()),
                    vec![to_hex(&historical_root.hash)],
                ),
            ],
        )
        .custom_created_at(Timestamp::from(10))
        .to_event(&keys)
        .unwrap();
        relay
            .ingest_trusted_event(historical_event.clone())
            .await
            .unwrap();

        let current_event = EventBuilder::new(
            Kind::Custom(30078),
            "",
            [
                Tag::identifier(tree_name.to_string()),
                Tag::custom(
                    TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::L)),
                    vec!["hashtree".to_string()],
                ),
                Tag::custom(
                    TagKind::Custom("hash".into()),
                    vec![to_hex(&current_root.hash)],
                ),
            ],
        )
        .custom_created_at(Timestamp::from(20))
        .to_event(&keys)
        .unwrap();
        relay.ingest_trusted_event(current_event).await.unwrap();

        let state = AppState {
            nostr_relay: Some(relay),
            ..test_app_state(store, Vec::new())
        };
        let npub = keys.public_key().to_bech32().unwrap();
        put_cached_tree_root(
            &state,
            tree_root_cache_key(&npub, tree_name, None),
            current_root.clone(),
            "cache",
            None,
        );

        let response = htree_npub_impl(
            State(state),
            npub,
            tree_name.to_string(),
            Some("thumbnail".to_string()),
            Query(HashMap::new()),
            axum::http::HeaderMap::new(),
            axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43123))),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn cache_tree_root_public_chk_uses_plain_mutable_cache_key() {
        let temp_dir = TempDir::new().unwrap();
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
        let state = test_app_state(store, Vec::new());

        let response = cache_tree_root(
            State(state.clone()),
            Json(CacheTreeRootRequest {
                npub: "npub1example".to_string(),
                tree_name: "video".to_string(),
                hash: "be8f5da537f62d02d3ff113d213a7058116f790a8d0e158c2766543deda10e35"
                    .to_string(),
                key: Some(
                    "34e24fadaddc60da2e761501aae44c1c2b6b8706b73dff736eb0fc7d803133bb".to_string(),
                ),
                visibility: Some("public".to_string()),
            }),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let cached = get_cached_tree_root(&state, "npub1example/video").expect("cached cid");
        assert_eq!(
            to_hex(&cached.cid.hash),
            "be8f5da537f62d02d3ff113d213a7058116f790a8d0e158c2766543deda10e35"
        );
        assert_eq!(
            cached.cid.key.map(|key| to_hex(&key)).as_deref(),
            Some("34e24fadaddc60da2e761501aae44c1c2b6b8706b73dff736eb0fc7d803133bb")
        );
        assert!(get_cached_tree_root(
            &state,
            "npub1example/video?k=34e24fadaddc60da2e761501aae44c1c2b6b8706b73dff736eb0fc7d803133bb"
        )
        .is_none());
    }

    #[tokio::test]
    async fn clear_tree_root_cache_removes_seeded_mutable_root_cache() {
        let temp_dir = TempDir::new().unwrap();
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
        let state = test_app_state(store, Vec::new());

        let seed_response = cache_tree_root(
            State(state.clone()),
            Json(CacheTreeRootRequest {
                npub: "npub1example".to_string(),
                tree_name: "video".to_string(),
                hash: "988db3f24dc222715f1c1e1fa5876690d3147122243d72d85fd44283867cd61a"
                    .to_string(),
                key: None,
                visibility: Some("public".to_string()),
            }),
        )
        .await
        .into_response();
        assert_eq!(seed_response.status(), StatusCode::OK);
        assert!(get_cached_tree_root(&state, "npub1example/video").is_some());

        let clear_response = clear_tree_root_cache(
            State(state.clone()),
            Json(ClearTreeRootCacheRequest {
                npub: "npub1example".to_string(),
                tree_name: "video".to_string(),
                key: None,
                visibility: Some("public".to_string()),
            }),
        )
        .await
        .into_response();

        assert_eq!(clear_response.status(), StatusCode::OK);
        assert!(get_cached_tree_root(&state, "npub1example/video").is_none());
    }

    #[tokio::test]
    async fn cached_root_preserves_encrypted_key_metadata_for_followup_resolves() {
        let temp_dir = TempDir::new().unwrap();
        let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
        let state = test_app_state(store, Vec::new());
        let hash_hex = "cd".repeat(32);
        let encrypted_key = "ef".repeat(32);
        let cid = Cid::parse(&hash_hex).expect("valid cid");
        let root_event = PeerRootEvent {
            hash: hash_hex.clone(),
            key: None,
            encrypted_key: Some(encrypted_key.clone()),
            self_encrypted_key: None,
            event_id: "event-1".to_string(),
            created_at: 1,
            peer_id: "peer-a".to_string(),
        };

        put_cached_tree_root(
            &state,
            tree_root_cache_key("npub1example", "video", None),
            cid.clone(),
            "webrtc",
            Some(root_event.clone()),
        );

        let resolved = resolve_root_offline(&state, "npub1example", "video", None)
            .await
            .expect("cached root should resolve");

        assert_eq!(resolved.source, "cache");
        assert_eq!(resolved.cid, cid);
        assert_eq!(
            resolved
                .root_event
                .as_ref()
                .and_then(|root| root.encrypted_key.as_deref()),
            Some(encrypted_key.as_str())
        );
    }
}
