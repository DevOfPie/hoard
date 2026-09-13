//! Where a save's bytes live: its owner's namespace, or its group's.
//!
//! Dedup has been per user since ADR 0018 so content existence never leaks
//! across accounts. A shared save cannot stay under its owner's key without
//! letting members probe the owner's other saves, so a group is a namespace of
//! its own (`group_blobs`/`group_chunks`, migration 0024), on disk at
//! `blobs/group/<group_id>/…`. The user layout is untouched: [`Namespace::User`]
//! delegates to [`crate::store::blob_key`] byte for byte.
//!
//! Every row helper here branches on the namespace once, so the upload, download
//! and purge paths stop writing each query twice. The group's owner pays: every
//! charge lands on [`Namespace::billing_user`] and is mirrored into
//! `groups.storage_used_bytes` in the same statement.

use sqlx::{Sqlite, SqliteConnection};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Namespace {
    User(String),
    Group(String),
}

fn shard(sha256: &str) -> &str {
    if sha256.len() >= 2 {
        &sha256[..2]
    } else {
        "00"
    }
}

impl Namespace {
    /// The namespace of a save from what `save_access` already knows: the group
    /// when it is shared, the owner otherwise.
    pub fn of(owner_user_id: &str, group_id: Option<&str>) -> Namespace {
        match group_id {
            Some(g) => Namespace::Group(g.to_string()),
            None => Namespace::User(owner_user_id.to_string()),
        }
    }

    /// `None` when the save does not exist. A writer resolves it again inside
    /// its `BEGIN IMMEDIATE` transaction: a share can land between a lookup on
    /// the pool and the write, and the rows must go where the save is now.
    pub async fn for_save<'e, E>(ex: E, save_id: &str) -> Result<Option<Namespace>, sqlx::Error>
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
        Ok(row.map(|r| Namespace::of(&r.user_id, r.group_id.as_deref())))
    }

    pub fn blob_key(&self, sha256: &str) -> String {
        match self {
            Namespace::User(id) => crate::store::blob_key(id, sha256),
            Namespace::Group(id) => format!("blobs/group/{id}/{}/{sha256}", shard(sha256)),
        }
    }

    pub fn chunk_key(&self, sha256: &str) -> String {
        match self {
            Namespace::User(id) => crate::store::chunk_key(id, sha256),
            Namespace::Group(id) => format!("chunks/group/{id}/{}/{sha256}", shard(sha256)),
        }
    }

    /// The directory prefix of one object kind, for the local backend's tidy-up.
    pub fn dir(&self, kind: &str) -> String {
        match self {
            Namespace::User(id) => format!("{kind}/{id}"),
            Namespace::Group(id) => format!("{kind}/group/{id}"),
        }
    }

    /// Who is charged for this namespace: the user, or the group's owner.
    pub async fn billing_user<'e, E>(&self, ex: E) -> Result<String, sqlx::Error>
    where
        E: sqlx::Executor<'e, Database = Sqlite>,
    {
        match self {
            Namespace::User(id) => Ok(id.clone()),
            Namespace::Group(id) => {
                sqlx::query_scalar!("SELECT owner_user_id FROM groups WHERE id = ?", id)
                    .fetch_one(ex)
                    .await
            }
        }
    }

    /// Move `delta` bytes of quota (negative to refund, floored at 0) onto
    /// `billing_user`, mirrored into the group's display counter.
    pub async fn charge(
        &self,
        conn: &mut SqliteConnection,
        billing_user: &str,
        delta: i64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "UPDATE users SET storage_used_bytes = MAX(0, storage_used_bytes + ?) WHERE id = ?",
            delta,
            billing_user
        )
        .execute(&mut *conn)
        .await?;
        if let Namespace::Group(id) = self {
            sqlx::query!(
                "UPDATE groups SET storage_used_bytes = MAX(0, storage_used_bytes + ?) WHERE id = ?",
                delta,
                id
            )
            .execute(&mut *conn)
            .await?;
        }
        Ok(())
    }
}

// ---- rows

pub async fn blob_size<'e, E>(ex: E, ns: &Namespace, sha: &str) -> Result<Option<i64>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    match ns {
        Namespace::User(id) => {
            sqlx::query_scalar!(
                "SELECT size_bytes FROM blobs WHERE user_id = ? AND sha256 = ?",
                id,
                sha
            )
            .fetch_optional(ex)
            .await
        }
        Namespace::Group(id) => {
            sqlx::query_scalar!(
                "SELECT size_bytes FROM group_blobs WHERE group_id = ? AND sha256 = ?",
                id,
                sha
            )
            .fetch_optional(ex)
            .await
        }
    }
}

pub async fn chunk_size<'e, E>(ex: E, ns: &Namespace, sha: &str) -> Result<Option<i64>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    match ns {
        Namespace::User(id) => {
            sqlx::query_scalar!(
                "SELECT size_bytes FROM chunks WHERE user_id = ? AND sha256 = ?",
                id,
                sha
            )
            .fetch_optional(ex)
            .await
        }
        Namespace::Group(id) => {
            sqlx::query_scalar!(
                "SELECT size_bytes FROM group_chunks WHERE group_id = ? AND sha256 = ?",
                id,
                sha
            )
            .fetch_optional(ex)
            .await
        }
    }
}

pub async fn chunk_exists<'e, E>(ex: E, ns: &Namespace, sha: &str) -> Result<bool, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    chunk_size(ex, ns, sha).await.map(|s| s.is_some())
}

pub async fn blob_refcount<'e, E>(
    ex: E,
    ns: &Namespace,
    sha: &str,
) -> Result<Option<i64>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    match ns {
        Namespace::User(id) => {
            sqlx::query_scalar!(
                "SELECT refcount FROM blobs WHERE user_id = ? AND sha256 = ?",
                id,
                sha
            )
            .fetch_optional(ex)
            .await
        }
        Namespace::Group(id) => {
            sqlx::query_scalar!(
                "SELECT refcount FROM group_blobs WHERE group_id = ? AND sha256 = ?",
                id,
                sha
            )
            .fetch_optional(ex)
            .await
        }
    }
}

pub async fn chunk_refcount<'e, E>(
    ex: E,
    ns: &Namespace,
    sha: &str,
) -> Result<Option<i64>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    match ns {
        Namespace::User(id) => {
            sqlx::query_scalar!(
                "SELECT refcount FROM chunks WHERE user_id = ? AND sha256 = ?",
                id,
                sha
            )
            .fetch_optional(ex)
            .await
        }
        Namespace::Group(id) => {
            sqlx::query_scalar!(
                "SELECT refcount FROM group_chunks WHERE group_id = ? AND sha256 = ?",
                id,
                sha
            )
            .fetch_optional(ex)
            .await
        }
    }
}

/// The upsert of 0013: insert at `by`, or bump an existing row by `by`.
pub async fn blob_incref(
    conn: &mut SqliteConnection,
    ns: &Namespace,
    sha: &str,
    size: i64,
    by: i64,
) -> Result<(), sqlx::Error> {
    match ns {
        Namespace::User(id) => {
            sqlx::query!(
                "INSERT INTO blobs (user_id, sha256, size_bytes, refcount) VALUES (?,?,?,?)
                 ON CONFLICT(user_id, sha256) DO UPDATE SET refcount = refcount + excluded.refcount",
                id,
                sha,
                size,
                by
            )
            .execute(conn)
            .await?;
        }
        Namespace::Group(id) => {
            sqlx::query!(
                "INSERT INTO group_blobs (group_id, sha256, size_bytes, refcount) VALUES (?,?,?,?)
                 ON CONFLICT(group_id, sha256) DO UPDATE SET refcount = refcount + excluded.refcount",
                id,
                sha,
                size,
                by
            )
            .execute(conn)
            .await?;
        }
    }
    Ok(())
}

/// The upsert of 0014, chunk twin of [`blob_incref`].
pub async fn chunk_incref(
    conn: &mut SqliteConnection,
    ns: &Namespace,
    sha: &str,
    size: i64,
    by: i64,
) -> Result<(), sqlx::Error> {
    match ns {
        Namespace::User(id) => {
            sqlx::query!(
                "INSERT INTO chunks (user_id, sha256, size_bytes, refcount) VALUES (?,?,?,?)
                 ON CONFLICT(user_id, sha256) DO UPDATE SET refcount = refcount + excluded.refcount",
                id,
                sha,
                size,
                by
            )
            .execute(conn)
            .await?;
        }
        Namespace::Group(id) => {
            sqlx::query!(
                "INSERT INTO group_chunks (group_id, sha256, size_bytes, refcount) VALUES (?,?,?,?)
                 ON CONFLICT(group_id, sha256) DO UPDATE SET refcount = refcount + excluded.refcount",
                id,
                sha,
                size,
                by
            )
            .execute(conn)
            .await?;
        }
    }
    Ok(())
}

/// Drop `by` references. `(refcount, size_bytes)` after the decrement, `None`
/// when there was no row (a chunked file's sha has none in `blobs`). The row
/// stays at 0 until [`blob_delete_row`], so the caller decides what to GC.
pub async fn blob_decref(
    conn: &mut SqliteConnection,
    ns: &Namespace,
    sha: &str,
    by: i64,
) -> Result<Option<(i64, i64)>, sqlx::Error> {
    match ns {
        Namespace::User(id) => {
            sqlx::query!(
                "UPDATE blobs SET refcount = refcount - ? WHERE user_id = ? AND sha256 = ?",
                by,
                id,
                sha
            )
            .execute(&mut *conn)
            .await?;
            let r = sqlx::query!(
                "SELECT refcount, size_bytes FROM blobs WHERE user_id = ? AND sha256 = ?",
                id,
                sha
            )
            .fetch_optional(conn)
            .await?;
            Ok(r.map(|r| (r.refcount, r.size_bytes)))
        }
        Namespace::Group(id) => {
            sqlx::query!(
                "UPDATE group_blobs SET refcount = refcount - ? WHERE group_id = ? AND sha256 = ?",
                by,
                id,
                sha
            )
            .execute(&mut *conn)
            .await?;
            let r = sqlx::query!(
                "SELECT refcount, size_bytes FROM group_blobs WHERE group_id = ? AND sha256 = ?",
                id,
                sha
            )
            .fetch_optional(conn)
            .await?;
            Ok(r.map(|r| (r.refcount, r.size_bytes)))
        }
    }
}

pub async fn chunk_decref(
    conn: &mut SqliteConnection,
    ns: &Namespace,
    sha: &str,
    by: i64,
) -> Result<Option<(i64, i64)>, sqlx::Error> {
    match ns {
        Namespace::User(id) => {
            sqlx::query!(
                "UPDATE chunks SET refcount = refcount - ? WHERE user_id = ? AND sha256 = ?",
                by,
                id,
                sha
            )
            .execute(&mut *conn)
            .await?;
            let r = sqlx::query!(
                "SELECT refcount, size_bytes FROM chunks WHERE user_id = ? AND sha256 = ?",
                id,
                sha
            )
            .fetch_optional(conn)
            .await?;
            Ok(r.map(|r| (r.refcount, r.size_bytes)))
        }
        Namespace::Group(id) => {
            sqlx::query!(
                "UPDATE group_chunks SET refcount = refcount - ? WHERE group_id = ? AND sha256 = ?",
                by,
                id,
                sha
            )
            .execute(&mut *conn)
            .await?;
            let r = sqlx::query!(
                "SELECT refcount, size_bytes FROM group_chunks WHERE group_id = ? AND sha256 = ?",
                id,
                sha
            )
            .fetch_optional(conn)
            .await?;
            Ok(r.map(|r| (r.refcount, r.size_bytes)))
        }
    }
}

pub async fn blob_delete_row(
    conn: &mut SqliteConnection,
    ns: &Namespace,
    sha: &str,
) -> Result<(), sqlx::Error> {
    match ns {
        Namespace::User(id) => {
            sqlx::query!(
                "DELETE FROM blobs WHERE user_id = ? AND sha256 = ?",
                id,
                sha
            )
            .execute(conn)
            .await?;
        }
        Namespace::Group(id) => {
            sqlx::query!(
                "DELETE FROM group_blobs WHERE group_id = ? AND sha256 = ?",
                id,
                sha
            )
            .execute(conn)
            .await?;
        }
    }
    Ok(())
}

pub async fn chunk_delete_row(
    conn: &mut SqliteConnection,
    ns: &Namespace,
    sha: &str,
) -> Result<(), sqlx::Error> {
    match ns {
        Namespace::User(id) => {
            sqlx::query!(
                "DELETE FROM chunks WHERE user_id = ? AND sha256 = ?",
                id,
                sha
            )
            .execute(conn)
            .await?;
        }
        Namespace::Group(id) => {
            sqlx::query!(
                "DELETE FROM group_chunks WHERE group_id = ? AND sha256 = ?",
                id,
                sha
            )
            .execute(conn)
            .await?;
        }
    }
    Ok(())
}

/// The ordered chunk list of one `snapshot_files` row, each with its size from
/// this namespace's chunk table (0 when the row is missing).
pub async fn chunk_list<'e, E>(
    ex: E,
    ns: &Namespace,
    snapshot_file_id: &str,
) -> Result<Vec<(String, i64)>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    match ns {
        Namespace::User(id) => sqlx::query!(
            r#"SELECT c.chunk_sha256 AS sha, COALESCE(k.size_bytes, 0) AS "size!: i64"
               FROM snapshot_file_chunks c
               LEFT JOIN chunks k ON k.user_id = ? AND k.sha256 = c.chunk_sha256
               WHERE c.snapshot_file_id = ?
               ORDER BY c.ordinal"#,
            id,
            snapshot_file_id
        )
        .fetch_all(ex)
        .await
        .map(|rows| rows.into_iter().map(|r| (r.sha, r.size)).collect()),
        Namespace::Group(id) => sqlx::query!(
            r#"SELECT c.chunk_sha256 AS sha, COALESCE(k.size_bytes, 0) AS "size!: i64"
               FROM snapshot_file_chunks c
               LEFT JOIN group_chunks k ON k.group_id = ? AND k.sha256 = c.chunk_sha256
               WHERE c.snapshot_file_id = ?
               ORDER BY c.ordinal"#,
            id,
            snapshot_file_id
        )
        .fetch_all(ex)
        .await
        .map(|rows| rows.into_iter().map(|r| (r.sha, r.size)).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_keys_are_the_store_keys_and_group_keys_sit_apart() {
        let sha = "ab".to_string() + &"0".repeat(62);
        let user = Namespace::User("u1".into());
        assert_eq!(user.blob_key(&sha), crate::store::blob_key("u1", &sha));
        assert_eq!(user.chunk_key(&sha), crate::store::chunk_key("u1", &sha));
        let group = Namespace::Group("g1".into());
        assert_eq!(group.blob_key(&sha), format!("blobs/group/g1/ab/{sha}"));
        assert_eq!(group.chunk_key(&sha), format!("chunks/group/g1/ab/{sha}"));
        assert_eq!(group.dir("blobs"), "blobs/group/g1");
    }
}
