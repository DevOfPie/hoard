//! Who is hosting a shared save (`/v1/saves/{id}/lease`).
//!
//! One row per save (`save_leases`, migration 0025), rewritten on acquire. A
//! lease is live while `released_at IS NULL` and `renewed_at` is within
//! [`LEASE_TTL_SECS`]; both are computed in SQL on every read, so a host whose
//! machine died stops holding the save when its heartbeat stops (the `online`
//! reasoning of 0019). The holder renews every 30 s (HRD-D-0003).
//!
//! The lease gates the push: `cas::init`, `cas::commit` and `snapshots::create`
//! call [`require_host`] on a save in a group namespace, the owner included,
//! since a world is played on one machine at a time and the one playing it is
//! the one pushing. Acquire carries the caller's head and is refused while the
//! save has moved past it, which is where "a viewer upgrades to host only if
//! the head has not moved" cannot be raced.
//!
//! Every change of holder, liveness or `pushed_since` goes out as an
//! `event: lease` frame to the owner and every member (`events.rs`). A renew
//! changes none of those and stays quiet.

use axum::{
    extract::{Extension, Path, State},
    http::{HeaderMap, StatusCode},
    response::Json,
};
use hoard_core::wire::{Lease, LeaseAcquireRequest, LeaseEvent, LeaseOut};
use sqlx::{Sqlite, SqlitePool};
use std::sync::Arc;
use tracing::info;

use crate::auth::AuthUser;
use crate::routes::access::{save_access, SaveAccess};
use crate::routes::events::Frame;
use crate::routes::health::ServerState;
use crate::routes::snapshots::{err, internal_logged};
use crate::routes::{repair_ts, repair_username};

type ApiError = (StatusCode, Json<serde_json::Value>);

/// Seconds a lease stays live after its last renewal (HRD-D-0003).
pub const LEASE_TTL_SECS: i64 = 300;

/// One `save_leases` row joined to its holder, as read by [`live_lease`].
#[derive(Debug, Clone)]
pub struct LeaseRow {
    pub save_id: String,
    pub holder_user_id: String,
    pub holder_username: String,
    pub holder_device_fp: Option<String>,
    pub acquired_at: String,
    pub renewed_at: String,
    pub base_version: i64,
    pub pushed_since: bool,
}

impl LeaseRow {
    pub fn to_wire(&self) -> Lease {
        Lease {
            save_id: self.save_id.clone(),
            holder_user_id: self.holder_user_id.clone(),
            holder_username: repair_username(&self.holder_username),
            holder_device_fp: self.holder_device_fp.clone(),
            acquired_at: repair_ts(&self.acquired_at),
            renewed_at: repair_ts(&self.renewed_at),
            base_version: self.base_version,
            pushed_since: self.pushed_since,
            live: true,
        }
    }

    fn event(&self) -> LeaseEvent {
        LeaseEvent {
            save_id: self.save_id.clone(),
            holder_user_id: Some(self.holder_user_id.clone()),
            live: true,
            pushed_since: self.pushed_since,
        }
    }
}

/// The live lease on `save_id`, if any. Expired and released rows read as
/// `None`; liveness is decided by the database's clock, not the caller's.
pub async fn live_lease<'e, E>(ex: E, save_id: &str) -> Result<Option<LeaseRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    let ttl = -LEASE_TTL_SECS;
    let row = sqlx::query!(
        r#"SELECT l.save_id, l.holder_user_id, u.username AS holder_username,
                  l.holder_device_fp, l.acquired_at, l.renewed_at, l.base_version,
                  l.pushed_since
           FROM save_leases l JOIN users u ON u.id = l.holder_user_id
           WHERE l.save_id = ? AND l.released_at IS NULL
             AND l.renewed_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ? || ' seconds')"#,
        save_id,
        ttl
    )
    .fetch_optional(ex)
    .await?;
    Ok(row.map(|r| LeaseRow {
        save_id: r.save_id,
        holder_user_id: r.holder_user_id,
        holder_username: r.holder_username,
        holder_device_fp: r.holder_device_fp,
        acquired_at: r.acquired_at,
        renewed_at: r.renewed_at,
        base_version: r.base_version,
        pushed_since: r.pushed_since != 0,
    }))
}

fn conflict(code: &str, msg: &str) -> ApiError {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({ "error": msg, "code": code })),
    )
}

fn conflict_with(code: &str, msg: &str, extra: serde_json::Value) -> ApiError {
    let mut body = serde_json::json!({ "error": msg, "code": code });
    if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
        b.extend(e.clone());
    }
    (StatusCode::CONFLICT, Json(body))
}

fn not_holder() -> ApiError {
    conflict("not_holder", "you do not hold a live lease on this save")
}

/// The caller's access to a save that is shared. A stranger gets the 404 an
/// unknown id gets; a save with no group answers `409 not_shared`, since a
/// lease on a private save would gate nothing.
pub async fn require_shared_access(
    pool: &SqlitePool,
    save_id: &str,
    user_id: &str,
) -> Result<SaveAccess, ApiError> {
    let access = save_access(pool, save_id, user_id)
        .await
        .map_err(|e| internal_logged("access lookup", e))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "save not found"))?;
    if access.group_id.is_none() {
        return Err(conflict("not_shared", "the save is not shared"));
    }
    Ok(access)
}

/// The push gate: the caller must hold the live lease on a shared save. The
/// 409 carries the current holder's lease when there is one, so the client can
/// say who is hosting instead of "conflict".
pub async fn require_host<'e, E>(ex: E, save_id: &str, user_id: &str) -> Result<(), ApiError>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    let lease = live_lease(ex, save_id)
        .await
        .map_err(|e| internal_logged("lease lookup", e))?;
    match lease {
        Some(l) if l.holder_user_id == user_id => Ok(()),
        Some(l) => Err(conflict_with(
            "lease_required",
            "another member is hosting this save",
            serde_json::json!({ "lease": l.to_wire() }),
        )),
        None => Err(conflict(
            "lease_required",
            "a shared save is pushed by its host: acquire the lease first",
        )),
    }
}

/// Record that the holder pushed: from here on the lease cannot be forced.
/// Called inside the commit transaction; a row that is not the caller's is left
/// alone, [`require_host`] having already ruled on it.
pub async fn mark_pushed<'e, E>(ex: E, save_id: &str, user_id: &str) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    sqlx::query!(
        "UPDATE save_leases SET pushed_since = 1 WHERE save_id = ? AND holder_user_id = ?",
        save_id,
        user_id
    )
    .execute(ex)
    .await?;
    Ok(())
}

/// After a commit on a shared save: the holder's lease now covers a push.
pub async fn announce_push(state: &ServerState, save_id: &str, user_id: &str) {
    publish(
        state,
        save_id,
        LeaseEvent {
            save_id: save_id.to_string(),
            holder_user_id: Some(user_id.to_string()),
            live: true,
            pushed_since: true,
        },
    )
    .await;
}

fn device_fp(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-hoard-device-fp")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

async fn publish(state: &ServerState, save_id: &str, event: LeaseEvent) {
    if let Err(e) = state
        .events
        .publish_save(&state.pool, save_id, Frame::Lease(event))
        .await
    {
        tracing::warn!(error = %e, save_id, "lease event: recipients not resolved");
    }
}

// ---- GET /v1/saves/:save_id/lease

pub async fn get(
    State(state): State<Arc<ServerState>>,
    Extension(user): Extension<AuthUser>,
    Path(save_id): Path<String>,
) -> Result<Json<LeaseOut>, ApiError> {
    let user_id = user.user_id.to_string();
    require_shared_access(&state.pool, &save_id, &user_id).await?;
    let lease = live_lease(&state.pool, &save_id)
        .await
        .map_err(|e| internal_logged("lease lookup", e))?;
    Ok(Json(LeaseOut {
        lease: lease.map(|l| l.to_wire()),
    }))
}

// ---- POST /v1/saves/:save_id/lease/acquire

/// `409 held` while another member's lease is live, `409 stale` when the save's
/// head is past `base_version`. Re-acquiring one's own live lease refreshes it
/// and keeps `pushed_since`; anything else writes a fresh row.
pub async fn acquire(
    State(state): State<Arc<ServerState>>,
    Extension(user): Extension<AuthUser>,
    Path(save_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<LeaseAcquireRequest>,
) -> Result<Json<Lease>, ApiError> {
    let user_id = user.user_id.to_string();
    require_shared_access(&state.pool, &save_id, &user_id).await?;
    let fp = device_fp(&headers);

    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|e| internal_logged("opening the lease transaction", e))?;
    let current = live_lease(&mut *tx, &save_id)
        .await
        .map_err(|e| internal_logged("lease lookup", e))?;
    if let Some(l) = current.as_ref().filter(|l| l.holder_user_id != user_id) {
        return Err(conflict_with(
            "held",
            "another member is hosting this save",
            serde_json::json!({ "lease": l.to_wire() }),
        ));
    }
    let head = sqlx::query_scalar!("SELECT latest_version_num FROM saves WHERE id = ?", save_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| internal_logged("reading the save's latest version", e))?;
    if head > body.base_version {
        return Err(conflict_with(
            "stale",
            "the save moved past your version: pull before hosting",
            serde_json::json!({ "head_version": head, "base_version": body.base_version }),
        ));
    }

    if current.is_some() {
        sqlx::query!(
            "UPDATE save_leases
             SET renewed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                 holder_device_fp = ?, base_version = ?
             WHERE save_id = ?",
            fp,
            body.base_version,
            save_id
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| internal_logged("refreshing the lease", e))?;
    } else {
        sqlx::query!(
            "INSERT OR REPLACE INTO save_leases
                 (save_id, holder_user_id, holder_device_fp, acquired_at, renewed_at,
                  base_version, pushed_since, released_at)
             VALUES (?, ?, ?, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                     strftime('%Y-%m-%dT%H:%M:%SZ', 'now'), ?, 0, NULL)",
            save_id,
            user_id,
            fp,
            body.base_version
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| internal_logged("recording the lease", e))?;
    }
    let lease = live_lease(&mut *tx, &save_id)
        .await
        .map_err(|e| internal_logged("lease lookup", e))?
        .ok_or_else(|| internal_logged("lease lookup", "the row just written is not live"))?;
    tx.commit()
        .await
        .map_err(|e| internal_logged("committing the lease", e))?;

    if current.is_none() {
        info!(user = %user.username, save_id = %save_id, base_version = body.base_version, "lease acquired");
        publish(&state, &save_id, lease.event()).await;
    }
    Ok(Json(lease.to_wire()))
}

// ---- POST /v1/saves/:save_id/lease/renew

/// The heartbeat. An expired lease is not renewed: it has to be acquired again,
/// which re-runs the head check.
pub async fn renew(
    State(state): State<Arc<ServerState>>,
    Extension(user): Extension<AuthUser>,
    Path(save_id): Path<String>,
) -> Result<Json<Lease>, ApiError> {
    let user_id = user.user_id.to_string();
    require_shared_access(&state.pool, &save_id, &user_id).await?;
    let ttl = -LEASE_TTL_SECS;
    let done = sqlx::query!(
        "UPDATE save_leases SET renewed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
         WHERE save_id = ? AND holder_user_id = ? AND released_at IS NULL
           AND renewed_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ? || ' seconds')",
        save_id,
        user_id,
        ttl
    )
    .execute(&state.pool)
    .await
    .map_err(|e| internal_logged("renewing the lease", e))?;
    if done.rows_affected() == 0 {
        return Err(not_holder());
    }
    live_lease(&state.pool, &save_id)
        .await
        .map_err(|e| internal_logged("lease lookup", e))?
        .map(|l| Json(l.to_wire()))
        .ok_or_else(not_holder)
}

// ---- POST /v1/saves/:save_id/lease/release

pub async fn release(
    State(state): State<Arc<ServerState>>,
    Extension(user): Extension<AuthUser>,
    Path(save_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let user_id = user.user_id.to_string();
    require_shared_access(&state.pool, &save_id, &user_id).await?;
    let lease = live_lease(&state.pool, &save_id)
        .await
        .map_err(|e| internal_logged("lease lookup", e))?
        .filter(|l| l.holder_user_id == user_id)
        .ok_or_else(not_holder)?;
    end_lease(&state, &lease).await?;
    info!(user = %user.username, save_id = %save_id, "lease released");
    Ok(StatusCode::NO_CONTENT)
}

// ---- POST /v1/saves/:save_id/lease/force

/// A member takes a live lease off its holder, while the holder has pushed
/// nothing under it: a takeover then discards no play that reached the server.
/// The caller acquires separately afterwards, so the head check still runs.
pub async fn force(
    State(state): State<Arc<ServerState>>,
    Extension(user): Extension<AuthUser>,
    Path(save_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let user_id = user.user_id.to_string();
    require_shared_access(&state.pool, &save_id, &user_id).await?;
    let lease = live_lease(&state.pool, &save_id)
        .await
        .map_err(|e| internal_logged("lease lookup", e))?
        .ok_or_else(|| conflict("not_held", "nobody is hosting this save"))?;
    if lease.pushed_since {
        return Err(conflict_with(
            "pushed",
            "the host has pushed under this lease; it ends when they release it",
            serde_json::json!({ "lease": lease.to_wire() }),
        ));
    }
    end_lease(&state, &lease).await?;
    info!(user = %user.username, save_id = %save_id, holder = %lease.holder_user_id, "lease forced");
    Ok(StatusCode::NO_CONTENT)
}

/// Set `released_at` on the live row and tell everyone the save has no host.
async fn end_lease(state: &ServerState, lease: &LeaseRow) -> Result<(), ApiError> {
    sqlx::query!(
        "UPDATE save_leases SET released_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
         WHERE save_id = ? AND holder_user_id = ? AND released_at IS NULL",
        lease.save_id,
        lease.holder_user_id
    )
    .execute(&state.pool)
    .await
    .map_err(|e| internal_logged("releasing the lease", e))?;
    publish(
        state,
        &lease.save_id,
        LeaseEvent {
            save_id: lease.save_id.clone(),
            holder_user_id: None,
            live: false,
            pushed_since: lease.pushed_since,
        },
    )
    .await;
    Ok(())
}
