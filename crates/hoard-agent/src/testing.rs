//! Test support shared with the crates above this one (`hoardd`), behind the
//! `test-support` feature so no production build carries it.

use std::sync::{Arc, Mutex};

/// The request paths a [`canned_github`] server has been asked, in order.
pub type Asked = Arc<Mutex<Vec<String>>>;

/// A GitHub stand-in on `127.0.0.1`: each `(path, json)` pair answers a GET for
/// exactly that path and query with `200` and the JSON; anything else is a
/// `404`. Returns the root to hand to the discovery functions as their `api`,
/// and the log of what was asked.
///
/// One request per connection (`Connection: close`), so keep-alive never has to
/// be implemented. The server lives as long as the test's runtime.
pub async fn canned_github(routes: Vec<(&'static str, String)>) -> (String, Asked) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let root = format!("http://{}", listener.local_addr().expect("addr"));
    let asked: Asked = Default::default();
    let log = asked.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            // A GET has no body: the head is the whole request.
            loop {
                let n = sock.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let head = String::from_utf8_lossy(&buf);
            let path = head
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or_default()
                .to_string();
            log.lock().unwrap().push(path.clone());
            let (status, body) = match routes.iter().find(|(p, _)| *p == path) {
                Some((_, body)) => ("200 OK", body.clone()),
                None => ("404 Not Found", r#"{"message":"Not Found"}"#.to_string()),
            };
            let reply = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(reply.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    (root, asked)
}

/// A release as GitHub's API lists it, with one asset so asset plumbing has
/// something to carry.
pub fn release_json(tag: &str, prerelease: bool, draft: bool) -> serde_json::Value {
    serde_json::json!({
        "tag_name": tag,
        "prerelease": prerelease,
        "draft": draft,
        "assets": [{
            "name": format!("hoard_{}_amd64.deb", tag.trim_start_matches('v')),
            "browser_download_url": format!("https://example.invalid/{tag}.deb"),
        }],
    })
}

/// The path the stable channel asks.
pub const LATEST_PATH: &str = "/repos/DevOfPie/hoard/releases/latest";
/// The path the pre-release channel asks.
pub const LIST_PATH: &str = "/repos/DevOfPie/hoard/releases?per_page=10";
