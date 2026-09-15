//! Groups and who gets to read a shared save, end to end (`routes::groups`,
//! `routes::access`).
//!
//! Same approach as `devices_presence.rs`: the real handlers against a real
//! database, with the accounts created the way `admin_users.rs` creates them.
//! What it pins down is the membership lifecycle (create, invite, join, leave,
//! delete) and the one rule the rest of sharing rests on: a member reads, only
//! the owner writes, and a stranger is told nothing exists.

use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use hoard_core::ids::GameSlug;
use hoard_core::wire::{
    CreateGroupRequest, CreateInviteRequest, CreateSaveRequest, Group, InviteOut, JoinGroupRequest,
    PatchSaveRequest, Save,
};
use hoard_server::auth::AuthUser;
use hoard_server::routes::access::{save_access, Role};
use hoard_server::routes::health::ServerState;
use hoard_server::routes::{admin, groups, saves, snapshots};
use sqlx::Row;
use std::sync::Arc;
use std::time::Instant;
use uuid::Uuid;

const ADMIN_ID: &str = "11111111-2222-4333-8444-555555555555";

struct Harness {
    state: Arc<ServerState>,
    admin: AuthUser,
    _dir: tempfile::TempDir,
}

async fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().to_path_buf();
    let cfg_path = data_dir.join("config.toml");
    // A `\` inside a TOML basic string is an escape sequence, so a Windows path
    // written verbatim fails to parse before the first test body runs.
    let toml_path = |p: &std::path::Path| p.display().to_string().replace('\\', "/");
    std::fs::write(
        &cfg_path,
        format!(
            r#"
[server]
host = "127.0.0.1"
port = 12421
public_url = "http://localhost:12421"

[storage]
data_dir = "{data}"
max_snapshot_size_mb = 64
upload_timeout_secs = 600

[database]
url = "sqlite://{db}"
max_connections = 1

[auth]
token_lifetime_days = 365
allow_registration = true

[retention]
trash_retention_days = 30
tmp_cleanup_hours = 24

[logging]
level = "warn"
format = "pretty"
"#,
            data = toml_path(&data_dir),
            db = toml_path(&data_dir.join("hoard.db")),
        ),
    )
    .unwrap();

    let config = hoard_server::config::Config::load(&cfg_path).unwrap();
    let pool = hoard_server::db::connect(&config.database.url, 1)
        .await
        .unwrap();
    hoard_server::db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, is_admin) VALUES (?,'root','x',1)",
    )
    .bind(ADMIN_ID)
    .execute(&pool)
    .await
    .unwrap();
    let store = hoard_server::store::build_store(&config).await.unwrap();

    Harness {
        state: Arc::new(ServerState {
            trusted_proxies: Default::default(),
            pool,
            config,
            start_time: Instant::now(),
            store,
            events: Default::default(),
        }),
        admin: AuthUser {
            user_id: Uuid::parse_str(ADMIN_ID).unwrap(),
            username: "root".into(),
            is_admin: true,
        },
        _dir: dir,
    }
}

/// An ordinary account, created through the admin route like a real one.
async fn user(h: &Harness, username: &str) -> AuthUser {
    let (_, Json(u)) = admin::create_user(
        Extension(h.admin.clone()),
        State(h.state.clone()),
        Json(admin::NewUser {
            username: username.into(),
            password: "hunter2hunter2".into(),
            is_admin: false,
            storage_quota_bytes: None,
        }),
    )
    .await
    .expect("user created");
    AuthUser {
        user_id: Uuid::parse_str(&u.id).unwrap(),
        username: username.into(),
        is_admin: false,
    }
}

fn st(h: &Harness) -> State<Arc<ServerState>> {
    State(h.state.clone())
}

fn uid(u: &AuthUser) -> String {
    u.user_id.to_string()
}

async fn create_group(h: &Harness, who: &AuthUser, name: &str) -> Result<Group, StatusCode> {
    groups::create(
        st(h),
        Extension(who.clone()),
        Json(CreateGroupRequest { name: name.into() }),
    )
    .await
    .map(|(_, Json(g))| g)
    .map_err(|(code, _)| code)
}

async fn list_groups(h: &Harness, who: &AuthUser) -> Vec<Group> {
    groups::list(st(h), Extension(who.clone()))
        .await
        .expect("listed")
        .0
}

async fn delete_group(h: &Harness, who: &AuthUser, id: &str) -> Result<StatusCode, StatusCode> {
    groups::delete(st(h), Extension(who.clone()), Path(id.to_string()))
        .await
        .map_err(|(code, _)| code)
}

async fn invite(
    h: &Harness,
    who: &AuthUser,
    id: &str,
    ttl: Option<u64>,
) -> Result<InviteOut, StatusCode> {
    groups::create_invite(
        st(h),
        Extension(who.clone()),
        Path(id.to_string()),
        Json(CreateInviteRequest {
            expires_in_secs: ttl,
        }),
    )
    .await
    .map(|(_, Json(i))| i)
    .map_err(|(code, _)| code)
}

async fn join(h: &Harness, who: &AuthUser, token: &str) -> Result<Group, StatusCode> {
    groups::join(
        st(h),
        Extension(who.clone()),
        Json(JoinGroupRequest {
            token: token.into(),
        }),
    )
    .await
    .map(|Json(g)| g)
    .map_err(|(code, _)| code)
}

async fn remove(
    h: &Harness,
    who: &AuthUser,
    id: &str,
    target: &AuthUser,
) -> Result<StatusCode, StatusCode> {
    groups::remove_member(
        st(h),
        Extension(who.clone()),
        Path((id.to_string(), uid(target))),
    )
    .await
    .map_err(|(code, _)| code)
}

async fn create_save(h: &Harness, who: &AuthUser, label: &str) -> Save {
    saves::create(
        st(h),
        Extension(who.clone()),
        Json(CreateSaveRequest {
            game_slug: GameSlug::parse("stardew-valley").unwrap(),
            label: Some(label.into()),
            local_path_hint: None,
            client_os: None,
            display_name: None,
            steam_app_id: None,
        }),
    )
    .await
    .map(|(_, Json(s))| s)
    .map_err(|(code, _)| code)
    .expect("save created")
}

async fn list_saves(h: &Harness, who: &AuthUser) -> Vec<Save> {
    saves::list(
        st(h),
        Extension(who.clone()),
        Query(saves::ListQuery { game_slug: None }),
    )
    .await
    .expect("listed")
    .0
}

async fn get_save(h: &Harness, who: &AuthUser, id: &str) -> Result<Save, StatusCode> {
    saves::get_one(st(h), Extension(who.clone()), Path(id.to_string()))
        .await
        .map(|Json(s)| s)
}

/// Attach a save to a group the way step 2's share route will, minus the blob
/// move, which nothing here reads.
async fn share_by_sql(h: &Harness, save_id: &str, group_id: &str) {
    sqlx::query("INSERT INTO shared_saves (save_id, group_id) VALUES (?, ?)")
        .bind(save_id)
        .bind(group_id)
        .execute(&h.state.pool)
        .await
        .unwrap();
}

/// A group with an owner and one joined member, the fixture most tests start from.
async fn owner_and_member(h: &Harness) -> (AuthUser, AuthUser, Group) {
    let owner = user(h, "owner").await;
    let member = user(h, "member").await;
    let g = create_group(h, &owner, "valheim-crew").await.unwrap();
    let inv = invite(h, &owner, &g.id, None).await.unwrap();
    let g = join(h, &member, &inv.token).await.unwrap();
    (owner, member, g)
}

#[tokio::test]
async fn the_creator_is_the_owner_and_its_first_member() {
    let h = harness().await;
    let owner = user(&h, "owner").await;
    let g = create_group(&h, &owner, "  valheim-crew ").await.unwrap();
    assert_eq!(g.name, "valheim-crew", "trimmed");
    assert_eq!(g.owner_user_id, uid(&owner));
    assert_eq!(g.members.len(), 1);
    assert_eq!(g.members[0].user_id, uid(&owner));
    assert_eq!(g.members[0].role, "owner");
    assert_eq!(g.members[0].username.as_str(), "owner");

    let mine = list_groups(&h, &owner).await;
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].id, g.id);

    assert_eq!(
        create_group(&h, &owner, "   ").await,
        Err(StatusCode::BAD_REQUEST)
    );
    assert_eq!(
        create_group(&h, &owner, &"x".repeat(65)).await,
        Err(StatusCode::BAD_REQUEST)
    );
}

#[tokio::test]
async fn an_invite_redeemed_makes_a_member() {
    let h = harness().await;
    let owner = user(&h, "owner").await;
    let member = user(&h, "member").await;
    let g = create_group(&h, &owner, "valheim-crew").await.unwrap();

    let inv = invite(&h, &owner, &g.id, None).await.unwrap();
    assert!(!inv.token.is_empty());
    // Only the hash is kept; the token itself never touches the database.
    let stored: String = sqlx::query("SELECT token_hash FROM group_invites WHERE id = ?")
        .bind(&inv.invite_id)
        .fetch_one(&h.state.pool)
        .await
        .unwrap()
        .get("token_hash");
    assert_ne!(stored, inv.token);
    assert_eq!(stored, hoard_core::hashing::hash_token(&inv.token));

    let joined = join(&h, &member, &inv.token).await.unwrap();
    assert_eq!(joined.id, g.id);
    assert_eq!(joined.members.len(), 2);
    let me = joined
        .members
        .iter()
        .find(|m| m.user_id == uid(&member))
        .expect("listed as a member");
    assert_eq!(me.role, "member");

    let used_by: Option<String> = sqlx::query("SELECT used_by FROM group_invites WHERE id = ?")
        .bind(&inv.invite_id)
        .fetch_one(&h.state.pool)
        .await
        .unwrap()
        .get("used_by");
    assert_eq!(used_by.as_deref(), Some(uid(&member).as_str()));

    let mine = list_groups(&h, &member).await;
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].members.len(), 2);

    // Only the owner mints invites.
    assert_eq!(
        invite(&h, &member, &g.id, None).await,
        Err(StatusCode::FORBIDDEN)
    );
}

#[tokio::test]
async fn a_used_or_expired_token_opens_nothing() {
    let h = harness().await;
    let owner = user(&h, "owner").await;
    let member = user(&h, "member").await;
    let third = user(&h, "third").await;
    let g = create_group(&h, &owner, "valheim-crew").await.unwrap();

    // Used: one link admits one person.
    let inv = invite(&h, &owner, &g.id, None).await.unwrap();
    join(&h, &member, &inv.token).await.unwrap();
    assert_eq!(
        join(&h, &third, &inv.token).await,
        Err(StatusCode::NOT_FOUND)
    );

    // Expired.
    let inv = invite(&h, &owner, &g.id, Some(60)).await.unwrap();
    sqlx::query("UPDATE group_invites SET expires_at = '2000-01-01T00:00:00Z' WHERE id = ?")
        .bind(&inv.invite_id)
        .execute(&h.state.pool)
        .await
        .unwrap();
    assert_eq!(
        join(&h, &third, &inv.token).await,
        Err(StatusCode::NOT_FOUND)
    );

    // Made up.
    assert_eq!(
        join(&h, &third, "not-a-token").await,
        Err(StatusCode::NOT_FOUND)
    );
    assert!(list_groups(&h, &third).await.is_empty());
}

#[tokio::test]
async fn joining_twice_changes_nothing() {
    let h = harness().await;
    let owner = user(&h, "owner").await;
    let member = user(&h, "member").await;
    let g = create_group(&h, &owner, "valheim-crew").await.unwrap();
    let inv = invite(&h, &owner, &g.id, None).await.unwrap();

    let first = join(&h, &member, &inv.token).await.unwrap();
    let again = join(&h, &member, &inv.token).await.unwrap();
    assert_eq!(first.id, again.id);
    assert_eq!(again.members.len(), 2);

    // The owner redeeming their own link is equally harmless.
    let inv2 = invite(&h, &owner, &g.id, None).await.unwrap();
    assert_eq!(
        join(&h, &owner, &inv2.token).await.unwrap().members.len(),
        2
    );
}

#[tokio::test]
async fn a_stranger_is_told_the_group_does_not_exist() {
    let h = harness().await;
    let (owner, member, g) = owner_and_member(&h).await;
    let stranger = user(&h, "stranger").await;

    assert!(list_groups(&h, &stranger).await.is_empty());
    assert_eq!(
        delete_group(&h, &stranger, &g.id).await,
        Err(StatusCode::NOT_FOUND)
    );
    assert_eq!(
        invite(&h, &stranger, &g.id, None).await,
        Err(StatusCode::NOT_FOUND)
    );
    assert_eq!(
        remove(&h, &stranger, &g.id, &member).await,
        Err(StatusCode::NOT_FOUND)
    );
    assert_eq!(
        remove(&h, &stranger, &g.id, &owner).await,
        Err(StatusCode::NOT_FOUND)
    );
    assert_eq!(
        delete_group(&h, &stranger, "no-such-group").await,
        Err(StatusCode::NOT_FOUND)
    );
}

#[tokio::test]
async fn the_owner_stays_and_members_may_leave_or_be_removed() {
    let h = harness().await;
    let (owner, member, g) = owner_and_member(&h).await;
    let inv = invite(&h, &owner, &g.id, None).await.unwrap();
    let other = user(&h, "other").await;
    join(&h, &other, &inv.token).await.unwrap();

    // The owner cannot leave, whoever asks.
    assert_eq!(
        remove(&h, &owner, &g.id, &owner).await,
        Err(StatusCode::CONFLICT)
    );
    assert_eq!(
        remove(&h, &member, &g.id, &owner).await,
        Err(StatusCode::FORBIDDEN)
    );
    // A member removes only themself.
    assert_eq!(
        remove(&h, &member, &g.id, &other).await,
        Err(StatusCode::FORBIDDEN)
    );
    assert_eq!(
        remove(&h, &member, &g.id, &member).await,
        Ok(StatusCode::NO_CONTENT)
    );
    assert!(list_groups(&h, &member).await.is_empty());

    // The owner removes anybody.
    assert_eq!(
        remove(&h, &owner, &g.id, &other).await,
        Ok(StatusCode::NO_CONTENT)
    );
    assert_eq!(
        remove(&h, &owner, &g.id, &other).await,
        Err(StatusCode::NOT_FOUND),
        "already gone"
    );
    let left = list_groups(&h, &owner).await;
    assert_eq!(left[0].members.len(), 1);

    // Only the owner deletes.
    let inv = invite(&h, &owner, &g.id, None).await.unwrap();
    join(&h, &member, &inv.token).await.unwrap();
    assert_eq!(
        delete_group(&h, &member, &g.id).await,
        Err(StatusCode::FORBIDDEN)
    );
}

#[tokio::test]
async fn a_group_with_shared_saves_refuses_to_die() {
    let h = harness().await;
    let (owner, member, g) = owner_and_member(&h).await;
    let save = create_save(&h, &owner, "farm").await;
    share_by_sql(&h, save.id.as_str(), &g.id).await;

    assert_eq!(
        delete_group(&h, &owner, &g.id).await,
        Err(StatusCode::CONFLICT)
    );

    sqlx::query("DELETE FROM shared_saves WHERE save_id = ?")
        .bind(save.id.as_str())
        .execute(&h.state.pool)
        .await
        .unwrap();
    assert_eq!(
        delete_group(&h, &owner, &g.id).await,
        Ok(StatusCode::NO_CONTENT)
    );
    assert!(list_groups(&h, &owner).await.is_empty());
    assert!(list_groups(&h, &member).await.is_empty());
    // The save itself is untouched.
    assert!(get_save(&h, &owner, save.id.as_str()).await.is_ok());
}

#[tokio::test]
async fn save_access_tells_owner_member_and_stranger_apart() {
    let h = harness().await;
    let (owner, member, g) = owner_and_member(&h).await;
    let stranger = user(&h, "stranger").await;
    let save = create_save(&h, &owner, "farm").await;
    let pool = &h.state.pool;

    // Unshared: the owner, nobody else.
    let a = save_access(pool, save.id.as_str(), &uid(&owner))
        .await
        .unwrap()
        .expect("owner");
    assert_eq!(a.role, Role::Owner);
    assert_eq!(a.group_id, None);
    assert_eq!(a.owner_user_id, uid(&owner));
    assert_eq!(a.game_slug, "stardew-valley");
    assert_eq!(a.label, "farm");
    assert!(save_access(pool, save.id.as_str(), &uid(&member))
        .await
        .unwrap()
        .is_none());

    share_by_sql(&h, save.id.as_str(), &g.id).await;

    let a = save_access(pool, save.id.as_str(), &uid(&owner))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(a.role, Role::Owner);
    assert_eq!(a.group_id.as_deref(), Some(g.id.as_str()));

    let a = save_access(pool, save.id.as_str(), &uid(&member))
        .await
        .unwrap()
        .expect("member");
    assert_eq!(a.role, Role::Member);
    assert_eq!(a.owner_user_id, uid(&owner));
    assert_eq!(a.group_id.as_deref(), Some(g.id.as_str()));

    assert!(save_access(pool, save.id.as_str(), &uid(&stranger))
        .await
        .unwrap()
        .is_none());
    assert!(save_access(pool, "no-such-save", &uid(&owner))
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn a_shared_save_is_listed_for_the_member_and_marked_for_both() {
    let h = harness().await;
    let (owner, member, g) = owner_and_member(&h).await;
    let shared = create_save(&h, &owner, "farm").await;
    let private = create_save(&h, &owner, "private").await;
    let own = create_save(&h, &member, "mine").await;
    share_by_sql(&h, shared.id.as_str(), &g.id).await;

    let seen = list_saves(&h, &member).await;
    assert_eq!(
        seen.len(),
        2,
        "their own plus the shared one, not the private one"
    );
    let s = seen
        .iter()
        .find(|s| s.id == shared.id)
        .expect("shared listed");
    let info = s.shared.as_ref().expect("marked shared");
    assert_eq!(info.group_id, g.id);
    assert_eq!(info.group_name, "valheim-crew");
    assert_eq!(info.owner_user_id, uid(&owner));
    assert_eq!(info.owner_username.as_str(), "owner");
    let mine = seen.iter().find(|s| s.id == own.id).unwrap();
    assert!(mine.shared.is_none());
    assert!(!seen.iter().any(|s| s.id == private.id));

    let owners = list_saves(&h, &owner).await;
    assert_eq!(owners.len(), 2);
    let s = owners.iter().find(|s| s.id == shared.id).unwrap();
    assert_eq!(
        s.shared.as_ref().map(|i| i.group_id.as_str()),
        Some(g.id.as_str())
    );
    assert!(owners
        .iter()
        .find(|s| s.id == private.id)
        .unwrap()
        .shared
        .is_none());

    // The slug filter keeps the same reach.
    let filtered = saves::list(
        st(&h),
        Extension(member.clone()),
        Query(saves::ListQuery {
            game_slug: Some("stardew-valley".into()),
        }),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(filtered.len(), 2);

    // Unshared again: it leaves the member's list and loses its mark.
    sqlx::query("DELETE FROM shared_saves WHERE save_id = ?")
        .bind(shared.id.as_str())
        .execute(&h.state.pool)
        .await
        .unwrap();
    assert_eq!(list_saves(&h, &member).await.len(), 1);
    assert!(get_save(&h, &owner, shared.id.as_str())
        .await
        .unwrap()
        .shared
        .is_none());
}

#[tokio::test]
async fn a_member_reads_the_save_and_a_stranger_finds_nothing() {
    let h = harness().await;
    let (owner, member, g) = owner_and_member(&h).await;
    let stranger = user(&h, "stranger").await;
    let save = create_save(&h, &owner, "farm").await;
    share_by_sql(&h, save.id.as_str(), &g.id).await;
    sqlx::query(
        "INSERT INTO snapshots (id, save_id, version_num, device_name) VALUES (?, ?, 1, 'pc')",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(save.id.as_str())
    .execute(&h.state.pool)
    .await
    .unwrap();

    let sid = save.id.as_str();
    let got = get_save(&h, &member, sid).await.expect("member reads");
    assert_eq!(got.shared.as_ref().unwrap().group_id, g.id);
    assert_eq!(
        get_save(&h, &stranger, sid).await.map(|_| ()),
        Err(StatusCode::NOT_FOUND)
    );

    let page = snapshots::ListQuery {
        include_deleted: false,
        limit: 50,
        offset: 0,
    };
    let listed = snapshots::list(
        st(&h),
        Extension(member.clone()),
        Path(sid.to_string()),
        Query(page),
    )
    .await
    .map_err(|(code, _)| code)
    .expect("member lists versions")
    .0;
    assert_eq!(listed.len(), 1);
    assert_eq!(
        snapshots::list(
            st(&h),
            Extension(stranger.clone()),
            Path(sid.to_string()),
            Query(snapshots::ListQuery {
                include_deleted: false,
                limit: 50,
                offset: 0,
            }),
        )
        .await
        .map(|_| ())
        .map_err(|(code, _)| code),
        Err(StatusCode::NOT_FOUND)
    );

    let detail = snapshots::detail(
        st(&h),
        Extension(member.clone()),
        Path((sid.to_string(), 1)),
    )
    .await
    .map_err(|(code, _)| code)
    .expect("member reads a version")
    .0;
    assert_eq!(detail.snapshot.version_num, 1);
    assert_eq!(
        snapshots::detail(
            st(&h),
            Extension(stranger.clone()),
            Path((sid.to_string(), 1)),
        )
        .await
        .map(|_| ())
        .map_err(|(code, _)| code),
        Err(StatusCode::NOT_FOUND)
    );
}

#[tokio::test]
async fn a_member_cannot_rename_or_delete_the_save() {
    let h = harness().await;
    let (owner, member, g) = owner_and_member(&h).await;
    let save = create_save(&h, &owner, "farm").await;
    share_by_sql(&h, save.id.as_str(), &g.id).await;
    let sid = save.id.as_str();

    let patched = saves::patch(
        st(&h),
        Extension(member.clone()),
        Path(sid.to_string()),
        Json(PatchSaveRequest {
            label: Some("stolen".into()),
            ..Default::default()
        }),
    )
    .await
    .map(|_| ())
    .map_err(|(code, _)| code);
    assert_eq!(patched, Err(StatusCode::NOT_FOUND));

    assert_eq!(
        saves::delete(st(&h), Extension(member.clone()), Path(sid.to_string())).await,
        Err(StatusCode::NOT_FOUND)
    );

    // Still there, still named as the owner left it.
    let still = get_save(&h, &owner, sid).await.unwrap();
    assert_eq!(still.label, "farm");
}

// ---- invites: bounds, the guard, and the redeemer's deletion

/// A year is the most an invite lives; more used to overflow the stamp.
#[tokio::test]
async fn an_invite_ttl_past_a_year_is_400() {
    let h = harness().await;
    let owner = user(&h, "owner").await;
    let g = create_group(&h, &owner, "valheim-crew").await.unwrap();
    assert_eq!(
        invite(&h, &owner, &g.id, Some(u64::MAX)).await,
        Err(StatusCode::BAD_REQUEST)
    );
    assert_eq!(
        invite(&h, &owner, &g.id, Some(366 * 24 * 60 * 60)).await,
        Err(StatusCode::BAD_REQUEST)
    );
    assert!(invite(&h, &owner, &g.id, Some(365 * 24 * 60 * 60))
        .await
        .is_ok());
}

/// The UPDATE that spends a token is guarded on `used_at`: a row already
/// marked used, by whatever path, admits nobody and is not rewritten.
#[tokio::test]
async fn a_spent_invite_is_not_spent_again() {
    let h = harness().await;
    let owner = user(&h, "owner").await;
    let member = user(&h, "member").await;
    let g = create_group(&h, &owner, "valheim-crew").await.unwrap();
    let inv = invite(&h, &owner, &g.id, None).await.unwrap();
    sqlx::query("UPDATE group_invites SET used_at = '2000-01-01T00:00:00Z' WHERE id = ?")
        .bind(&inv.invite_id)
        .execute(&h.state.pool)
        .await
        .unwrap();

    assert_eq!(
        join(&h, &member, &inv.token).await,
        Err(StatusCode::NOT_FOUND)
    );
    let used_by: Option<String> = sqlx::query("SELECT used_by FROM group_invites WHERE id = ?")
        .bind(&inv.invite_id)
        .fetch_one(&h.state.pool)
        .await
        .unwrap()
        .get("used_by");
    assert_eq!(used_by, None, "the guarded UPDATE changed nothing");
    assert!(list_groups(&h, &member).await.is_empty());
}

/// Deleting the account that redeemed an invite used to fail on the foreign
/// key (0022 had no ON DELETE); 0026 clears `used_by` and lets it through.
#[tokio::test]
async fn deleting_a_user_who_redeemed_an_invite_clears_used_by() {
    let h = harness().await;
    let owner = user(&h, "owner").await;
    let member = user(&h, "member").await;
    let g = create_group(&h, &owner, "valheim-crew").await.unwrap();
    let inv = invite(&h, &owner, &g.id, None).await.unwrap();
    join(&h, &member, &inv.token).await.unwrap();

    let Json(_) = admin::delete_user(
        Extension(h.admin.clone()),
        State(h.state.clone()),
        Path(uid(&member)),
    )
    .await
    .expect("the redeemer can be deleted");
    let used_by: Option<String> = sqlx::query("SELECT used_by FROM group_invites WHERE id = ?")
        .bind(&inv.invite_id)
        .fetch_one(&h.state.pool)
        .await
        .unwrap()
        .get("used_by");
    assert_eq!(used_by, None);
    assert_eq!(list_groups(&h, &owner).await[0].members.len(), 1);

    // The minter goes with their invites.
    let Json(_) = admin::delete_user(
        Extension(h.admin.clone()),
        State(h.state.clone()),
        Path(uid(&owner)),
    )
    .await
    .expect("the minter can be deleted");
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM group_invites")
        .fetch_one(&h.state.pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
}
