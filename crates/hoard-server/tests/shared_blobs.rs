//! A save shared into a group: where its bytes go and who pays (`routes::share`,
//! `namespace`).
//!
//! The `cas_roundtrip.rs` approach: the real handlers against a real database
//! and a real store. Three accounts: the save's owner, the group's owner (who
//! pays), and a plain member, so a charge that lands on the wrong one shows.

use axum::body::Body;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::Json;
use hoard_core::ids::Sha256 as Sha256Hex;
use hoard_core::wire::{
    CasCommit, CasFile, CasInit, CreateGroupRequest, CreateInviteRequest, JoinGroupRequest,
    LeaseAcquireRequest, Save, ShareSaveRequest,
};
use hoard_server::auth::AuthUser;
use hoard_server::routes::health::ServerState;
use hoard_server::routes::{admin, cas, groups, leases, share, snapshots};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use std::sync::Arc;
use std::time::Instant;
use uuid::Uuid;

const OWNER: &str = "11111111-2222-4333-8444-555555555555";
const PAYER: &str = "22222222-2222-4333-8444-555555555555";
const MEMBER: &str = "33333333-2222-4333-8444-555555555555";
const STRANGER: &str = "44444444-2222-4333-8444-555555555555";
const ADMIN: &str = "55555555-2222-4333-8444-555555555555";
const SAVE: &str = "66666666-7777-4888-8999-aaaaaaaaaaaa";

struct Harness {
    state: Arc<ServerState>,
    owner: AuthUser,
    payer: AuthUser,
    member: AuthUser,
    stranger: AuthUser,
    admin: AuthUser,
    _dir: tempfile::TempDir,
}

fn auth(id: &str, name: &str, is_admin: bool) -> AuthUser {
    AuthUser {
        user_id: Uuid::parse_str(id).unwrap(),
        username: name.into(),
        is_admin,
    }
}

async fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().to_path_buf();
    let db_path = data_dir.join("hoard.db");
    let cfg_path = data_dir.join("config.toml");
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
            db = toml_path(&db_path),
        ),
    )
    .unwrap();

    let config = hoard_server::config::Config::load(&cfg_path).expect("test config loads");
    let pool = hoard_server::db::connect(&config.database.url, 1)
        .await
        .expect("test database");
    hoard_server::db::run_migrations(&pool)
        .await
        .expect("migrations");
    seed(&pool).await;
    let store = hoard_server::store::build_store(&config)
        .await
        .expect("local store");

    Harness {
        state: Arc::new(ServerState {
            trusted_proxies: Default::default(),
            pool,
            config,
            start_time: Instant::now(),
            store,
            events: Default::default(),
        }),
        owner: auth(OWNER, "jacka", false),
        payer: auth(PAYER, "sam", false),
        member: auth(MEMBER, "tom", false),
        stranger: auth(STRANGER, "eve", false),
        admin: auth(ADMIN, "root", true),
        _dir: dir,
    }
}

async fn seed(pool: &SqlitePool) {
    for (id, name, is_admin) in [
        (OWNER, "jacka", 0),
        (PAYER, "sam", 0),
        (MEMBER, "tom", 0),
        (STRANGER, "eve", 0),
        (ADMIN, "root", 1),
    ] {
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, is_admin, storage_quota_bytes, storage_used_bytes)
             VALUES (?,?,'x',?,1073741824,0)",
        )
        .bind(id)
        .bind(name)
        .bind(is_admin)
        .execute(pool)
        .await
        .unwrap();
    }
    sqlx::query("INSERT OR IGNORE INTO games (slug, display_name) VALUES ('valheim','Valheim')")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO saves (id, user_id, game_slug, label, latest_version_num)
         VALUES (?,?,'valheim','default',0)",
    )
    .bind(SAVE)
    .bind(OWNER)
    .execute(pool)
    .await
    .unwrap();
}

fn st(h: &Harness) -> State<Arc<ServerState>> {
    State(h.state.clone())
}

fn sha_of(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn manifest(files: &[(&str, &[u8])]) -> Vec<CasFile> {
    files
        .iter()
        .map(|(path, bytes)| CasFile {
            relative_path: (*path).into(),
            sha256: Sha256Hex::parse(&sha_of(bytes)).unwrap(),
            size_bytes: bytes.len() as i64,
            modified_at: None,
        })
        .collect()
}

/// A full backup by `who`: init, upload what is missing, commit. Returns the
/// shas the server asked for and the snapshot.
async fn backup_as(
    h: &Harness,
    who: &AuthUser,
    files: &[(&str, &[u8])],
    base: Option<i64>,
) -> (Vec<String>, hoard_core::wire::Snapshot) {
    let m = manifest(files);
    let init = cas::init(
        st(h),
        Extension(who.clone()),
        Path(SAVE.to_string()),
        Json(CasInit {
            base_version: base,
            files: m.clone(),
        }),
    )
    .await
    .expect("init")
    .0;

    let asked: Vec<String> = init
        .missing
        .iter()
        .map(|m| m.sha256.as_str().to_string())
        .collect();
    for sha in &asked {
        let bytes = files
            .iter()
            .find(|(_, b)| sha_of(b) == *sha)
            .map(|(_, b)| *b)
            .expect("the server only asks for shas from the manifest");
        let code = cas::upload_blob(
            st(h),
            Extension(who.clone()),
            Path((init.upload_id.clone(), sha.clone())),
            axum::http::HeaderMap::new(),
            Body::from(bytes.to_vec()),
        )
        .await
        .expect("blob upload");
        assert_eq!(code, StatusCode::NO_CONTENT);
    }

    let snap = cas::commit(
        st(h),
        Extension(who.clone()),
        Path(SAVE.to_string()),
        Json(CasCommit {
            upload_id: init.upload_id,
            base_version: base,
            device_name: Some("desk".into()),
            notes: None,
            files: m,
        }),
    )
    .await
    .expect("commit")
    .1
     .0;
    (asked, snap)
}

/// A shared save is pushed by its host: take the lease at `base`, then back up.
async fn host(
    h: &Harness,
    who: &AuthUser,
    files: &[(&str, &[u8])],
    base: Option<i64>,
) -> (Vec<String>, hoard_core::wire::Snapshot) {
    let Json(_) = leases::acquire(
        st(h),
        Extension(who.clone()),
        Path(SAVE.to_string()),
        axum::http::HeaderMap::new(),
        Json(LeaseAcquireRequest {
            base_version: base.unwrap_or(0),
        }),
    )
    .await
    .expect("lease");
    backup_as(h, who, files, base).await
}

/// A group owned by the payer, with the save's owner and the member in it.
async fn group_with_everyone(h: &Harness) -> String {
    let (_, Json(g)) = groups::create(
        st(h),
        Extension(h.payer.clone()),
        Json(CreateGroupRequest {
            name: "valheim crew".into(),
        }),
    )
    .await
    .expect("group");
    for who in [&h.owner, &h.member] {
        let (_, Json(inv)) = groups::create_invite(
            st(h),
            Extension(h.payer.clone()),
            Path(g.id.clone()),
            Json(CreateInviteRequest::default()),
        )
        .await
        .expect("invite");
        let Json(_) = groups::join(
            st(h),
            Extension(who.clone()),
            Json(JoinGroupRequest { token: inv.token }),
        )
        .await
        .expect("join");
    }
    g.id
}

async fn share(
    h: &Harness,
    who: &AuthUser,
    group_id: &str,
) -> Result<Save, (StatusCode, serde_json::Value)> {
    share::share(
        st(h),
        Extension(who.clone()),
        Path(SAVE.to_string()),
        Json(ShareSaveRequest {
            group_id: group_id.into(),
        }),
    )
    .await
    .map(|Json(s)| s)
    .map_err(|(code, Json(body))| (code, body))
}

async fn unshare(
    h: &Harness,
    who: &AuthUser,
) -> Result<StatusCode, (StatusCode, serde_json::Value)> {
    share::unshare(st(h), Extension(who.clone()), Path(SAVE.to_string()))
        .await
        .map_err(|(code, Json(body))| (code, body))
}

async fn used(pool: &SqlitePool, user_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT storage_used_bytes FROM users WHERE id=?")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn group_used(pool: &SqlitePool, group_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT storage_used_bytes FROM groups WHERE id=?")
        .bind(group_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// `(refcount, size_bytes)` of a row in the user's or the group's blob table.
async fn user_blob(pool: &SqlitePool, sha: &str) -> Option<(i64, i64)> {
    sqlx::query_as("SELECT refcount, size_bytes FROM blobs WHERE user_id=? AND sha256=?")
        .bind(OWNER)
        .bind(sha)
        .fetch_optional(pool)
        .await
        .unwrap()
}

async fn group_blob(pool: &SqlitePool, group_id: &str, sha: &str) -> Option<(i64, i64)> {
    sqlx::query_as("SELECT refcount, size_bytes FROM group_blobs WHERE group_id=? AND sha256=?")
        .bind(group_id)
        .bind(sha)
        .fetch_optional(pool)
        .await
        .unwrap()
}

fn user_key(sha: &str) -> String {
    hoard_server::store::blob_key(OWNER, sha)
}

fn group_key(group_id: &str, sha: &str) -> String {
    hoard_server::namespace::Namespace::Group(group_id.into()).blob_key(sha)
}

async fn stored(h: &Harness, key: &str) -> bool {
    h.state.store.exists(key).await.unwrap()
}

/// Download one version as `who` and unpack it to `(path, bytes)` pairs.
async fn download_as(h: &Harness, who: &AuthUser, version: i64) -> Vec<(String, Vec<u8>)> {
    use tokio::io::AsyncReadExt;
    let resp = snapshots::download(
        st(h),
        Extension(who.clone()),
        Path((SAVE.to_string(), version)),
    )
    .await
    .expect("download");
    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024 * 1024)
        .await
        .expect("body");
    let reader = async_compression::tokio::bufread::ZstdDecoder::new(std::io::Cursor::new(body));
    let mut archive = tokio_tar::Archive::new(reader);
    let mut entries = archive.entries().unwrap();
    let mut got: Vec<(String, Vec<u8>)> = Vec::new();
    while let Some(entry) = futures::StreamExt::next(&mut entries).await {
        let mut entry = entry.unwrap();
        let path = entry.path().unwrap().to_string_lossy().to_string();
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf).await.unwrap();
        got.push((path, buf));
    }
    got.sort_by(|x, y| x.0.cmp(&y.0));
    got
}

/// Two versions by the owner: `a` in both, `b` in the first, `c` in the second.
struct Fixture {
    a: Vec<u8>,
    b: Vec<u8>,
    c: Vec<u8>,
}

impl Fixture {
    fn new() -> Self {
        Fixture {
            a: vec![1u8; 30_000],
            b: vec![2u8; 10_000],
            c: vec![3u8; 20_000],
        }
    }

    fn total(&self) -> i64 {
        (self.a.len() + self.b.len() + self.c.len()) as i64
    }
}

async fn two_versions(h: &Harness, f: &Fixture) {
    backup_as(
        h,
        &h.owner,
        &[("world.db", &f.a), ("world.fwl", &f.b)],
        Some(0),
    )
    .await;
    backup_as(
        h,
        &h.owner,
        &[("world.db", &f.a), ("world.fwl", &f.c)],
        Some(1),
    )
    .await;
}

/// Sharing moves the bytes: the owner's rows and files go, the group's arrive
/// with the same refcounts, and the bill moves from the owner to the group's
/// owner byte for byte.
#[tokio::test]
async fn share_moves_blobs_rows_files_and_quota_to_the_group() {
    let h = harness().await;
    let f = Fixture::new();
    two_versions(&h, &f).await;
    let pool = &h.state.pool;
    assert_eq!(used(pool, OWNER).await, f.total());
    assert_eq!(user_blob(pool, &sha_of(&f.a)).await, Some((2, 30_000)));

    let gid = group_with_everyone(&h).await;
    let save = share(&h, &h.owner, &gid).await.expect("shared");
    assert_eq!(
        save.shared.as_ref().map(|s| s.group_id.as_str()),
        Some(gid.as_str())
    );

    for (bytes, refs) in [(&f.a, 2), (&f.b, 1), (&f.c, 1)] {
        let sha = sha_of(bytes);
        assert_eq!(user_blob(pool, &sha).await, None, "the owner's row is gone");
        assert_eq!(
            group_blob(pool, &gid, &sha).await,
            Some((refs, bytes.len() as i64)),
            "the group's row carries the refcount"
        );
        assert!(
            stored(&h, &group_key(&gid, &sha)).await,
            "file at the group key"
        );
        assert!(
            !stored(&h, &user_key(&sha)).await,
            "no file at the user key"
        );
    }
    assert_eq!(used(pool, OWNER).await, 0);
    assert_eq!(used(pool, PAYER).await, f.total());
    assert_eq!(group_used(pool, &gid).await, f.total());
}

/// A member's push negotiates against the group's namespace, lands under the
/// group's keys, and is charged to the group's owner.
#[tokio::test]
async fn a_member_pushes_into_the_group_namespace_on_the_group_owners_bill() {
    let h = harness().await;
    let f = Fixture::new();
    two_versions(&h, &f).await;
    let gid = group_with_everyone(&h).await;
    share(&h, &h.owner, &gid).await.expect("shared");
    let pool = &h.state.pool;

    let d = vec![4u8; 5_000];
    let (asked, snap) = host(
        &h,
        &h.member,
        &[("world.db", &f.a), ("world.fwl", &d)],
        Some(2),
    )
    .await;
    assert_eq!(asked, vec![sha_of(&d)], "only what the group lacks travels");
    assert_eq!(snap.version_num, 3);
    assert_eq!(snap.parent_version, Some(2));
    let head: i64 = sqlx::query_scalar("SELECT latest_version_num FROM saves WHERE id=?")
        .bind(SAVE)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(head, 3);

    assert!(stored(&h, &group_key(&gid, &sha_of(&d))).await);
    assert_eq!(
        group_blob(pool, &gid, &sha_of(&f.a)).await,
        Some((3, 30_000))
    );
    assert_eq!(used(pool, PAYER).await, f.total() + 5_000);
    assert_eq!(group_used(pool, &gid).await, f.total() + 5_000);
    assert_eq!(used(pool, MEMBER).await, 0, "the member is not charged");
    assert_eq!(used(pool, OWNER).await, 0, "nor is the save's owner");
}

/// A member gets the owner's bytes back out of the group namespace.
#[tokio::test]
async fn a_member_downloads_what_the_owner_pushed() {
    let h = harness().await;
    let f = Fixture::new();
    two_versions(&h, &f).await;
    let gid = group_with_everyone(&h).await;
    share(&h, &h.owner, &gid).await.expect("shared");

    let got = download_as(&h, &h.member, 1).await;
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].0, "world.db");
    assert_eq!(got[0].1, f.a);
    assert_eq!(got[1].0, "world.fwl");
    assert_eq!(got[1].1, f.b);
}

/// Somebody outside the group is told the save does not exist.
#[tokio::test]
async fn a_strangers_init_on_a_shared_save_is_404() {
    let h = harness().await;
    let f = Fixture::new();
    two_versions(&h, &f).await;
    let gid = group_with_everyone(&h).await;
    share(&h, &h.owner, &gid).await.expect("shared");

    let err = cas::init(
        st(&h),
        Extension(h.stranger.clone()),
        Path(SAVE.to_string()),
        Json(CasInit {
            base_version: Some(2),
            files: manifest(&[("world.db", &f.a)]),
        }),
    )
    .await
    .expect_err("not theirs");
    assert_eq!(err.0, StatusCode::NOT_FOUND);
}

/// Unsharing is the exact reverse move, refused while a member holds a live
/// lease, and refused again once there is nothing to reverse.
#[tokio::test]
async fn unshare_reverses_the_move_and_a_second_one_is_409() {
    let h = harness().await;
    let f = Fixture::new();
    two_versions(&h, &f).await;
    let gid = group_with_everyone(&h).await;
    share(&h, &h.owner, &gid).await.expect("shared");
    let pool = &h.state.pool;

    sqlx::query(
        "INSERT INTO save_leases (save_id, holder_user_id, acquired_at, renewed_at, base_version)
         VALUES (?,?, strftime('%Y-%m-%dT%H:%M:%SZ','now'), strftime('%Y-%m-%dT%H:%M:%SZ','now'), 2)",
    )
    .bind(SAVE)
    .bind(MEMBER)
    .execute(pool)
    .await
    .unwrap();
    let (code, body) = unshare(&h, &h.owner)
        .await
        .expect_err("a member is hosting");
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(body["code"], "lease_held");
    sqlx::query(
        "UPDATE save_leases SET released_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE save_id=?",
    )
    .bind(SAVE)
    .execute(pool)
    .await
    .unwrap();

    assert_eq!(unshare(&h, &h.owner).await.unwrap(), StatusCode::NO_CONTENT);
    for (bytes, refs) in [(&f.a, 2), (&f.b, 1), (&f.c, 1)] {
        let sha = sha_of(bytes);
        assert_eq!(
            user_blob(pool, &sha).await,
            Some((refs, bytes.len() as i64))
        );
        assert_eq!(group_blob(pool, &gid, &sha).await, None);
        assert!(stored(&h, &user_key(&sha)).await);
        assert!(!stored(&h, &group_key(&gid, &sha)).await);
    }
    assert_eq!(used(pool, OWNER).await, f.total());
    assert_eq!(used(pool, PAYER).await, 0);
    assert_eq!(group_used(pool, &gid).await, 0);
    let shared: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM shared_saves WHERE save_id=?")
        .bind(SAVE)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(shared, 0);

    let (code, body) = unshare(&h, &h.owner).await.expect_err("nothing to unshare");
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(body["code"], "not_shared");
}

/// A group the caller is not in reads as unknown; a second share is a conflict;
/// a member cannot share the owner's save.
#[tokio::test]
async fn share_refuses_foreign_groups_and_double_shares() {
    let h = harness().await;
    let f = Fixture::new();
    two_versions(&h, &f).await;
    let gid = group_with_everyone(&h).await;

    let (_, Json(private)) = groups::create(
        st(&h),
        Extension(h.stranger.clone()),
        Json(CreateGroupRequest {
            name: "not yours".into(),
        }),
    )
    .await
    .expect("group");
    let (code, _) = share(&h, &h.owner, &private.id)
        .await
        .expect_err("not a member");
    assert_eq!(code, StatusCode::NOT_FOUND);
    let (code, _) = share(&h, &h.owner, "no-such-group")
        .await
        .expect_err("unknown");
    assert_eq!(code, StatusCode::NOT_FOUND);

    // Before the share the member is a stranger to the save; after it they
    // are a member, and still not allowed to share.
    let (code, _) = share(&h, &h.member, &gid).await.expect_err("not theirs");
    assert_eq!(code, StatusCode::NOT_FOUND);

    share(&h, &h.owner, &gid).await.expect("shared");
    let (code, body) = share(&h, &h.owner, &gid).await.expect_err("twice");
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(body["code"], "already_shared");
    let (code, _) = share(&h, &h.member, &gid).await.expect_err("not the owner");
    assert_eq!(code, StatusCode::FORBIDDEN);
    assert_eq!(used(&h.state.pool, PAYER).await, f.total(), "charged once");
}

/// Purging a trashed version of a shared save refunds the group's owner and
/// deletes the group's objects that reached zero.
#[tokio::test]
async fn purging_trash_on_a_shared_save_refunds_the_group_owner() {
    let h = harness().await;
    let f = Fixture::new();
    two_versions(&h, &f).await;
    let gid = group_with_everyone(&h).await;
    share(&h, &h.owner, &gid).await.expect("shared");
    let pool = &h.state.pool;

    sqlx::query(
        "UPDATE snapshots SET deleted_at='2000-01-01T00:00:00Z' WHERE save_id=? AND version_num=1",
    )
    .bind(SAVE)
    .execute(pool)
    .await
    .unwrap();
    hoard_server::cleanup::run_once(
        pool,
        &h.state.config.storage.data_dir,
        &h.state.store,
        24,
        0,
        None,
    )
    .await
    .expect("cleanup");

    assert_eq!(group_blob(pool, &gid, &sha_of(&f.b)).await, None);
    assert!(!stored(&h, &group_key(&gid, &sha_of(&f.b))).await);
    assert_eq!(
        group_blob(pool, &gid, &sha_of(&f.a)).await,
        Some((1, 30_000))
    );
    assert!(stored(&h, &group_key(&gid, &sha_of(&f.a))).await);
    let left = (f.a.len() + f.c.len()) as i64;
    assert_eq!(used(pool, PAYER).await, left);
    assert_eq!(group_used(pool, &gid).await, left);
    assert_eq!(used(pool, OWNER).await, 0);
}

/// Deleting the group's owner takes the group's objects off the disk, not just
/// its rows.
#[tokio::test]
async fn deleting_the_group_owner_purges_the_groups_objects() {
    let h = harness().await;
    let f = Fixture::new();
    two_versions(&h, &f).await;
    let gid = group_with_everyone(&h).await;
    share(&h, &h.owner, &gid).await.expect("shared");
    let pool = &h.state.pool;
    let group_dir = h
        .state
        .config
        .storage
        .data_dir
        .join("blobs")
        .join("group")
        .join(&gid);
    assert!(group_dir.exists());

    let Json(out) = admin::delete_user(Extension(h.admin.clone()), st(&h), Path(PAYER.to_string()))
        .await
        .expect("deleted");
    assert_eq!(out.objects_removed, 3);
    assert_eq!(out.bytes_removed, f.total());

    assert!(!group_dir.exists());
    for bytes in [&f.a, &f.b, &f.c] {
        assert!(!stored(&h, &group_key(&gid, &sha_of(bytes))).await);
    }
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM group_blobs WHERE group_id=?")
        .bind(&gid)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "the cascade took the rows");
    let shared: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM shared_saves WHERE save_id=?")
        .bind(SAVE)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(shared, 0);
}
