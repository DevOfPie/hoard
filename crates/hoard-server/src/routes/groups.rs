//! Groups: create, list, invite, join, leave (`/v1/groups`).
//!
//! A group is one owner plus members, joined by one-time invite links. The
//! token in a link is random, shown once, and stored as its sha256 the way
//! `api_tokens.token_hash` is (`auth.rs`), so the database never holds anything
//! that opens a group.
//!
//! A caller who is not a member of a group gets the same 404 an unknown id
//! gets: membership is the only way to learn a group exists.

use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    response::Json,
};
use base64::Engine;
use hoard_core::wire::{
    CreateGroupRequest, CreateInviteRequest, Group, GroupMember, InviteOut, JoinGroupRequest,
};
use rand::RngCore;
use sqlx::SqlitePool;
use std::sync::Arc;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::routes::health::ServerState;
use crate::routes::snapshots::{err, internal_logged};
use crate::routes::{repair_ts, repair_username};

type ApiError = (StatusCode, Json<serde_json::Value>);

const MAX_NAME_CHARS: usize = 64;
const DEFAULT_INVITE_TTL_SECS: u64 = 7 * 24 * 60 * 60;
/// A year. Past it `OffsetDateTime` arithmetic panics before the stamp exists.
const MAX_INVITE_TTL_SECS: u64 = 365 * 24 * 60 * 60;

/// One (group, member) row; `fold_groups` turns a run of them into `Group`s.
struct GroupRow {
    id: String,
    name: String,
    owner_user_id: String,
    created_at: String,
    member_id: String,
    username: String,
    role: String,
    joined_at: String,
}

/// Rows arrive ordered by group, so each group is one contiguous run.
fn fold_groups(rows: Vec<GroupRow>) -> Vec<Group> {
    let mut out: Vec<Group> = Vec::new();
    for r in rows {
        if out.last().map(|g| g.id.as_str()) != Some(r.id.as_str()) {
            out.push(Group {
                id: r.id.clone(),
                name: r.name,
                owner_user_id: r.owner_user_id,
                created_at: repair_ts(&r.created_at),
                members: Vec::new(),
            });
        }
        out.last_mut().unwrap().members.push(GroupMember {
            user_id: r.member_id,
            username: repair_username(&r.username),
            role: r.role,
            joined_at: repair_ts(&r.joined_at),
        });
    }
    out
}

async fn fetch_group(pool: &SqlitePool, group_id: &str) -> Result<Option<Group>, sqlx::Error> {
    let rows = sqlx::query_as!(
        GroupRow,
        r#"SELECT g.id, g.name, g.owner_user_id, g.created_at,
                  gm.user_id AS member_id, u.username, gm.role, gm.joined_at
           FROM groups g
           JOIN group_members gm ON gm.group_id = g.id
           JOIN users u ON u.id = gm.user_id
           WHERE g.id = ?
           ORDER BY gm.joined_at, gm.user_id"#,
        group_id
    )
    .fetch_all(pool)
    .await?;
    Ok(fold_groups(rows).pop())
}

/// The caller's row in a group, with the group's owner. `None` when the group
/// does not exist or the caller is not in it, which the routes treat alike.
struct Standing {
    owner_user_id: String,
}

async fn standing(
    pool: &SqlitePool,
    group_id: &str,
    user_id: &str,
) -> Result<Option<Standing>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT g.owner_user_id
           FROM groups g
           JOIN group_members gm ON gm.group_id = g.id AND gm.user_id = ?
           WHERE g.id = ?"#,
        user_id,
        group_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| Standing {
        owner_user_id: r.owner_user_id,
    }))
}

/// Same format the migrations write, so text comparisons in SQL sort right.
fn stamp(at: OffsetDateTime) -> String {
    let d = at.date();
    let t = at.time();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        d.year(),
        u8::from(d.month()),
        d.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

fn not_found() -> ApiError {
    err(StatusCode::NOT_FOUND, "group not found")
}

fn owner_only() -> ApiError {
    err(StatusCode::FORBIDDEN, "only the group owner can do that")
}

// ---- POST /v1/groups

pub async fn create(
    State(state): State<Arc<ServerState>>,
    Extension(user): Extension<AuthUser>,
    Json(body): Json<CreateGroupRequest>,
) -> Result<(StatusCode, Json<Group>), ApiError> {
    let name = body.name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "group name must be 1 to 64 characters",
        ));
    }
    let id = Uuid::new_v4().to_string();
    let user_id = user.user_id.to_string();

    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|e| internal_logged("opening a transaction", e))?;
    sqlx::query!(
        "INSERT INTO groups (id, name, owner_user_id) VALUES (?, ?, ?)",
        id,
        name,
        user_id
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| internal_logged("inserting a group", e))?;
    sqlx::query!(
        "INSERT INTO group_members (group_id, user_id, role) VALUES (?, ?, 'owner')",
        id,
        user_id
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| internal_logged("inserting the owner membership", e))?;
    tx.commit()
        .await
        .map_err(|e| internal_logged("committing a group", e))?;

    let group = fetch_group(&state.pool, &id)
        .await
        .map_err(|e| internal_logged("reading a group", e))?
        .ok_or_else(crate::routes::snapshots::internal)?;
    Ok((StatusCode::CREATED, Json(group)))
}

// ---- GET /v1/groups

pub async fn list(
    State(state): State<Arc<ServerState>>,
    Extension(user): Extension<AuthUser>,
) -> Result<Json<Vec<Group>>, ApiError> {
    let user_id = user.user_id.to_string();
    let rows = sqlx::query_as!(
        GroupRow,
        r#"SELECT g.id, g.name, g.owner_user_id, g.created_at,
                  gm.user_id AS member_id, u.username, gm.role, gm.joined_at
           FROM groups g
           JOIN group_members me ON me.group_id = g.id AND me.user_id = ?
           JOIN group_members gm ON gm.group_id = g.id
           JOIN users u ON u.id = gm.user_id
           ORDER BY g.created_at, g.id, gm.joined_at, gm.user_id"#,
        user_id
    )
    .fetch_all(&state.pool)
    .await
    .map_err(|e| internal_logged("listing groups", e))?;
    Ok(Json(fold_groups(rows)))
}

// ---- DELETE /v1/groups/:id

/// Refused while a save is shared into the group: unsharing moves blobs, and a
/// cascade here would leave members' copies pointing at nothing.
pub async fn delete(
    State(state): State<Arc<ServerState>>,
    Extension(user): Extension<AuthUser>,
    Path(group_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let user_id = user.user_id.to_string();
    let st = standing(&state.pool, &group_id, &user_id)
        .await
        .map_err(|e| internal_logged("reading a membership", e))?
        .ok_or_else(not_found)?;
    if st.owner_user_id != user_id {
        return Err(owner_only());
    }
    // The guard, the object purge and the row go under one write lock: a
    // share landing between them would cascade its rows away and orphan its
    // objects. With no save shared the namespace should hold nothing; what it
    // does hold is stray, and would otherwise outlive its `group_blobs` rows.
    let mut tx = state
        .pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(|e| internal_logged("opening a transaction", e))?;
    let shared: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n: i64" FROM shared_saves WHERE group_id = ?"#,
        group_id
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| internal_logged("counting shared saves", e))?;
    if shared > 0 {
        return Err(err(
            StatusCode::CONFLICT,
            "unshare every save before deleting the group",
        ));
    }
    let (objects, bytes) = crate::store::purge_group_objects(&mut tx, &state.store, &group_id)
        .await
        .map_err(|e| internal_logged("purging a group's objects", e))?;
    if objects > 0 {
        tracing::warn!(
            group_id,
            objects,
            bytes,
            "group delete: stray objects purged"
        );
    }
    sqlx::query!("DELETE FROM groups WHERE id = ?", group_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| internal_logged("deleting a group", e))?;
    tx.commit()
        .await
        .map_err(|e| internal_logged("committing a group delete", e))?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- POST /v1/groups/:id/invites

pub async fn create_invite(
    State(state): State<Arc<ServerState>>,
    Extension(user): Extension<AuthUser>,
    Path(group_id): Path<String>,
    Json(body): Json<CreateInviteRequest>,
) -> Result<(StatusCode, Json<InviteOut>), ApiError> {
    let user_id = user.user_id.to_string();
    let st = standing(&state.pool, &group_id, &user_id)
        .await
        .map_err(|e| internal_logged("reading a membership", e))?
        .ok_or_else(not_found)?;
    if st.owner_user_id != user_id {
        return Err(owner_only());
    }
    let ttl = body.expires_in_secs.unwrap_or(DEFAULT_INVITE_TTL_SECS);
    if ttl == 0 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "expires_in_secs must be positive",
        ));
    }
    if ttl > MAX_INVITE_TTL_SECS {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "expires_in_secs must be at most one year",
        ));
    }
    let ttl = i64::try_from(ttl).expect("capped at a year");

    let mut raw = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw);
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
    let token_hash = hoard_core::hashing::hash_token(&token);
    let invite_id = Uuid::new_v4().to_string();
    let expires_at = OffsetDateTime::now_utc() + Duration::seconds(ttl);
    let expires_stamp = stamp(expires_at);

    sqlx::query!(
        "INSERT INTO group_invites (id, group_id, token_hash, created_by, expires_at)
         VALUES (?, ?, ?, ?, ?)",
        invite_id,
        group_id,
        token_hash,
        user_id,
        expires_stamp
    )
    .execute(&state.pool)
    .await
    .map_err(|e| internal_logged("inserting an invite", e))?;

    Ok((
        StatusCode::CREATED,
        Json(InviteOut {
            invite_id,
            token,
            expires_at: repair_ts(&expires_stamp),
        }),
    ))
}

// ---- POST /v1/groups/join

/// Redeem an invite. A caller already in the group gets it back whatever the
/// token's state, so a retried join is harmless; anyone else needs a token that
/// is unknown to nobody, unused and unexpired, and every failure is the same 404.
pub async fn join(
    State(state): State<Arc<ServerState>>,
    Extension(user): Extension<AuthUser>,
    Json(body): Json<JoinGroupRequest>,
) -> Result<Json<Group>, ApiError> {
    let user_id = user.user_id.to_string();
    let token_hash = hoard_core::hashing::hash_token(body.token.trim());
    let invalid = || err(StatusCode::NOT_FOUND, "invite not found");

    let invite = sqlx::query!(
        "SELECT id, group_id, expires_at, used_at FROM group_invites WHERE token_hash = ?",
        token_hash
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| internal_logged("reading an invite", e))?
    .ok_or_else(invalid)?;

    if standing(&state.pool, &invite.group_id, &user_id)
        .await
        .map_err(|e| internal_logged("reading a membership", e))?
        .is_some()
    {
        return fetch_group(&state.pool, &invite.group_id)
            .await
            .map_err(|e| internal_logged("reading a group", e))?
            .map(Json)
            .ok_or_else(invalid);
    }

    let now = stamp(OffsetDateTime::now_utc());
    if invite.used_at.is_some() || invite.expires_at.as_str() < now.as_str() {
        return Err(invalid());
    }

    // The UPDATE carries the checks: two redemptions of one token serialise on
    // the write lock, and the second finds `used_at` set and changes nothing.
    let mut tx = state
        .pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(|e| internal_logged("opening a transaction", e))?;
    let used = sqlx::query!(
        "UPDATE group_invites SET used_by = ?, used_at = ?
         WHERE id = ? AND used_at IS NULL AND expires_at > ?",
        user_id,
        now,
        invite.id,
        now
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| internal_logged("marking an invite used", e))?;
    if used.rows_affected() == 0 {
        return Err(invalid());
    }
    sqlx::query!(
        "INSERT INTO group_members (group_id, user_id, role) VALUES (?, ?, 'member')",
        invite.group_id,
        user_id
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| internal_logged("inserting a membership", e))?;
    tx.commit()
        .await
        .map_err(|e| internal_logged("committing a join", e))?;

    fetch_group(&state.pool, &invite.group_id)
        .await
        .map_err(|e| internal_logged("reading a group", e))?
        .map(Json)
        .ok_or_else(invalid)
}

// ---- DELETE /v1/groups/:id/members/:user

/// The owner removes anybody but themself; a member removes only themself.
/// Ownership does not transfer, so the owner leaving would orphan the group.
pub async fn remove_member(
    State(state): State<Arc<ServerState>>,
    Extension(user): Extension<AuthUser>,
    Path((group_id, target)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let user_id = user.user_id.to_string();
    let st = standing(&state.pool, &group_id, &user_id)
        .await
        .map_err(|e| internal_logged("reading a membership", e))?
        .ok_or_else(not_found)?;
    let is_owner = st.owner_user_id == user_id;
    if !is_owner && target != user_id {
        return Err(err(
            StatusCode::FORBIDDEN,
            "a member can only remove themself",
        ));
    }
    if target == st.owner_user_id {
        return Err(err(
            StatusCode::CONFLICT,
            "the owner cannot leave their own group",
        ));
    }
    // What the departing member holds in the group goes first: the leases
    // they hold (they can no longer read the saves they host), then the saves
    // they shared, back into their own namespace (the owner would go on
    // paying for a save nobody in the group can unshare). Each ended lease is
    // announced once its transaction is behind it.
    let ended = crate::routes::leases::end_leases_held_in_group(&state.pool, &group_id, &target)
        .await
        .map_err(|e| internal_logged("ending the member's leases", e))?;
    for lease in &ended {
        crate::routes::leases::announce_end(&state, lease).await;
    }
    let ended = crate::routes::share::take_back_group(
        &state.pool,
        &state.store,
        &user_id,
        &group_id,
        Some(&target),
    )
    .await
    .map_err(|e| internal_logged("taking back the member's shared saves", e))?;
    for lease in &ended {
        crate::routes::leases::announce_end(&state, lease).await;
    }
    let done = sqlx::query!(
        "DELETE FROM group_members WHERE group_id = ? AND user_id = ?",
        group_id,
        target
    )
    .execute(&state.pool)
    .await
    .map_err(|e| internal_logged("removing a member", e))?;
    if done.rows_affected() == 0 {
        return Err(err(StatusCode::NOT_FOUND, "member not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}
