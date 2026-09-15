//! Who may touch a save: its owner, or a member of the group it is shared into.
//!
//! One query answers it, `saves ⋈ shared_saves ⋈ group_members` on the caller.
//! Every read handler calls [`save_access`]; the write handlers still go through
//! `snapshots::ownership_check`, which is this with the answer narrowed to
//! [`Role::Owner`]. A stranger gets `None`, and the route turns that into the
//! same 404 an unknown id gets, so existence does not leak either.

use sqlx::SqlitePool;

/// The caller's standing on a save.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// `saves.user_id` is the caller.
    Owner,
    /// The caller belongs to the group the save is shared into.
    Member,
}

#[derive(Debug, Clone)]
pub struct SaveAccess {
    pub game_slug: String,
    pub label: String,
    pub owner_user_id: String,
    pub role: Role,
    /// The group the save is shared into, if any. Set for the owner too.
    pub group_id: Option<String>,
}

pub async fn save_access(
    pool: &SqlitePool,
    save_id: &str,
    user_id: &str,
) -> Result<Option<SaveAccess>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT s.game_slug, s.label, s.user_id AS owner_user_id,
                  ss.group_id AS "group_id?", gm.user_id AS "member_id?"
           FROM saves s
           LEFT JOIN shared_saves ss ON ss.save_id = s.id
           LEFT JOIN group_members gm ON gm.group_id = ss.group_id AND gm.user_id = ?
           WHERE s.id = ? AND (s.user_id = ? OR gm.user_id IS NOT NULL)"#,
        user_id,
        save_id,
        user_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| {
        let role = if r.owner_user_id == user_id {
            Role::Owner
        } else {
            Role::Member
        };
        SaveAccess {
            game_slug: r.game_slug,
            label: r.label,
            owner_user_id: r.owner_user_id,
            role,
            group_id: r.group_id,
        }
    }))
}

/// What of the save the caller may read, as an include list: empty for the
/// whole save. The owner reads everything; a member reads what the share names,
/// on every version, including the ones uploaded before the share existed.
pub async fn read_include<'e, E>(
    ex: E,
    save_id: &str,
    access: &SaveAccess,
) -> Result<Vec<String>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    if access.role == Role::Owner {
        return Ok(Vec::new());
    }
    crate::routes::share::include_for(ex, save_id).await
}
