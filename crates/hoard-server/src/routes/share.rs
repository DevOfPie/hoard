//! Share a save into a group, and take it back (`/v1/saves/{id}/share`).
//!
//! A shared save's bytes live in the group's namespace (`namespace.rs`), so
//! sharing is a move: every blob and chunk the save's versions reference, live
//! or trashed, is copied to the group's key, referenced there, released from
//! the owner's row, and the quota follows to the group's owner. Unsharing is
//! the same move the other way.
//!
//! The disk step comes first and is idempotent: a copy is skipped when the
//! destination key exists, so a failure halfway leaves the database untouched
//! and the next attempt finds its copies waiting. Then one transaction moves the
//! rows, the quota and the `shared_saves` row together, and only after it
//! commits are the source objects nothing references any more deleted.

use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    response::Json,
};
use hoard_core::wire::{Save, ShareSaveRequest};
use sqlx::SqlitePool;
use std::sync::Arc;
use tracing::{info, warn};
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::namespace::{self, Namespace};
use crate::routes::access::{save_access, Role, SaveAccess};
use crate::routes::health::ServerState;
use crate::routes::snapshots::{err, internal, internal_logged};

type ApiError = (StatusCode, Json<serde_json::Value>);

/// Seconds a lease stays live after its last renewal (HRD-D-0003).
const LEASE_TTL_SECS: i64 = 300;

fn conflict(code: &str, msg: &str) -> ApiError {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({ "error": msg, "code": code })),
    )
}

async fn owner_access(
    pool: &SqlitePool,
    save_id: &str,
    user_id: &str,
) -> Result<SaveAccess, ApiError> {
    let access = save_access(pool, save_id, user_id)
        .await
        .map_err(|e| internal_logged("access lookup", e))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "save not found"))?;
    if access.role != Role::Owner {
        return Err(err(
            StatusCode::FORBIDDEN,
            "only the save owner can share or unshare it",
        ));
    }
    Ok(access)
}

// ---- POST /v1/saves/:save_id/share

/// The caller must own the save and belong to the group (as its owner or a
/// member). A group the caller is not in answers 404, like an unknown one.
pub async fn share(
    State(state): State<Arc<ServerState>>,
    Extension(user): Extension<AuthUser>,
    Path(save_id): Path<String>,
    Json(body): Json<ShareSaveRequest>,
) -> Result<Json<Save>, ApiError> {
    let user_id = user.user_id.to_string();
    let access = owner_access(&state.pool, &save_id, &user_id).await?;
    if access.group_id.is_some() {
        return Err(conflict("already_shared", "the save is already shared"));
    }
    let member = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n: i64" FROM group_members WHERE group_id = ? AND user_id = ?"#,
        body.group_id,
        user_id
    )
    .fetch_one(&state.pool)
    .await
    .map_err(|e| internal_logged("membership lookup", e))?;
    if member == 0 {
        return Err(err(StatusCode::NOT_FOUND, "group not found"));
    }

    let from = Namespace::User(access.owner_user_id.clone());
    let to = Namespace::Group(body.group_id.clone());
    move_content(&state, &user_id, &save_id, &from, &to, Some(&body.group_id)).await?;

    info!(user = %user.username, save_id = %save_id, group_id = %body.group_id, "save shared");
    crate::routes::saves::fetch_save(&state.pool, &save_id, &user_id)
        .await
        .map_err(|e| internal_logged("reading a save", e))?
        .map(Json)
        .ok_or_else(internal)
}

// ---- DELETE /v1/saves/:save_id/share

/// Refused while a member holds a live lease: their session would be pushing
/// into a namespace the save has just left.
pub async fn unshare(
    State(state): State<Arc<ServerState>>,
    Extension(user): Extension<AuthUser>,
    Path(save_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let user_id = user.user_id.to_string();
    let access = owner_access(&state.pool, &save_id, &user_id).await?;
    let Some(group_id) = access.group_id.clone() else {
        return Err(conflict("not_shared", "the save is not shared"));
    };
    let ttl = -LEASE_TTL_SECS;
    let held = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n: i64" FROM save_leases
           WHERE save_id = ? AND released_at IS NULL
             AND renewed_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ? || ' seconds')"#,
        save_id,
        ttl
    )
    .fetch_one(&state.pool)
    .await
    .map_err(|e| internal_logged("lease lookup", e))?;
    if held > 0 {
        return Err(conflict(
            "lease_held",
            "a member is hosting this save right now",
        ));
    }

    let from = Namespace::Group(group_id.clone());
    let to = Namespace::User(access.owner_user_id.clone());
    move_content(&state, &user_id, &save_id, &from, &to, None).await?;

    info!(user = %user.username, save_id = %save_id, group_id = %group_id, "save unshared");
    Ok(StatusCode::NO_CONTENT)
}

// ---- the move

/// One distinct sha the save references: its size and how many rows point at
/// it, which is what the refcounts move by.
struct Ref {
    sha: String,
    size: i64,
    count: i64,
}

/// Every whole-file blob and every chunk of every version of the save, trashed
/// ones included, since those still pin their bytes.
async fn save_refs(
    pool: &SqlitePool,
    save_id: &str,
    from: &Namespace,
) -> Result<(Vec<Ref>, Vec<Ref>), sqlx::Error> {
    let blobs = sqlx::query!(
        r#"SELECT sf.sha256 AS sha, MAX(sf.size_bytes) AS "size!: i64", COUNT(*) AS "count!: i64"
           FROM snapshot_files sf
           JOIN snapshots s ON s.id = sf.snapshot_id
           WHERE s.save_id = ?
             AND NOT EXISTS (SELECT 1 FROM snapshot_file_chunks c WHERE c.snapshot_file_id = sf.id)
           GROUP BY sf.sha256"#,
        save_id
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| Ref {
        sha: r.sha,
        size: r.size,
        count: r.count,
    })
    .collect();

    let chunk_rows = sqlx::query!(
        r#"SELECT sfc.chunk_sha256 AS sha, COUNT(*) AS "count!: i64"
           FROM snapshot_file_chunks sfc
           JOIN snapshot_files sf ON sf.id = sfc.snapshot_file_id
           JOIN snapshots s ON s.id = sf.snapshot_id
           WHERE s.save_id = ?
           GROUP BY sfc.chunk_sha256"#,
        save_id
    )
    .fetch_all(pool)
    .await?;
    let mut chunks = Vec::with_capacity(chunk_rows.len());
    for r in chunk_rows {
        // A chunk's size lives only in its row; a missing row is a broken
        // save, and moving it would only spread the damage.
        let size = namespace::chunk_size(pool, from, &r.sha)
            .await?
            .ok_or(sqlx::Error::RowNotFound)?;
        chunks.push(Ref {
            sha: r.sha,
            size,
            count: r.count,
        });
    }
    Ok((blobs, chunks))
}

/// Move the save's content from one namespace to the other, then record the
/// share (`group_id` set) or the unshare (`None`) in the same transaction.
async fn move_content(
    state: &ServerState,
    actor_id: &str,
    save_id: &str,
    from: &Namespace,
    to: &Namespace,
    group_id: Option<&str>,
) -> Result<(), ApiError> {
    let pool = &state.pool;
    let store = &state.store;
    let (blobs, chunks) = save_refs(pool, save_id, from)
        .await
        .map_err(|e| internal_logged("listing the save's content", e))?;

    // ---- disk: copy what the destination lacks. Idempotent, nothing recorded.
    for (r, is_chunk) in blobs
        .iter()
        .map(|r| (r, false))
        .chain(chunks.iter().map(|r| (r, true)))
    {
        let (src, dst) = if is_chunk {
            (from.chunk_key(&r.sha), to.chunk_key(&r.sha))
        } else {
            (from.blob_key(&r.sha), to.blob_key(&r.sha))
        };
        let present = store
            .exists(&dst)
            .await
            .map_err(|e| internal_logged("checking a destination object", e))?;
        if !present {
            store.copy(&src, &dst).await.map_err(|e| {
                warn!(error = %e, key = %src, "share: copying an object failed");
                internal_logged("copying an object", e)
            })?;
        }
    }

    // ---- rows, quota and the shared_saves row: one transaction
    let from_billing = from
        .billing_user(pool)
        .await
        .map_err(|e| internal_logged("billing lookup", e))?;
    let to_billing = to
        .billing_user(pool)
        .await
        .map_err(|e| internal_logged("billing lookup", e))?;
    let fail = |e: sqlx::Error, step: &'static str| internal_logged(step, e);

    let mut tx = pool
        .begin()
        .await
        .map_err(|e| fail(e, "opening the share transaction"))?;
    let mut to_new_bytes: i64 = 0;
    let mut from_freed_bytes: i64 = 0;
    // Source objects whose row reached 0: deleted after the commit.
    let mut orphans: Vec<(bool, String, String)> = Vec::new();

    for r in &blobs {
        let had = namespace::blob_size(&mut *tx, to, &r.sha)
            .await
            .map_err(|e| fail(e, "reading a destination blob"))?
            .is_some();
        namespace::blob_incref(&mut tx, to, &r.sha, r.size, r.count)
            .await
            .map_err(|e| fail(e, "referencing a destination blob"))?;
        if !had {
            to_new_bytes += r.size;
        }
        if let Some((rc, size)) = namespace::blob_decref(&mut tx, from, &r.sha, r.count)
            .await
            .map_err(|e| fail(e, "releasing a source blob"))?
        {
            if rc <= 0 {
                namespace::blob_delete_row(&mut tx, from, &r.sha)
                    .await
                    .map_err(|e| fail(e, "deleting a source blob row"))?;
                from_freed_bytes += size;
                orphans.push((false, r.sha.clone(), from.blob_key(&r.sha)));
            }
        }
    }
    for r in &chunks {
        let had = namespace::chunk_size(&mut *tx, to, &r.sha)
            .await
            .map_err(|e| fail(e, "reading a destination chunk"))?
            .is_some();
        namespace::chunk_incref(&mut tx, to, &r.sha, r.size, r.count)
            .await
            .map_err(|e| fail(e, "referencing a destination chunk"))?;
        if !had {
            to_new_bytes += r.size;
        }
        if let Some((rc, size)) = namespace::chunk_decref(&mut tx, from, &r.sha, r.count)
            .await
            .map_err(|e| fail(e, "releasing a source chunk"))?
        {
            if rc <= 0 {
                namespace::chunk_delete_row(&mut tx, from, &r.sha)
                    .await
                    .map_err(|e| fail(e, "deleting a source chunk row"))?;
                from_freed_bytes += size;
                orphans.push((true, r.sha.clone(), from.chunk_key(&r.sha)));
            }
        }
    }

    to.charge(&mut tx, &to_billing, to_new_bytes)
        .await
        .map_err(|e| fail(e, "charging the destination"))?;
    from.charge(&mut tx, &from_billing, -from_freed_bytes)
        .await
        .map_err(|e| fail(e, "refunding the source"))?;

    let event = match group_id {
        Some(gid) => {
            sqlx::query!(
                "INSERT INTO shared_saves (save_id, group_id) VALUES (?, ?)",
                save_id,
                gid
            )
            .execute(&mut *tx)
            .await
            .map_err(|e| fail(e, "recording the share"))?;
            "save.shared"
        }
        None => {
            sqlx::query!("DELETE FROM shared_saves WHERE save_id = ?", save_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| fail(e, "recording the unshare"))?;
            "save.unshared"
        }
    };
    let audit_id = Uuid::new_v4().to_string();
    let metadata = serde_json::json!({
        "save_id": save_id,
        "group_id": group_id.map(str::to_string).or_else(|| match from {
            Namespace::Group(g) => Some(g.clone()),
            Namespace::User(_) => None,
        }),
        "blobs": blobs.len(),
        "chunks": chunks.len(),
        "bytes_moved": to_new_bytes,
        "bytes_freed": from_freed_bytes,
    })
    .to_string();
    sqlx::query!(
        "INSERT INTO audit_log (id, user_id, event_type, entity_id, metadata)
         VALUES (?,?,?,?,?)",
        audit_id,
        actor_id,
        event,
        save_id,
        metadata
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| fail(e, "the share transaction"))?;
    tx.commit()
        .await
        .map_err(|e| fail(e, "committing the share"))?;

    // ---- the source objects nothing references any more. Re-checked like the
    // trash purge does: a push in the window since the commit may have
    // re-created the row.
    for (is_chunk, sha, key) in orphans {
        let revived = if is_chunk {
            namespace::chunk_refcount(pool, from, &sha).await
        } else {
            namespace::blob_refcount(pool, from, &sha).await
        }
        .unwrap_or(Some(1))
        .is_some_and(|rc| rc > 0);
        if revived {
            continue;
        }
        if let Err(e) = store.delete(&key).await {
            warn!(key = %key, error = %e, "share: deleting a moved object failed");
        }
    }
    Ok(())
}
