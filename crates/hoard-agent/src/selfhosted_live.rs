//! Live lease frames from a self-hosted server (`GET /v1/events`, HRD-F-0008).
//!
//! The twin of `cloud_live` for someone's own server: one SSE connection per
//! engine that feeds `event: lease` frames to the engine as
//! `AgentHandle::set_lease`, so a member learns within a second that a world
//! got a host, lost it, or was taken over. `save` frames are the desktop's
//! (its own loop still force-restores off them) and `lagged` needs no action
//! here: the next renew or acquire re-reads the truth.
//!
//! Gated on the server advertising groups; Cloud never gets this task. The
//! caller's user id comes from `whoami` once at start, since a frame names
//! holders by id.

use std::time::Duration;

use futures::StreamExt;
use hoard_core::kernel::LeaseObs;
use hoard_core::wire::LeaseEvent;
use tokio::task::JoinHandle;
use tokio::time::sleep;

use crate::agent::AgentHandle;
use crate::api::ApiClient;

const BACKOFF_MIN_SECS: u64 = 2;
const BACKOFF_MAX_SECS: u64 = 60;
/// The server sends a keep-alive comment well inside this; nothing at all for
/// this long means a dead socket.
const READ_IDLE_SECS: u64 = 60;

/// Starts the stream. The task lives as long as the engine that spawned it;
/// hoardd aborts it with the rest of the engine's auxiliaries.
pub fn spawn(api: ApiClient, agent: AgentHandle) -> JoinHandle<()> {
    tokio::spawn(run(api, agent))
}

async fn run(api: ApiClient, agent: AgentHandle) {
    let mut backoff = BACKOFF_MIN_SECS;
    // Only a definite answer settles the gate: a failed probe is retried, a
    // server without groups ends the task.
    loop {
        let _ = api.server_mode().await;
        match api.probed_supports_groups() {
            Some(true) => break,
            Some(false) => {
                tracing::debug!("selfhosted-live: the server has no groups; not listening");
                return;
            }
            None => {
                sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(BACKOFF_MAX_SECS);
            }
        }
    }
    let me = loop {
        match api.whoami().await {
            Ok(w) => break w.user_id,
            Err(e) => {
                tracing::debug!(error = %e, "selfhosted-live: whoami failed, retrying");
                sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(BACKOFF_MAX_SECS);
            }
        }
    };

    let mut backoff = BACKOFF_MIN_SECS;
    loop {
        match connect_once(&api, &agent, &me).await {
            Ok(()) => backoff = BACKOFF_MIN_SECS,
            Err(e) => {
                tracing::debug!(error = %format!("{e:#}"), "selfhosted-live: stream dropped, retrying");
            }
        }
        sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(BACKOFF_MAX_SECS);
    }
}

/// One connection: open the stream and pump frames until the socket dies or
/// the keep-alive gap is exceeded.
async fn connect_once(api: &ApiClient, agent: &AgentHandle, me: &str) -> anyhow::Result<()> {
    let resp = api.events_stream().await?;
    tracing::debug!("selfhosted-live: stream open");
    let mut stream = resp.bytes_stream();
    let mut parser = SseParser::default();
    loop {
        let next = tokio::time::timeout(Duration::from_secs(READ_IDLE_SECS), stream.next()).await;
        let chunk = match next {
            Err(_) => anyhow::bail!("idle timeout: no keep-alive, socket presumed dead"),
            Ok(None) => return Ok(()),
            Ok(Some(Err(e))) => return Err(e.into()),
            Ok(Some(Ok(c))) => c,
        };
        for (ev_type, data) in parser.push(&String::from_utf8_lossy(&chunk)) {
            if ev_type == "lease" {
                handle_lease(api, agent, me, &data).await;
            }
        }
    }
}

/// SSE framing: `event:` sets the type, `data:` lines accumulate, a blank line
/// dispatches. `:` comments (the keep-alives) and unknown fields fall through.
#[derive(Default)]
struct SseParser {
    buf: String,
    ev_type: String,
    ev_data: String,
}

impl SseParser {
    fn push(&mut self, chunk: &str) -> Vec<(String, String)> {
        self.buf.push_str(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.find('\n') {
            let line = self.buf[..pos].trim_end_matches('\r').to_string();
            self.buf.drain(..=pos);
            if line.is_empty() {
                if !self.ev_type.is_empty() || !self.ev_data.is_empty() {
                    out.push((
                        std::mem::take(&mut self.ev_type),
                        std::mem::take(&mut self.ev_data),
                    ));
                }
                self.ev_type.clear();
                self.ev_data.clear();
            } else if let Some(rest) = line.strip_prefix("event:") {
                self.ev_type = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("data:") {
                if !self.ev_data.is_empty() {
                    self.ev_data.push('\n');
                }
                self.ev_data
                    .push_str(rest.strip_prefix(' ').unwrap_or(rest));
            }
        }
        out
    }
}

/// What a frame says about the lease from this machine's point of view.
fn classify(ev: &LeaseEvent, me: &str) -> LeaseObs {
    match ev.holder_user_id.as_deref() {
        Some(_) if !ev.live => LeaseObs::Free,
        Some(h) if h == me => LeaseObs::Mine,
        Some(_) => LeaseObs::Other,
        None => LeaseObs::Free,
    }
}

async fn handle_lease(api: &ApiClient, agent: &AgentHandle, me: &str, data: &str) {
    let Ok(ev) = serde_json::from_str::<LeaseEvent>(data) else {
        tracing::debug!(data = %data, "selfhosted-live: unparseable lease frame");
        return;
    };
    let obs = classify(&ev, me);
    // The frame carries the holder's id, the events want their name: one GET,
    // only when there is somebody to name.
    let holder = match obs {
        LeaseObs::Other => api
            .get_lease(&ev.save_id)
            .await
            .ok()
            .flatten()
            .map(|l| l.holder_username.as_str().to_string()),
        _ => None,
    };
    tracing::debug!(save_id = %ev.save_id, ?obs, holder = holder.as_deref().unwrap_or("-"), "selfhosted-live: lease frame");
    if let Err(e) = agent.set_lease(ev.save_id, obs, holder).await {
        tracing::debug!(error = %e, "selfhosted-live: the engine is gone");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(holder: Option<&str>, live: bool) -> LeaseEvent {
        LeaseEvent {
            save_id: "w1".into(),
            holder_user_id: holder.map(String::from),
            live,
            pushed_since: false,
        }
    }

    #[test]
    fn a_frame_reads_as_mine_other_or_free() {
        assert_eq!(classify(&frame(Some("me"), true), "me"), LeaseObs::Mine);
        assert_eq!(classify(&frame(Some("bob"), true), "me"), LeaseObs::Other);
        assert_eq!(classify(&frame(None, false), "me"), LeaseObs::Free);
        // A holder the server no longer counts as live holds nothing.
        assert_eq!(classify(&frame(Some("bob"), false), "me"), LeaseObs::Free);
    }

    /// Frames split across chunks, comments between them, CRLF endings.
    #[test]
    fn the_parser_reassembles_frames_across_chunks() {
        let mut p = SseParser::default();
        assert!(p.push(": keep-alive\n\nevent: lea").is_empty());
        let got = p.push("se\r\ndata: {\"save_id\":\"w1\"}\r\n\r\nevent: lagged\ndata: \n\n");
        assert_eq!(
            got,
            vec![
                ("lease".to_string(), "{\"save_id\":\"w1\"}".to_string()),
                ("lagged".to_string(), String::new()),
            ]
        );
        // A multi-line `data:` joins with newlines.
        let got = p.push("data: a\ndata: b\n\n");
        assert_eq!(got, vec![(String::new(), "a\nb".to_string())]);
    }
}
