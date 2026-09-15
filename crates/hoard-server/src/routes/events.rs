//! Server→app push for self-hosted deployments (Server-Sent Events).
//!
//! The cloud deployment gets near-instant cross-device sync from Supabase
//! Realtime (Postgres logical replication → WebSocket). Self-hosted has no
//! such thing, so historically the only "another device uploaded" path was the
//! agent's reconciliation sweep, up to a cooldown of latency. This module adds
//! the missing push: clients open a long-lived `GET /v1/events` SSE stream and
//! the snapshot-commit path publishes a [`SaveEvent`] the instant a new version
//! lands, so the listening device pulls within ~1s.
//!
//! The fan-out key is the user. A shared save has more than one interested
//! user, so [`EventBus::publish_save`] resolves the recipients (the owner and
//! every member of the save's group) and publishes to each channel; the lease
//! routes send an `event: lease` frame the same way.
//!
//! Reverse-proxy note: SSE needs response buffering disabled. Nginx works with
//! `proxy_buffering off;` + `proxy_set_header X-Accel-Buffering no;` and a high
//! `proxy_read_timeout` on the `/v1/events` location; Caddy streams correctly
//! out of the box. The 25 s keep-alive comment below also keeps idle
//! connections from being reaped.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Extension, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::Stream;
use serde::Serialize;
use tokio::sync::broadcast;
use uuid::Uuid;

use hoard_core::wire::LeaseEvent;
use sqlx::SqlitePool;

use crate::auth::AuthUser;
use crate::routes::health::ServerState;

/// One "save version landed" notification. The client only needs to know which
/// save advanced; the version number lets it short-circuit if it already has
/// that version (e.g. it was this very device's own upload echoed back).
#[derive(Clone, Debug, Serialize)]
pub struct SaveEvent {
    pub save_id: String,
    pub version_num: i64,
}

/// One frame on the stream: which SSE `event:` it goes out as, and its body.
#[derive(Clone, Debug)]
pub enum Frame {
    Save(SaveEvent),
    Lease(LeaseEvent),
}

impl Frame {
    fn event_name(&self) -> &'static str {
        match self {
            Frame::Save(_) => "save",
            Frame::Lease(_) => "lease",
        }
    }

    fn json(&self) -> Result<Event, axum::Error> {
        let ev = Event::default().event(self.event_name());
        match self {
            Frame::Save(e) => ev.json_data(e),
            Frame::Lease(e) => ev.json_data(e),
        }
    }
}

/// Bounded backlog per user channel. A device that falls this far behind gets a
/// `lagged` hint and does a full catch-up pull, so a small buffer is plenty.
const CHANNEL_CAPACITY: usize = 64;

/// Per-user fan-out of [`Frame`]s. A user's `broadcast` channel is created
/// lazily on their first `/v1/events` subscribe; the snapshot-commit path
/// publishes into it. Lives in [`ServerState`]; uses interior mutability so it
/// can sit behind the shared `Arc<ServerState>` without its own lock dance at
/// the call sites. Empty/unused on the cloud deployment.
#[derive(Default)]
pub struct EventBus {
    inner: Mutex<HashMap<Uuid, broadcast::Sender<Frame>>>,
}

impl EventBus {
    /// Subscribe `user` to their event stream, creating the channel on first
    /// use. The returned receiver keeps the channel alive for as long as the
    /// SSE connection holds it.
    pub fn subscribe(&self, user: Uuid) -> broadcast::Receiver<Frame> {
        // A poisoned lock only means another thread panicked mid-insert; the
        // map is still usable, so keep serving instead of panicking the request.
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(user)
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
            .subscribe()
    }

    /// Publish `ev` to every device of `user` currently listening. A cheap
    /// no-op when nobody is connected. `broadcast::Sender::send` errors only
    /// when there are zero live receivers, so that case doubles as our cue to
    /// drop the channel and keep the map from growing without bound.
    pub fn publish(&self, user: Uuid, ev: SaveEvent) {
        self.publish_frame(user, Frame::Save(ev));
    }

    pub fn publish_frame(&self, user: Uuid, frame: Frame) {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tx) = map.get(&user) {
            if tx.send(frame).is_err() {
                map.remove(&user);
            }
        }
    }

    /// Publish `frame` to everyone with a stake in `save_id`: its owner, and
    /// every member of the group it is shared into. An unshared save reaches
    /// its owner alone, which is what [`EventBus::publish`] did for it.
    pub async fn publish_save(
        &self,
        pool: &SqlitePool,
        save_id: &str,
        frame: Frame,
    ) -> Result<(), sqlx::Error> {
        let rows = sqlx::query_scalar!(
            r#"SELECT user_id AS "user_id!: String" FROM saves WHERE id = ?
               UNION
               SELECT gm.user_id FROM shared_saves ss
               JOIN group_members gm ON gm.group_id = ss.group_id
               WHERE ss.save_id = ?"#,
            save_id,
            save_id
        )
        .fetch_all(pool)
        .await?;
        for id in rows {
            if let Ok(user) = Uuid::parse_str(&id) {
                self.publish_frame(user, frame.clone());
            }
        }
        Ok(())
    }
}

/// `GET /v1/events`: long-lived SSE stream of the authenticated user's save
/// changes. Emits `event: save` frames carrying the JSON [`SaveEvent`],
/// `event: lease` frames carrying a [`LeaseEvent`], and an `event: lagged`
/// hint if the client fell behind (it should respond with a full catch-up
/// pull). A 25 s keep-alive comment keeps idle proxies from cutting the
/// connection.
pub async fn stream(
    State(state): State<Arc<ServerState>>,
    Extension(user): Extension<AuthUser>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.events.subscribe(user.user_id);
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Ok(frame) => {
                let frame = frame
                    .json()
                    .unwrap_or_else(|_| Event::default().comment("encode error"));
                Some((Ok(frame), rx))
            }
            // Buffer overflow: we dropped some events. Tell the client to
            // reconcile from scratch rather than trust the stream.
            Err(broadcast::error::RecvError::Lagged(_)) => {
                Some((Ok(Event::default().event("lagged").data("")), rx))
            }
            // Sender gone (shouldn't happen while ServerState is alive):
            // end the stream so the client reconnects.
            Err(broadcast::error::RecvError::Closed) => None,
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(25)).text(""))
}
