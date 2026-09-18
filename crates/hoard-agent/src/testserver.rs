//! A canned HTTP server for tests that need a real download: each route is a
//! method and a path (the query is ignored) with the status and body it
//! answers, as often as asked. Anything else is a 404. The routes are built
//! once the server's URL is known, so a body can point back at it (a
//! presigned URL, say).

use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One canned answer: `("GET /v1/...", status, body)`.
pub(crate) type Route = (String, u16, Vec<u8>);

/// Every request line the server saw, in order.
pub(crate) type Seen = Arc<Mutex<Vec<String>>>;

/// Serves `routes(url)` on a fresh local port. Returns the URL and the
/// request lines seen.
pub(crate) async fn serve(routes: impl FnOnce(&str) -> Vec<Route>) -> (String, Seen) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let routes = routes(&url);
    let seen: Seen = Default::default();
    let log = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let head_end = loop {
                let n = sock.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break None;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break Some(i + 4);
                }
            };
            let Some(head_end) = head_end else {
                continue;
            };
            let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
            let len = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            while buf.len() < head_end + len {
                let n = sock.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let line = head.lines().next().unwrap_or_default().to_string();
            log.lock().unwrap().push(line.clone());
            let mut parts = line.split(' ');
            let method = parts.next().unwrap_or_default();
            let path = parts
                .next()
                .unwrap_or_default()
                .split('?')
                .next()
                .unwrap_or_default();
            let key = format!("{method} {path}");
            let (status, body) = routes
                .iter()
                .find(|(k, _, _)| *k == key)
                .map(|(_, s, b)| (*s, b.clone()))
                .unwrap_or((404, br#"{"error":"not found"}"#.to_vec()));
            let head = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(&body).await;
            let _ = sock.shutdown().await;
        }
    });
    (url, seen)
}

/// A route answering 200 with `body`.
pub(crate) fn ok(key: impl Into<String>, body: impl Into<Vec<u8>>) -> Route {
    (key.into(), 200, body.into())
}

pub(crate) fn sha_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// `files` as a zstd-compressed tar, the self-hosted download's shape.
pub(crate) async fn tar_zst(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut tar = tokio_tar::Builder::new(Vec::new());
    for (rel, bytes) in files {
        let mut header = tokio_tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(1_700_000_000);
        header.set_cksum();
        tar.append_data(&mut header, rel, *bytes).await.unwrap();
    }
    let raw = tar.into_inner().await.unwrap();
    let mut enc = async_compression::tokio::write::ZstdEncoder::new(Vec::new());
    enc.write_all(&raw).await.unwrap();
    enc.shutdown().await.unwrap();
    enc.into_inner()
}

/// The routes of a self-hosted server holding `save_id` at `version`, made
/// of `files`: the health probe, the save's row, the version's listing and
/// its archive.
pub(crate) async fn selfhosted_version(
    save_id: &str,
    version: i64,
    files: &[(&str, &[u8])],
) -> Vec<Route> {
    let listing: Vec<serde_json::Value> = files
        .iter()
        .map(|(rel, bytes)| {
            serde_json::json!({
                "relative_path": rel,
                "size_bytes": bytes.len(),
                "sha256": sha_hex(bytes),
            })
        })
        .collect();
    let total: usize = files.iter().map(|(_, b)| b.len()).sum();
    vec![
        ok(
            "GET /v1/health",
            r#"{"status":"ok","version":"test","groups":true}"#,
        ),
        ok(
            format!("GET /v1/saves/{save_id}"),
            serde_json::json!({
                // The row's id is a UUID on the wire; the path is what the
                // client addresses it by.
                "id": "3f2504e0-4f89-41d3-9a0c-0305e82c3301",
                "game_slug": "valheim",
                "label": "main",
                "latest_version_num": version,
                "created_at": "2026-06-01T10:00:00Z",
                "updated_at": "2026-06-01T10:00:00Z",
            })
            .to_string(),
        ),
        ok(
            format!("GET /v1/saves/{save_id}/snapshots/{version}"),
            serde_json::json!({
                "id": "snap",
                "version_num": version,
                "total_size_bytes": total,
                "file_count": files.len(),
                "is_pinned": false,
                "created_at": "2026-06-01T10:00:00Z",
                "files": listing,
            })
            .to_string(),
        ),
        ok(
            format!("GET /v1/saves/{save_id}/snapshots/{version}/download"),
            tar_zst(files).await,
        ),
    ]
}
