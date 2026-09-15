use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    response::Json,
};
use hoard_core::wire::{CreateSaveRequest, PatchSaveRequest, Save, SharedInfo};
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::routes::health::ServerState;
use crate::routes::{parse_save_id, repair_slug, repair_ts, repair_username};

// ---- request and response types
//
// The shapes live in `hoard_core::wire` (ADR 0021 C.6): the client compiles against
// the same ones, so drift between the two ends is a compile error rather than a 422
// in production.

#[derive(Deserialize)]
pub struct ListQuery {
    pub game_slug: Option<String>,
}

// ─── Handlers ───────────────────────────────────────────────────────────────

pub async fn create(
    State(state): State<Arc<ServerState>>,
    Extension(user): Extension<AuthUser>,
    Json(body): Json<CreateSaveRequest>,
) -> Result<(StatusCode, Json<Save>), (StatusCode, Json<serde_json::Value>)> {
    // `game_slug` already went through `GameSlug`'s gate when the body was
    // deserialised, so all that is needed here is the `&str` for SQL and for the
    // disk paths.
    let slug_str = body.game_slug.as_str();

    // Validate game exists
    let game_count: i64 =
        sqlx::query_scalar!("SELECT COUNT(*) as cnt FROM games WHERE slug = ?", slug_str)
            .fetch_one(&state.pool)
            .await
            .map_err(|e| internal_logged("reading a row", e))?;

    if game_count == 0 {
        // Self-heal path: if the client supplied a display_name (newer
        // desktops do; older CLI clients don't), insert a minimal games
        // stub. This unblocks tracking when the desktop's Ludusavi catalog
        // is fresher than the server's seed; the alternative is making
        // the user manually re-import the manifest on every server upgrade.
        if let Some(display) = body.display_name.as_deref().filter(|s| !s.is_empty()) {
            let display = display.to_string();
            let slug_for_insert = slug_str.to_string();
            let res = sqlx::query!(
                "INSERT INTO games (slug, display_name, steam_app_id, imported_from)
                 VALUES (?, ?, ?, 'client-supplied')
                 ON CONFLICT(slug) DO NOTHING",
                slug_for_insert,
                display,
                body.steam_app_id,
            )
            .execute(&state.pool)
            .await;
            if let Err(e) = res {
                tracing::warn!(error = %e, slug = %body.game_slug,
                    "couldn't self-heal games row from client metadata");
                return Err((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(serde_json::json!({"error": "game not found"})),
                ));
            }
            tracing::info!(slug = %body.game_slug,
                "inserted client-supplied games row to unblock tracking");
        } else {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({"error": "game not found"})),
            ));
        }
    }

    let label = body.label.unwrap_or_else(|| "default".to_string());
    let id = Uuid::new_v4().to_string();
    let user_id = user.user_id.to_string();

    sqlx::query!(
        "INSERT INTO saves (id, user_id, game_slug, label, local_path_hint, client_os)
         VALUES (?,?,?,?,?,?)",
        id,
        user_id,
        slug_str,
        label,
        body.local_path_hint,
        body.client_os
    )
    .execute(&state.pool)
    .await
    .map_err(|e| {
        if e.to_string().contains("UNIQUE constraint") {
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "save with this game and label already exists"
                })),
            )
        } else {
            internal_err()
        }
    })?;

    // Create physical directory
    let save_dir = state
        .config
        .storage
        .data_dir
        .join("data")
        .join(&user_id)
        .join(slug_str)
        .join(&label);
    tokio::fs::create_dir_all(&save_dir).await.ok();

    let row = fetch_save(&state.pool, &id, &user_id)
        .await
        .map_err(|e| internal_logged("database access", e))?
        .ok_or_else(internal_err)?;

    Ok((StatusCode::CREATED, Json(row)))
}

/// Every save the caller owns plus every save shared into a group they belong
/// to, each with `shared` filled when a `shared_saves` row exists.
pub async fn list(
    State(state): State<Arc<ServerState>>,
    Extension(user): Extension<AuthUser>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<Save>>, StatusCode> {
    let user_id = user.user_id.to_string();

    let rows = if let Some(slug) = q.game_slug {
        sqlx::query_as!(
            SaveRow,
            r#"SELECT s.id, s.user_id as owner_user_id, s.game_slug, s.label,
                      s.local_path_hint, s.client_os,
                      s.latest_version_num, s.created_at, s.updated_at,
                      COALESCE(COUNT(sn.id), 0) as "snapshot_count: i64",
                      COALESCE(SUM(sn.total_size_bytes), 0) as "total_size_bytes: i64",
                      ss.group_id as "group_id?", g.name as "group_name?",
                      g.owner_user_id as "group_owner_id?", u.username as "group_owner_name?",
                      ss.include_json as "include_json?"
               FROM saves s
               LEFT JOIN shared_saves ss ON ss.save_id = s.id
               LEFT JOIN groups g ON g.id = ss.group_id
               LEFT JOIN users u ON u.id = g.owner_user_id
               LEFT JOIN group_members gm ON gm.group_id = ss.group_id AND gm.user_id = ?
               LEFT JOIN snapshots sn ON sn.save_id = s.id AND sn.deleted_at IS NULL
               WHERE (s.user_id = ? OR gm.user_id IS NOT NULL) AND s.game_slug = ?
               GROUP BY s.id ORDER BY s.created_at"#,
            user_id,
            user_id,
            slug
        )
        .fetch_all(&state.pool)
        .await
    } else {
        sqlx::query_as!(
            SaveRow,
            r#"SELECT s.id, s.user_id as owner_user_id, s.game_slug, s.label,
                      s.local_path_hint, s.client_os,
                      s.latest_version_num, s.created_at, s.updated_at,
                      COALESCE(COUNT(sn.id), 0) as "snapshot_count: i64",
                      COALESCE(SUM(sn.total_size_bytes), 0) as "total_size_bytes: i64",
                      ss.group_id as "group_id?", g.name as "group_name?",
                      g.owner_user_id as "group_owner_id?", u.username as "group_owner_name?",
                      ss.include_json as "include_json?"
               FROM saves s
               LEFT JOIN shared_saves ss ON ss.save_id = s.id
               LEFT JOIN groups g ON g.id = ss.group_id
               LEFT JOIN users u ON u.id = g.owner_user_id
               LEFT JOIN group_members gm ON gm.group_id = ss.group_id AND gm.user_id = ?
               LEFT JOIN snapshots sn ON sn.save_id = s.id AND sn.deleted_at IS NULL
               WHERE s.user_id = ? OR gm.user_id IS NOT NULL
               GROUP BY s.id ORDER BY s.created_at"#,
            user_id,
            user_id
        )
        .fetch_all(&state.pool)
        .await
    }
    .map_err(|e| internal_logged_status("listing rows", e))?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        if let Some(save) = row
            .into_visible(&state.pool, &user_id)
            .await
            .map_err(|e| internal_logged_status("reading a member's totals", e))?
        {
            out.push(save);
        }
    }
    Ok(Json(out))
}

/// One save by id, for its owner or a member of the group it is shared into.
pub async fn get_one(
    State(state): State<Arc<ServerState>>,
    Extension(user): Extension<AuthUser>,
    Path(save_id): Path<String>,
) -> Result<Json<Save>, StatusCode> {
    let user_id = user.user_id.to_string();
    fetch_save(&state.pool, &save_id, &user_id)
        .await
        .map_err(|e| internal_logged_status("database access", e))?
        .ok_or(StatusCode::NOT_FOUND)
        .map(Json)
}

pub async fn patch(
    State(state): State<Arc<ServerState>>,
    Extension(user): Extension<AuthUser>,
    Path(save_id): Path<String>,
    Json(body): Json<PatchSaveRequest>,
) -> Result<Json<Save>, (StatusCode, Json<serde_json::Value>)> {
    let user_id = user.user_id.to_string();

    // Verify ownership and fetch the current label so we can rename the
    // physical snapshot directory atomically when the label changes.
    let current = sqlx::query!(
        "SELECT game_slug, label FROM saves WHERE id=? AND user_id=?",
        save_id,
        user_id
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| internal_logged("reading a row", e))?
    .ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error":"not found"})),
        )
    })?;

    // ---- Label rename ------------------------------------------------------
    // The label is part of the on-disk path (data/<user>/<game>/<label>/v*/),
    // so a rename has to move two pieces in lockstep: the `saves.label`
    // row and the directory itself. We do the UPDATE inside a transaction,
    // then rename the directory, then commit. If the rename fails we roll
    // back the UPDATE so the user sees a clean error instead of a save
    // whose old snapshots are unreachable.
    if let Some(new_label) = body
        .label
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if new_label != current.label {
            let mut tx = state
                .pool
                .begin()
                .await
                .map_err(|e| internal_logged("opening a transaction", e))?;

            sqlx::query!("UPDATE saves SET label=? WHERE id=?", new_label, save_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    if e.to_string().contains("UNIQUE") {
                        (
                            StatusCode::CONFLICT,
                            Json(serde_json::json!({"error":"label collision"})),
                        )
                    } else {
                        internal_err()
                    }
                })?;

            let old_dir = state
                .config
                .storage
                .data_dir
                .join("data")
                .join(&user_id)
                .join(&current.game_slug)
                .join(&current.label);
            let new_dir = state
                .config
                .storage
                .data_dir
                .join("data")
                .join(&user_id)
                .join(&current.game_slug)
                .join(new_label);

            if old_dir.exists() {
                // Reject if the target dir already exists: the UNIQUE
                // constraint above should make this unreachable but we
                // double-check rather than trust two writers can't race.
                if new_dir.exists() {
                    tx.rollback().await.ok();
                    return Err((
                        StatusCode::CONFLICT,
                        Json(serde_json::json!({"error":"target directory already exists"})),
                    ));
                }
                if let Some(parent) = new_dir.parent() {
                    tokio::fs::create_dir_all(parent).await.ok();
                }
                if let Err(e) = tokio::fs::rename(&old_dir, &new_dir).await {
                    tracing::warn!(error = %e, old = ?old_dir, new = ?new_dir,
                        "rename failed during save label patch; rolling back DB");
                    tx.rollback().await.ok();
                    return Err(internal_err());
                }
            }

            if let Err(e) = tx.commit().await {
                // Commit failed after the rename succeeded, so try to undo
                // the rename so the world stays consistent.
                tracing::warn!(error = %e, "commit failed after rename; reverting rename");
                tokio::fs::rename(&new_dir, &old_dir).await.ok();
                return Err(internal_err());
            }
        }
    }

    if let Some(hint) = &body.local_path_hint {
        sqlx::query!(
            "UPDATE saves SET local_path_hint=? WHERE id=?",
            hint,
            save_id
        )
        .execute(&state.pool)
        .await
        .map_err(|e| internal_logged("writing to the database", e))?;
    }
    if let Some(os) = &body.client_os {
        sqlx::query!("UPDATE saves SET client_os=? WHERE id=?", os, save_id)
            .execute(&state.pool)
            .await
            .map_err(|e| internal_logged("writing to the database", e))?;
    }

    fetch_save(&state.pool, &save_id, &user_id)
        .await
        .map_err(|e| internal_logged("writing to the database", e))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error":"not found"})),
            )
        })
        .map(Json)
}

pub async fn delete(
    State(state): State<Arc<ServerState>>,
    Extension(user): Extension<AuthUser>,
    Path(save_id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let user_id = user.user_id.to_string();

    let row = sqlx::query!(
        "SELECT game_slug, label FROM saves WHERE id=? AND user_id=?",
        save_id,
        user_id
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| internal_logged_status("reading a row", e))?
    .ok_or(StatusCode::NOT_FOUND)?;

    // A shared save comes back to its owner first: the cascade would take the
    // `shared_saves` row without releasing the group's refcounts or refunding
    // its owner. A share landing between the take-back and the delete is
    // caught under the write lock and taken back again.
    for round in 0.. {
        crate::routes::share::take_back(&state.pool, &state.store, &user_id, &save_id)
            .await
            .map_err(|e| internal_logged_status("taking back a shared save", e))?;
        let mut tx = state
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| internal_logged_status("opening a transaction", e))?;
        let shared = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n: i64" FROM shared_saves WHERE save_id = ?"#,
            save_id
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| internal_logged_status("reading a row", e))?;
        if shared > 0 {
            if round >= 2 {
                return Err(StatusCode::CONFLICT);
            }
            continue;
        }
        sqlx::query!("DELETE FROM saves WHERE id=?", save_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| internal_logged_status("writing to the database", e))?;
        tx.commit()
            .await
            .map_err(|e| internal_logged_status("writing to the database", e))?;
        break;
    }

    // Remove physical directory
    let dir = state
        .config
        .storage
        .data_dir
        .join("data")
        .join(&user_id)
        .join(&row.game_slug)
        .join(&row.label);
    tokio::fs::remove_dir_all(&dir).await.ok();

    Ok(StatusCode::NO_CONTENT)
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// A `saves` row with its aggregates and, when shared, its group. The same
/// columns from every query so one conversion serves them all.
struct SaveRow {
    id: String,
    owner_user_id: String,
    game_slug: String,
    label: String,
    local_path_hint: Option<String>,
    client_os: Option<String>,
    latest_version_num: i64,
    created_at: String,
    updated_at: String,
    snapshot_count: Option<i64>,
    total_size_bytes: Option<i64>,
    group_id: Option<String>,
    group_name: Option<String>,
    group_owner_id: Option<String>,
    group_owner_name: Option<String>,
    include_json: Option<String>,
}

impl SaveRow {
    /// [`Self::into_wire`] as `user_id` may see it. The row's size sums every
    /// file of every live version; a member of a share that names its files
    /// reads only those, so for that caller alone the size is recomputed from
    /// the live versions' manifests, matched the way the snapshot list matches
    /// them. The owner and a member of an unfiltered share cost no query.
    async fn into_visible(
        self,
        pool: &sqlx::SqlitePool,
        user_id: &str,
    ) -> Result<Option<Save>, sqlx::Error> {
        let is_owner = self.owner_user_id == user_id;
        let save_id = self.id.clone();
        let Some(mut save) = self.into_wire() else {
            return Ok(None);
        };
        let include = match &save.shared {
            Some(shared) if !is_owner && !shared.include.is_empty() => &shared.include,
            _ => return Ok(Some(save)),
        };
        // Summed per path in SQL, so the rows scale with distinct paths rather
        // than versions times files; the included total is the same sum.
        let files: Vec<(String, i64)> = sqlx::query_as(
            "SELECT sf.relative_path, SUM(sf.size_bytes)
             FROM snapshot_files sf
             JOIN snapshots sn ON sn.id = sf.snapshot_id
             WHERE sn.save_id = ? AND sn.deleted_at IS NULL
             GROUP BY sf.relative_path",
        )
        .bind(&save_id)
        .fetch_all(pool)
        .await?;
        let (_, size) = crate::routes::snapshots::included_totals(include, &files);
        save.total_size_bytes = Some(size);
        Ok(Some(save))
    }

    /// `None` only for a row whose id is not a UUID (see [`parse_save_id`]).
    fn into_wire(self) -> Option<Save> {
        let shared = match (
            self.group_id,
            self.group_name,
            self.group_owner_id,
            self.group_owner_name,
        ) {
            (Some(group_id), Some(group_name), Some(owner_user_id), Some(owner)) => {
                // A column that does not parse reads as everything, the same as
                // NULL: the share was validated on the way in, so this only
                // happens to a row edited by hand.
                let include = self
                    .include_json
                    .as_deref()
                    .and_then(|j| serde_json::from_str::<Vec<String>>(j).ok())
                    .unwrap_or_default();
                Some(SharedInfo {
                    group_id,
                    group_name,
                    owner_user_id,
                    owner_username: repair_username(&owner),
                    include,
                })
            }
            _ => None,
        };
        Some(Save {
            id: parse_save_id(&self.id)?,
            user_id: None,
            game_slug: repair_slug(&self.game_slug),
            label: self.label,
            local_path_hint: self.local_path_hint,
            client_os: self.client_os,
            latest_version_num: Some(self.latest_version_num),
            snapshot_count: Some(self.snapshot_count.unwrap_or(0)),
            total_size_bytes: Some(self.total_size_bytes.unwrap_or(0)),
            created_at: repair_ts(&self.created_at),
            updated_at: repair_ts(&self.updated_at),
            shared,
        })
    }
}

/// The save as the caller may see it: theirs, or shared into one of their
/// groups. A stranger gets `None`, the same as an unknown id.
pub(crate) async fn fetch_save(
    pool: &sqlx::SqlitePool,
    save_id: &str,
    user_id: &str,
) -> Result<Option<Save>, sqlx::Error> {
    let row = sqlx::query_as!(
        SaveRow,
        r#"SELECT s.id, s.user_id as owner_user_id, s.game_slug, s.label,
                  s.local_path_hint, s.client_os,
                  s.latest_version_num, s.created_at, s.updated_at,
                  COALESCE(COUNT(sn.id), 0) as "snapshot_count: i64",
                  COALESCE(SUM(sn.total_size_bytes), 0) as "total_size_bytes: i64",
                  ss.group_id as "group_id?", g.name as "group_name?",
                  g.owner_user_id as "group_owner_id?", u.username as "group_owner_name?",
                  ss.include_json as "include_json?"
           FROM saves s
           LEFT JOIN shared_saves ss ON ss.save_id = s.id
           LEFT JOIN groups g ON g.id = ss.group_id
           LEFT JOIN users u ON u.id = g.owner_user_id
           LEFT JOIN group_members gm ON gm.group_id = ss.group_id AND gm.user_id = ?
           LEFT JOIN snapshots sn ON sn.save_id = s.id AND sn.deleted_at IS NULL
           WHERE s.id = ? AND (s.user_id = ? OR gm.user_id IS NOT NULL)
           GROUP BY s.id"#,
        user_id,
        save_id,
        user_id
    )
    .fetch_optional(pool)
    .await?;
    match row {
        Some(row) => row.into_visible(pool, user_id).await,
        None => Ok(None),
    }
}

fn internal_err() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": "internal server error"})),
    )
}

/// Same 500 the client already gets, but the cause reaches the log.
///
/// Companion to [`internal_err`]. Mapping a fallible call with a closure that
/// ignores its argument silently deletes the one fact an operator needs; see
/// the note on `snapshots.rs::internal_logged` for what that cost.
fn internal_logged<E: std::fmt::Display>(
    what: &'static str,
    e: E,
) -> (StatusCode, Json<serde_json::Value>) {
    tracing::error!(error = %e, step = what, "saves request failed");
    internal_err()
}

/// Status-only sibling of [`internal_logged`], for the handlers in this module
/// that answer with a bare [`StatusCode`] instead of a JSON body. Same job:
/// the client learns nothing new, the operator learns everything.
fn internal_logged_status<E: std::fmt::Display>(what: &'static str, e: E) -> StatusCode {
    tracing::error!(error = %e, step = what, "saves request failed");
    StatusCode::INTERNAL_SERVER_ERROR
}
