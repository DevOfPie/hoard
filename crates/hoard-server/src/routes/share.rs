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
//! and the next attempt finds its copies waiting. Then one `BEGIN IMMEDIATE`
//! transaction checks the save is still where the copy assumed, lists the
//! references again under the write lock, and moves the rows, the quota and
//! the `shared_saves` row together. Only after it commits are the source
//! objects nothing references any more deleted.
//!
//! The same move serves the places a share ends without its owner asking
//! ([`take_back`]): the group loses the member who shared it, the group's
//! owner is deleted, or the save itself is deleted. Those end the live lease
//! first, where `unshare` refuses under one.

use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    response::Json,
};
use hoard_core::wire::{validate_include, Save, ShareSaveRequest};
use sqlx::{Sqlite, SqliteConnection, SqlitePool};
use std::collections::HashSet;
use std::sync::Arc;
use tracing::{info, warn};
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::namespace::{self, Namespace};
use crate::routes::access::{save_access, Role, SaveAccess};
use crate::routes::health::ServerState;
use crate::routes::leases::{self, LeaseRow};
use crate::routes::snapshots::{err, internal, internal_logged, namespace_changed};
use crate::store::BlobStore;

type ApiError = (StatusCode, Json<serde_json::Value>);

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

fn move_err(e: MoveError) -> ApiError {
    match e {
        MoveError::Changed => namespace_changed(),
        MoveError::LeaseHeld => conflict("lease_held", "a member is hosting this save right now"),
        MoveError::Other(e) => internal_logged("moving a save's content", e),
    }
}

/// The include list stored with a share, as the client sent it; empty when the
/// save is unshared or shares whole.
pub async fn include_for<'e, E>(ex: E, save_id: &str) -> Result<Vec<String>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let json: Option<Option<String>> =
        sqlx::query_scalar("SELECT include_json FROM shared_saves WHERE save_id = ?")
            .bind(save_id)
            .fetch_optional(ex)
            .await?;
    Ok(json
        .flatten()
        .and_then(|j| serde_json::from_str::<Vec<String>>(&j).ok())
        .unwrap_or_default())
}

/// The first path a push carries that the share's include list does not name.
/// The list is what every member's walk filters with, so a client whose row
/// lost it must not be able to push the rest of its folder into the group.
pub fn first_outside_include<'a>(
    include: &[String],
    paths: impl IntoIterator<Item = &'a str>,
) -> Option<&'a str> {
    if include.is_empty() {
        return None;
    }
    paths
        .into_iter()
        .find(|p| !hoard_core::kernel::fileclass::included(include, p))
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
    validate_include(&body.include).map_err(|m| err(StatusCode::BAD_REQUEST, &m))?;
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

    // An empty list is stored as NULL: "everything", the shape every share had
    // before the column existed.
    let include_json = if body.include.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&body.include).map_err(|e| internal_logged("include", e))?)
    };
    move_content(
        &state.pool,
        &state.store,
        &user_id,
        &save_id,
        Move::Share {
            group_id: &body.group_id,
            include_json: include_json.as_deref(),
        },
    )
    .await
    .map_err(move_err)?;

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

    move_content(&state.pool, &state.store, &user_id, &save_id, Move::Unshare)
        .await
        .map_err(move_err)?;

    info!(user = %user.username, save_id = %save_id, group_id = %group_id, "save unshared");
    Ok(StatusCode::NO_CONTENT)
}

// ---- the move

/// Which way the content goes.
#[derive(Debug, Clone, Copy)]
pub enum Move<'a> {
    /// Into this group, naming what the save consists of (`None` is everything).
    Share {
        group_id: &'a str,
        include_json: Option<&'a str>,
    },
    /// Back to the owner, at the owner's request: a live lease refuses it.
    Unshare,
    /// Back to the owner with nobody asking: a live lease is ended first.
    TakeBack,
}

/// Why a move did not happen.
#[derive(Debug)]
pub enum MoveError {
    /// The save is not where the caller saw it: shared, unshared or deleted
    /// since. The caller re-reads and decides.
    Changed,
    /// A member holds a live lease and the move was not allowed to end it.
    LeaseHeld,
    Other(anyhow::Error),
}

impl From<sqlx::Error> for MoveError {
    fn from(e: sqlx::Error) -> Self {
        MoveError::Other(e.into())
    }
}

impl From<anyhow::Error> for MoveError {
    fn from(e: anyhow::Error) -> Self {
        MoveError::Other(e)
    }
}

impl std::fmt::Display for MoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MoveError::Changed => f.write_str("the save moved meanwhile"),
            MoveError::LeaseHeld => f.write_str("a member holds a live lease"),
            MoveError::Other(e) => write!(f, "{e:#}"),
        }
    }
}

/// `(owner_user_id, group_id)` of a save, `None` when it does not exist.
async fn whereabouts<'e, E>(
    ex: E,
    save_id: &str,
) -> Result<Option<(String, Option<String>)>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    let row = sqlx::query!(
        r#"SELECT s.user_id, ss.group_id AS "group_id?"
           FROM saves s LEFT JOIN shared_saves ss ON ss.save_id = s.id
           WHERE s.id = ?"#,
        save_id
    )
    .fetch_optional(ex)
    .await?;
    Ok(row.map(|r| (r.user_id, r.group_id)))
}

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
    conn: &mut SqliteConnection,
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
    .fetch_all(&mut *conn)
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
    .fetch_all(&mut *conn)
    .await?;
    let mut chunks = Vec::with_capacity(chunk_rows.len());
    for r in chunk_rows {
        // A chunk's size lives only in its row; a missing row is a broken
        // save, and moving it would only spread the damage.
        let size = namespace::chunk_size(&mut *conn, from, &r.sha)
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

/// `(source key, destination key, is_chunk, sha)` of every object to copy.
fn keys(
    blobs: &[Ref],
    chunks: &[Ref],
    from: &Namespace,
    to: &Namespace,
) -> Vec<(String, String, bool, String)> {
    blobs
        .iter()
        .map(|r| {
            (
                from.blob_key(&r.sha),
                to.blob_key(&r.sha),
                false,
                r.sha.clone(),
            )
        })
        .chain(chunks.iter().map(|r| {
            (
                from.chunk_key(&r.sha),
                to.chunk_key(&r.sha),
                true,
                r.sha.clone(),
            )
        }))
        .collect()
}

/// Move the save's content between its owner's namespace and its group's, and
/// record the share or unshare in the same transaction. The ended lease, when
/// [`Move::TakeBack`] found one, so the caller can announce it after the fact.
///
/// The copy runs outside the write lock and is idempotent; the transaction
/// re-lists the references and, when a push landed something the copy missed,
/// gives the lock back and copies again. Three rounds cover any race a lease
/// admits; more is a save being pushed to continuously, and a failure.
pub async fn move_content(
    pool: &SqlitePool,
    store: &Arc<dyn BlobStore>,
    actor_id: &str,
    save_id: &str,
    mv: Move<'_>,
) -> Result<Option<LeaseRow>, MoveError> {
    let (owner, group) = whereabouts(pool, save_id)
        .await?
        .ok_or(MoveError::Changed)?;
    let (from, to, group_id) = match (mv, group.as_deref()) {
        (Move::Share { group_id: g, .. }, None) => (
            Namespace::User(owner.clone()),
            Namespace::Group(g.to_string()),
            Some(g),
        ),
        (Move::Unshare | Move::TakeBack, Some(g)) => (
            Namespace::Group(g.to_string()),
            Namespace::User(owner.clone()),
            None,
        ),
        _ => return Err(MoveError::Changed),
    };
    let expected = (owner.clone(), group.clone());

    let mut copied: HashSet<(bool, String)> = HashSet::new();
    for round in 0..3 {
        // ---- disk: copy what the destination lacks. Idempotent, nothing recorded.
        let (blobs, chunks) = {
            let mut conn = pool.acquire().await?;
            save_refs(&mut conn, save_id, &from).await?
        };
        for (src, dst, is_chunk, sha) in keys(&blobs, &chunks, &from, &to) {
            if !copied.insert((is_chunk, sha)) {
                continue;
            }
            if !store.exists(&dst).await? {
                store.copy(&src, &dst).await.map_err(|e| {
                    warn!(error = %e, key = %src, "share: copying an object failed");
                    e
                })?;
            }
        }

        // ---- rows, quota and the shared_saves row: one transaction
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        if whereabouts(&mut *tx, save_id).await?.as_ref() != Some(&expected) {
            return Err(MoveError::Changed);
        }
        let (blobs, chunks) = save_refs(&mut tx, save_id, &from).await?;
        if blobs
            .iter()
            .map(|r| (false, r.sha.as_str()))
            .chain(chunks.iter().map(|r| (true, r.sha.as_str())))
            .any(|(is_chunk, sha)| !copied.contains(&(is_chunk, sha.to_string())))
        {
            warn!(
                save_id,
                round, "share: a push landed during the copy, copying again"
            );
            drop(tx);
            continue;
        }

        let ended = match mv {
            Move::Share { .. } => None,
            Move::Unshare => {
                if leases::live_lease(&mut *tx, save_id).await?.is_some() {
                    return Err(MoveError::LeaseHeld);
                }
                None
            }
            Move::TakeBack => match leases::live_lease(&mut *tx, save_id).await? {
                Some(lease) => {
                    leases::end_lease_row(&mut tx, &lease).await?;
                    Some(lease)
                }
                None => None,
            },
        };

        let from_billing = from.billing_user(&mut *tx).await?;
        let to_billing = to.billing_user(&mut *tx).await?;
        let mut to_new_bytes: i64 = 0;
        let mut from_freed_bytes: i64 = 0;
        // Source objects whose row reached 0: deleted after the commit.
        let mut orphans: Vec<(bool, String, String)> = Vec::new();

        for r in &blobs {
            let had = namespace::blob_size(&mut *tx, &to, &r.sha).await?.is_some();
            namespace::blob_incref(&mut tx, &to, &r.sha, r.size, r.count).await?;
            if !had {
                to_new_bytes += r.size;
            }
            if let Some((rc, size)) =
                namespace::blob_decref(&mut tx, &from, &r.sha, r.count).await?
            {
                if rc <= 0 {
                    namespace::blob_delete_row(&mut tx, &from, &r.sha).await?;
                    from_freed_bytes += size;
                    orphans.push((false, r.sha.clone(), from.blob_key(&r.sha)));
                }
            }
        }
        for r in &chunks {
            let had = namespace::chunk_size(&mut *tx, &to, &r.sha)
                .await?
                .is_some();
            namespace::chunk_incref(&mut tx, &to, &r.sha, r.size, r.count).await?;
            if !had {
                to_new_bytes += r.size;
            }
            if let Some((rc, size)) =
                namespace::chunk_decref(&mut tx, &from, &r.sha, r.count).await?
            {
                if rc <= 0 {
                    namespace::chunk_delete_row(&mut tx, &from, &r.sha).await?;
                    from_freed_bytes += size;
                    orphans.push((true, r.sha.clone(), from.chunk_key(&r.sha)));
                }
            }
        }

        to.charge(&mut tx, &to_billing, to_new_bytes).await?;
        from.charge(&mut tx, &from_billing, -from_freed_bytes)
            .await?;

        let event = match mv {
            Move::Share {
                group_id: gid,
                include_json,
            } => {
                sqlx::query!(
                    "INSERT INTO shared_saves (save_id, group_id, include_json) VALUES (?, ?, ?)",
                    save_id,
                    gid,
                    include_json
                )
                .execute(&mut *tx)
                .await?;
                "save.shared"
            }
            Move::Unshare | Move::TakeBack => {
                sqlx::query!("DELETE FROM shared_saves WHERE save_id = ?", save_id)
                    .execute(&mut *tx)
                    .await?;
                "save.unshared"
            }
        };
        let audit_id = Uuid::new_v4().to_string();
        let metadata = serde_json::json!({
            "save_id": save_id,
            "group_id": group_id.map(str::to_string).or_else(|| match &from {
                Namespace::Group(g) => Some(g.clone()),
                Namespace::User(_) => None,
            }),
            "blobs": blobs.len(),
            "chunks": chunks.len(),
            "bytes_moved": to_new_bytes,
            "bytes_freed": from_freed_bytes,
            "lease_ended": ended.is_some(),
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
        .await?;
        tx.commit().await?;

        // ---- the source objects nothing references any more. Re-checked like
        // the trash purge does: a push in the window since the commit may have
        // re-created the row.
        for (is_chunk, sha, key) in orphans {
            let revived = if is_chunk {
                namespace::chunk_refcount(pool, &from, &sha).await
            } else {
                namespace::blob_refcount(pool, &from, &sha).await
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
        return Ok(ended);
    }
    Err(MoveError::Other(anyhow::anyhow!(
        "the save kept growing while its content was being copied"
    )))
}

// ---- taking shares back without the owner

/// A shared save back into its owner's namespace with nobody asking. A save
/// that is no longer shared, or no longer exists, is left alone. The ended
/// lease, if any, for the caller to announce.
pub async fn take_back(
    pool: &SqlitePool,
    store: &Arc<dyn BlobStore>,
    actor_id: &str,
    save_id: &str,
) -> anyhow::Result<Option<LeaseRow>> {
    for _ in 0..3 {
        match whereabouts(pool, save_id).await? {
            Some((_, Some(_))) => {}
            _ => return Ok(None),
        }
        match move_content(pool, store, actor_id, save_id, Move::TakeBack).await {
            Ok(ended) => return Ok(ended),
            Err(MoveError::Changed) => continue,
            Err(MoveError::LeaseHeld) => unreachable!("TakeBack ends the lease"),
            Err(MoveError::Other(e)) => return Err(e),
        }
    }
    anyhow::bail!("save {save_id} kept moving while being taken back")
}

/// Every save shared into `group_id` back to its owner, or only the ones
/// `owned_by` owns. The leases ended along the way.
pub async fn take_back_group(
    pool: &SqlitePool,
    store: &Arc<dyn BlobStore>,
    actor_id: &str,
    group_id: &str,
    owned_by: Option<&str>,
) -> anyhow::Result<Vec<LeaseRow>> {
    let save_ids = sqlx::query_scalar!(
        r#"SELECT ss.save_id AS "save_id!: String"
           FROM shared_saves ss JOIN saves s ON s.id = ss.save_id
           WHERE ss.group_id = ? AND (? IS NULL OR s.user_id = ?)"#,
        group_id,
        owned_by,
        owned_by
    )
    .fetch_all(pool)
    .await?;
    let mut ended = Vec::new();
    for save_id in &save_ids {
        ended.extend(take_back(pool, store, actor_id, save_id).await?);
    }
    Ok(ended)
}

/// Every save `owner_id` owns that is shared anywhere, back to them. The
/// leases ended along the way.
pub async fn take_back_owned(
    pool: &SqlitePool,
    store: &Arc<dyn BlobStore>,
    actor_id: &str,
    owner_id: &str,
) -> anyhow::Result<Vec<LeaseRow>> {
    let save_ids = sqlx::query_scalar!(
        r#"SELECT ss.save_id AS "save_id!: String"
           FROM shared_saves ss JOIN saves s ON s.id = ss.save_id
           WHERE s.user_id = ?"#,
        owner_id
    )
    .fetch_all(pool)
    .await?;
    let mut ended = Vec::new();
    for save_id in &save_ids {
        ended.extend(take_back(pool, store, actor_id, save_id).await?);
    }
    Ok(ended)
}
