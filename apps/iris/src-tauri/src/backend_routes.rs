use axum::{
    body::{Body, Bytes},
    extract::{Path, State},
    http::{header, Response, StatusCode},
    routing::get,
    Router,
};
use hashtree_cli::server::AppState;
use hashtree_core::{from_hex, sha256, to_hex};

fn parse_blob_hash(hash: &str) -> Result<[u8; 32], Response<Body>> {
    from_hex(hash).map_err(|_| {
        Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Body::from("invalid blob hash"))
            .unwrap()
    })
}

fn empty_response(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .unwrap()
}

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/__iris/store/:hash",
        get(get_blob)
            .head(head_blob)
            .put(put_blob)
            .delete(delete_blob),
    )
}

pub async fn get_blob(State(state): State<AppState>, Path(hash): Path<String>) -> Response<Body> {
    let hash = match parse_blob_hash(&hash) {
        Ok(hash) => hash,
        Err(response) => return response,
    };

    match state.store.get_blob(&hash) {
        Ok(Some(data)) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::CONTENT_LENGTH, data.len())
            .body(Body::from(data))
            .unwrap(),
        Ok(None) => empty_response(StatusCode::NOT_FOUND),
        Err(err) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Body::from(err.to_string()))
            .unwrap(),
    }
}

pub async fn head_blob(State(state): State<AppState>, Path(hash): Path<String>) -> Response<Body> {
    let hash = match parse_blob_hash(&hash) {
        Ok(hash) => hash,
        Err(response) => return response,
    };

    match state.store.blob_exists(&hash) {
        Ok(true) => empty_response(StatusCode::OK),
        Ok(false) => empty_response(StatusCode::NOT_FOUND),
        Err(err) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Body::from(err.to_string()))
            .unwrap(),
    }
}

pub async fn put_blob(
    State(state): State<AppState>,
    Path(hash): Path<String>,
    body: Bytes,
) -> Response<Body> {
    let hash = match parse_blob_hash(&hash) {
        Ok(hash) => hash,
        Err(response) => return response,
    };

    let computed = sha256(&body);
    if computed != hash {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Body::from(format!(
                "hash mismatch: expected {}, got {}",
                to_hex(&hash),
                to_hex(&computed)
            )))
            .unwrap();
    }

    match state.store.router().put_sync(hash, &body) {
        Ok(true) => empty_response(StatusCode::CREATED),
        Ok(false) => empty_response(StatusCode::OK),
        Err(err) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Body::from(err.to_string()))
            .unwrap(),
    }
}

pub async fn delete_blob(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Response<Body> {
    let hash = match parse_blob_hash(&hash) {
        Ok(hash) => hash,
        Err(response) => return response,
    };

    match state.store.router().delete_sync(&hash) {
        Ok(true) => empty_response(StatusCode::NO_CONTENT),
        Ok(false) => empty_response(StatusCode::NOT_FOUND),
        Err(err) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Body::from(err.to_string()))
            .unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashtree_cli::server::HashtreeServer;
    use hashtree_cli::storage::HashtreeStore;
    use std::sync::Arc;
    use tempfile::tempdir;

    async fn spawn_test_server() -> (String, tokio::task::JoinHandle<()>, tempfile::TempDir) {
        let temp_dir = tempdir().unwrap();
        let store = Arc::new(HashtreeStore::new(temp_dir.path()).unwrap());
        let server =
            HashtreeServer::new(store, "127.0.0.1:0".to_string()).with_extra_routes(router());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            server.run_with_listener(listener).await.unwrap();
        });
        (format!("http://127.0.0.1:{port}"), handle, temp_dir)
    }

    #[tokio::test]
    async fn iris_store_round_trip_supports_put_head_get_and_delete() {
        let body = Bytes::from_static(b"hello native iris");
        let hash = sha256(&body);
        let hash_hex = to_hex(&hash);
        let (base_url, handle, _temp_dir) = spawn_test_server().await;

        let client = reqwest::Client::new();
        let put = client
            .put(format!("{base_url}/__iris/store/{hash_hex}"))
            .body(body.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(put.status(), StatusCode::CREATED);

        let head = client
            .head(format!("{base_url}/__iris/store/{hash_hex}"))
            .send()
            .await
            .unwrap();
        assert_eq!(head.status(), StatusCode::OK);

        let get = client
            .get(format!("{base_url}/__iris/store/{hash_hex}"))
            .send()
            .await
            .unwrap();
        assert_eq!(get.status(), StatusCode::OK);
        assert_eq!(get.bytes().await.unwrap().as_ref(), body.as_ref());

        let delete = client
            .delete(format!("{base_url}/__iris/store/{hash_hex}"))
            .send()
            .await
            .unwrap();
        assert_eq!(delete.status(), StatusCode::NO_CONTENT);

        let missing = client
            .head(format!("{base_url}/__iris/store/{hash_hex}"))
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);

        handle.abort();
    }

    #[tokio::test]
    async fn iris_store_put_rejects_hash_mismatch() {
        let expected = sha256(b"expected");
        let hash_hex = to_hex(&expected);
        let (base_url, handle, _temp_dir) = spawn_test_server().await;
        let response = reqwest::Client::new()
            .put(format!("{base_url}/__iris/store/{hash_hex}"))
            .body(Bytes::from_static(b"wrong"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        handle.abort();
    }
}
