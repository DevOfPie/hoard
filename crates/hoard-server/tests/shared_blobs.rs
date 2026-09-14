//! A save shared into a group: where its bytes go and who pays (`routes::share`,
//! `namespace`).
//!
//! The `cas_roundtrip.rs` approach: the real handlers against a real database
//! and a real store. Three accounts: the save's owner, the group's owner (who
//! pays), and a plain member, so a charge that lands on the wrong one shows.

use axum::body::Body;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use hoard_core::ids::Sha256 as Sha256Hex;
use hoard_core::wire::{
    CasCommit, CasFile, CasInit, CreateGroupRequest, CreateInviteRequest, JoinGroupRequest,
    LeaseAcquireRequest, Save, ShareSaveRequest,
};
use hoard_server::auth::AuthUser;
use hoard_server::routes::health::ServerState;
use hoard_server::routes::{admin, cas, groups, leases, saves, share, snapshots};
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
    share_with(h, who, group_id, &[]).await
}

async fn share_with(
    h: &Harness,
    who: &AuthUser,
    group_id: &str,
    include: &[&str],
) -> Result<Save, (StatusCode, serde_json::Value)> {
    share::share(
        st(h),
        Extension(who.clone()),
        Path(SAVE.to_string()),
        Json(ShareSaveRequest {
            group_id: group_id.into(),
            include: include.iter().map(|s| s.to_string()).collect(),
        }),
    )
    .await
    .map(|Json(s)| s)
    .map_err(|(code, Json(body))| (code, body))
}

async fn list_as(h: &Harness, who: &AuthUser) -> Vec<Save> {
    saves::list(
        st(h),
        Extension(who.clone()),
        Query(saves::ListQuery { game_slug: None }),
    )
    .await
    .map(|Json(v)| v)
    .expect("list")
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

/// The include list travels with the share: the owner's and every member's
/// listing carry the same one, so every machine walks the same files.
#[tokio::test]
async fn the_include_list_is_stored_with_the_share_and_listed_to_everyone() {
    let h = harness().await;
    let f = Fixture::new();
    two_versions(&h, &f).await;
    let gid = group_with_everyone(&h).await;

    let include = [
        "worlds_local/Alpha.db",
        "worlds_local/Alpha.fwl",
        "worlds_local/Alpha_backup_*",
    ];
    let save = share_with(&h, &h.owner, &gid, &include)
        .await
        .expect("shared");
    let want: Vec<String> = include.iter().map(|s| s.to_string()).collect();
    assert_eq!(save.shared.as_ref().map(|s| &s.include), Some(&want));

    for who in [&h.owner, &h.member, &h.payer] {
        let rows = list_as(&h, who).await;
        let row = rows
            .iter()
            .find(|s| s.id.as_str() == SAVE)
            .unwrap_or_else(|| panic!("{} sees the save", who.username));
        assert_eq!(
            row.shared.as_ref().map(|s| &s.include),
            Some(&want),
            "{} reads the same list",
            who.username
        );
    }

    // Unsharing forgets it: the next share starts from its own body.
    unshare(&h, &h.owner).await.expect("unshared");
    let save = share(&h, &h.owner, &gid).await.expect("shared again");
    assert!(save.shared.as_ref().unwrap().include.is_empty());
}

/// The list is enforced where it cannot be forgotten: a push carrying a file
/// the share does not name is refused before a byte moves, whoever pushes.
#[tokio::test]
async fn a_push_outside_the_include_list_is_refused() {
    let h = harness().await;
    let f = Fixture::new();
    two_versions(&h, &f).await;
    let gid = group_with_everyone(&h).await;
    share_with(
        &h,
        &h.owner,
        &gid,
        &["worlds_local/Alpha.db", "worlds_local/Alpha.fwl"],
    )
    .await
    .expect("shared");
    let Json(_) = leases::acquire(
        st(&h),
        Extension(h.owner.clone()),
        Path(SAVE.to_string()),
        axum::http::HeaderMap::new(),
        Json(LeaseAcquireRequest { base_version: 2 }),
    )
    .await
    .expect("lease");

    let stray: &[(&str, &[u8])] = &[
        ("worlds_local/Alpha.db", b"world"),
        ("characters_local/Me.fch", b"me"),
    ];
    let err = match cas::init(
        st(&h),
        Extension(h.owner.clone()),
        Path(SAVE.to_string()),
        Json(CasInit {
            base_version: Some(2),
            files: manifest(stray),
        }),
    )
    .await
    {
        Err(e) => e,
        Ok(_) => panic!("a character file is refused"),
    };
    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert_eq!(err.1 .0["code"], "outside_include");
    assert_eq!(err.1 .0["path"], "characters_local/Me.fch");

    let inside: &[(&str, &[u8])] = &[("worlds_local/Alpha.db", b"world")];
    let (_, snap) = backup_as(&h, &h.owner, inside, Some(2)).await;
    assert_eq!(snap.version_num, 3);
}

/// One version of a whole Valheim folder, uploaded before any share: a
/// character and two worlds.
struct Folder {
    character: Vec<u8>,
    alpha_db: Vec<u8>,
    alpha_fwl: Vec<u8>,
    beta_db: Vec<u8>,
    beta_fwl: Vec<u8>,
}

impl Folder {
    fn new() -> Self {
        Folder {
            character: b"the character alice plays".to_vec(),
            alpha_db: vec![5u8; 7_000],
            alpha_fwl: vec![6u8; 300],
            beta_db: vec![7u8; 9_000],
            beta_fwl: vec![8u8; 400],
        }
    }

    fn files(&self) -> Vec<(&'static str, &[u8])> {
        vec![
            ("characters_local/x.fch", &self.character),
            ("worlds_local/Alpha.db", &self.alpha_db),
            ("worlds_local/Alpha.fwl", &self.alpha_fwl),
            ("worlds_local/Beta.db", &self.beta_db),
            ("worlds_local/Beta.fwl", &self.beta_fwl),
        ]
    }
}

/// `Folder` as v1, then shared naming the Alpha world only.
async fn folder_shared_as_alpha(h: &Harness, f: &Folder) {
    backup_as(h, &h.owner, &f.files(), Some(0)).await;
    let gid = group_with_everyone(h).await;
    share_with(
        h,
        &h.owner,
        &gid,
        &[
            "worlds_local/Alpha.db",
            "worlds_local/Alpha.fwl",
            "worlds_local/Alpha_backup_*",
        ],
    )
    .await
    .expect("shared");
}

async fn versions_as(h: &Harness, who: &AuthUser) -> Vec<hoard_core::wire::Snapshot> {
    snapshots::list(
        st(h),
        Extension(who.clone()),
        Path(SAVE.to_string()),
        Query(snapshots::ListQuery {
            include_deleted: false,
            limit: 50,
            offset: 0,
        }),
    )
    .await
    .map(|Json(v)| v)
    .expect("list")
}

async fn detail_paths_as(h: &Harness, who: &AuthUser, version: i64) -> Vec<String> {
    let Json(d) = snapshots::detail(
        st(h),
        Extension(who.clone()),
        Path((SAVE.to_string(), version)),
    )
    .await
    .expect("detail");
    d.files.into_iter().map(|f| f.relative_path).collect()
}

/// A member reads what the share names and nothing else, on a version uploaded
/// before the share: the listing's totals, the manifest and the download. The
/// owner still reads all five files.
#[tokio::test]
async fn a_member_reads_only_the_include_list_even_on_an_older_version() {
    let h = harness().await;
    let f = Folder::new();
    folder_shared_as_alpha(&h, &f).await;

    let v = versions_as(&h, &h.member).await;
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].file_count, 2);
    assert_eq!(
        v[0].total_size_bytes,
        (f.alpha_db.len() + f.alpha_fwl.len()) as i64
    );
    assert!(v[0].insight.is_none());
    assert_eq!(
        detail_paths_as(&h, &h.member, 1).await,
        ["worlds_local/Alpha.db", "worlds_local/Alpha.fwl"]
    );
    let got = download_as(&h, &h.member, 1).await;
    assert_eq!(
        got,
        vec![
            ("worlds_local/Alpha.db".to_string(), f.alpha_db.clone()),
            ("worlds_local/Alpha.fwl".to_string(), f.alpha_fwl.clone()),
        ]
    );

    let v = versions_as(&h, &h.owner).await;
    assert_eq!(v[0].file_count, 5);
    assert_eq!(detail_paths_as(&h, &h.owner, 1).await.len(), 5);
    let got = download_as(&h, &h.owner, 1).await;
    let mut want: Vec<(String, Vec<u8>)> = f
        .files()
        .into_iter()
        .map(|(p, b)| (p.to_string(), b.to_vec()))
        .collect();
    want.sort_by(|x, y| x.0.cmp(&y.0));
    assert_eq!(got, want);
}

/// The save's size, in the list and on its own, covers what the share names for
/// a member and every file for the owner.
#[tokio::test]
async fn a_member_sees_the_size_of_the_include_list_only() {
    let h = harness().await;
    let f = Folder::new();
    folder_shared_as_alpha(&h, &f).await;
    let alpha = (f.alpha_db.len() + f.alpha_fwl.len()) as i64;
    let all: i64 = f.files().iter().map(|(_, b)| b.len() as i64).sum();

    let size_as = |who: AuthUser| {
        let h = &h;
        async move {
            let listed = list_as(h, &who).await;
            assert_eq!(listed.len(), 1);
            let Json(one) = saves::get_one(st(h), Extension(who), Path(SAVE.to_string()))
                .await
                .expect("get");
            (listed[0].total_size_bytes, one.total_size_bytes)
        }
    };
    assert_eq!(size_as(h.member.clone()).await, (Some(alpha), Some(alpha)));
    assert_eq!(size_as(h.owner.clone()).await, (Some(all), Some(all)));
}

/// The group's tables hold the character's blob, but a member hosting the save
/// cannot reach it by its hash: `init` asks for its bytes like absent content,
/// and a commit that references it without them is refused. Content the member
/// can read still deduplicates.
#[tokio::test]
async fn a_member_cannot_reach_an_excluded_blob_by_its_hash() {
    let h = harness().await;
    let f = Folder::new();
    folder_shared_as_alpha(&h, &f).await;
    acquire_as(&h, &h.member, 1).await.expect("hosting");

    let m = manifest(&[
        ("worlds_local/Alpha.db", &f.character),
        ("worlds_local/Alpha.fwl", &f.alpha_fwl),
    ]);
    let Json(init) = cas::init(
        st(&h),
        Extension(h.member.clone()),
        Path(SAVE.to_string()),
        Json(CasInit {
            base_version: Some(1),
            files: m.clone(),
        }),
    )
    .await
    .expect("init");
    let missing: Vec<String> = init
        .missing
        .iter()
        .map(|m| m.sha256.as_str().to_string())
        .collect();
    assert_eq!(missing, vec![sha_of(&f.character)]);

    let (code, Json(body)) = cas::commit(
        st(&h),
        Extension(h.member.clone()),
        Path(SAVE.to_string()),
        Json(CasCommit {
            upload_id: init.upload_id,
            base_version: Some(1),
            device_name: Some("desk".into()),
            notes: None,
            files: m,
        }),
    )
    .await
    .expect_err("the character's blob was never sent");
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["error"],
        "manifest references a blob that was not uploaded"
    );
    assert_eq!(
        count(
            &h.state.pool,
            "SELECT COUNT(*) FROM snapshots WHERE save_id=?",
            SAVE
        )
        .await,
        1
    );
}

/// A list the client cannot have made from a world name is refused whole,
/// before anything moves.
#[tokio::test]
async fn an_invalid_include_pattern_is_400_and_shares_nothing() {
    let h = harness().await;
    let f = Fixture::new();
    two_versions(&h, &f).await;
    let gid = group_with_everyone(&h).await;

    for bad in [
        "../worlds_local/Alpha.db",
        "/worlds_local/Alpha.db",
        "",
        "a//b",
    ] {
        let (code, _) = share_with(&h, &h.owner, &gid, &[bad])
            .await
            .expect_err("refused");
        assert_eq!(code, StatusCode::BAD_REQUEST, "{bad:?}");
    }
    assert_eq!(used(&h.state.pool, OWNER).await, f.total(), "nothing moved");
    assert!(list_as(&h, &h.owner).await[0].shared.is_none());
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

/// Deleting the group's owner hands every save shared into the group back to
/// its owner and takes the group's objects off the disk, not just its rows.
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
    // Was 3 and `f.total()`: the members' saves used to be purged with the
    // group (review finding 1). They go back to their owners first, so the
    // group holds nothing by the time it is purged.
    assert_eq!(out.objects_removed, 0);
    assert_eq!(out.bytes_removed, 0);

    assert!(!group_dir.exists());
    assert_back_with_owner(&h, &f, &gid).await;
    let got = download_as(&h, &h.owner, 1).await;
    assert_eq!(got[0].1, f.a);
    assert_eq!(got[1].1, f.b);
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

// ---- shares ending without the owner asking

async fn delete_save_as(h: &Harness, who: &AuthUser) -> Result<StatusCode, StatusCode> {
    saves::delete(st(h), Extension(who.clone()), Path(SAVE.to_string())).await
}

async fn remove_member_as(
    h: &Harness,
    who: &AuthUser,
    group_id: &str,
    target: &str,
) -> Result<StatusCode, (StatusCode, serde_json::Value)> {
    groups::remove_member(
        st(h),
        Extension(who.clone()),
        Path((group_id.to_string(), target.to_string())),
    )
    .await
    .map_err(|(code, Json(body))| (code, body))
}

async fn acquire_as(
    h: &Harness,
    who: &AuthUser,
    base: i64,
) -> Result<hoard_core::wire::Lease, (StatusCode, serde_json::Value)> {
    leases::acquire(
        st(h),
        Extension(who.clone()),
        Path(SAVE.to_string()),
        axum::http::HeaderMap::new(),
        Json(LeaseAcquireRequest { base_version: base }),
    )
    .await
    .map(|Json(l)| l)
    .map_err(|(code, Json(body))| (code, body))
}

async fn lease_as(h: &Harness, who: &AuthUser) -> Option<hoard_core::wire::Lease> {
    leases::get(st(h), Extension(who.clone()), Path(SAVE.to_string()))
        .await
        .expect("lease read")
        .0
        .lease
}

async fn count(pool: &SqlitePool, sql: &str, arg: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .bind(arg)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The save is back where it started: the owner's rows, files and bill, no
/// `shared_saves` row, nothing left under the group.
async fn assert_back_with_owner(h: &Harness, f: &Fixture, gid: &str) {
    let pool = &h.state.pool;
    for (bytes, refs) in [(&f.a, 2), (&f.b, 1), (&f.c, 1)] {
        let sha = sha_of(bytes);
        assert_eq!(
            user_blob(pool, &sha).await,
            Some((refs, bytes.len() as i64)),
            "the owner's row is back"
        );
        assert_eq!(group_blob(pool, gid, &sha).await, None);
        assert!(stored(h, &user_key(&sha)).await);
        assert!(!stored(h, &group_key(gid, &sha)).await);
    }
    assert_eq!(used(pool, OWNER).await, f.total());
    assert_eq!(
        count(
            pool,
            "SELECT COUNT(*) FROM shared_saves WHERE save_id=?",
            SAVE
        )
        .await,
        0
    );
}

/// Deleting a member whose save is shared into somebody else's group takes
/// the save back first: the group's owner stops paying and the group holds
/// nothing of the deleted account.
#[tokio::test]
async fn deleting_a_member_hands_their_shared_save_back_before_the_purge() {
    let h = harness().await;
    let f = Fixture::new();
    two_versions(&h, &f).await;
    let gid = group_with_everyone(&h).await;
    share(&h, &h.owner, &gid).await.expect("shared");
    let pool = &h.state.pool;
    assert_eq!(used(pool, PAYER).await, f.total());

    let Json(out) = admin::delete_user(Extension(h.admin.clone()), st(&h), Path(OWNER.to_string()))
        .await
        .expect("deleted");
    assert_eq!(
        out.objects_removed, 3,
        "the owner's objects, back under their key"
    );
    assert_eq!(out.bytes_removed, f.total());

    assert_eq!(used(pool, PAYER).await, 0);
    assert_eq!(group_used(pool, &gid).await, 0);
    assert_eq!(
        count(
            pool,
            "SELECT COUNT(*) FROM group_blobs WHERE group_id=?",
            &gid
        )
        .await,
        0
    );
    for bytes in [&f.a, &f.b, &f.c] {
        assert!(!stored(&h, &group_key(&gid, &sha_of(bytes))).await);
        assert!(!stored(&h, &user_key(&sha_of(bytes))).await);
    }
    assert_eq!(
        count(pool, "SELECT COUNT(*) FROM saves WHERE id=?", SAVE).await,
        0
    );
}

/// A share landing between `init` and `commit` moves the save's bytes. The
/// commit reads the namespace afresh: without the lease the owner is now
/// refused like any member and nothing is recorded; with it the version lands
/// under the group's keys on the group owner's bill, though `init` negotiated
/// against the owner's. (The re-check inside the commit transaction guards
/// the window after that read; it needs two writers and has no test here.)
#[tokio::test]
async fn a_share_between_init_and_commit_is_read_by_the_commit() {
    let h = harness().await;
    let f = Fixture::new();
    two_versions(&h, &f).await;
    let gid = group_with_everyone(&h).await;
    let pool = &h.state.pool;

    let d = vec![4u8; 5_000];
    let m = manifest(&[("world.db", &f.a), ("world.fwl", &d)]);
    let init = cas::init(
        st(&h),
        Extension(h.owner.clone()),
        Path(SAVE.to_string()),
        Json(CasInit {
            base_version: Some(2),
            files: m.clone(),
        }),
    )
    .await
    .expect("init")
    .0;
    assert_eq!(init.missing.len(), 1);
    cas::upload_blob(
        st(&h),
        Extension(h.owner.clone()),
        Path((init.upload_id.clone(), sha_of(&d))),
        axum::http::HeaderMap::new(),
        Body::from(d.clone()),
    )
    .await
    .expect("upload");

    share(&h, &h.owner, &gid).await.expect("shared meanwhile");

    let commit = |h: &Harness| {
        cas::commit(
            st(h),
            Extension(h.owner.clone()),
            Path(SAVE.to_string()),
            Json(CasCommit {
                upload_id: init.upload_id.clone(),
                base_version: Some(2),
                device_name: Some("desk".into()),
                notes: None,
                files: m.clone(),
            }),
        )
    };
    let (code, Json(body)) = commit(&h).await.expect_err("the save is shared now");
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(body["code"], "lease_required");
    assert_eq!(
        count(pool, "SELECT COUNT(*) FROM snapshots WHERE save_id=?", SAVE).await,
        2
    );
    assert_eq!(user_blob(pool, &sha_of(&d)).await, None);
    assert_eq!(group_blob(pool, &gid, &sha_of(&d)).await, None);
    assert_eq!(used(pool, PAYER).await, f.total());
    assert_eq!(used(pool, OWNER).await, 0);

    // The staging dir is gone with the refusal; a fresh init sees the group.
    acquire_as(&h, &h.owner, 2).await.expect("hosting");
    let (asked, snap) = backup_as(
        &h,
        &h.owner,
        &[("world.db", &f.a), ("world.fwl", &d)],
        Some(2),
    )
    .await;
    assert_eq!(asked, vec![sha_of(&d)]);
    assert_eq!(snap.version_num, 3);
    assert!(stored(&h, &group_key(&gid, &sha_of(&d))).await);
    assert!(!stored(&h, &user_key(&sha_of(&d))).await);
    assert_eq!(user_blob(pool, &sha_of(&d)).await, None);
    assert_eq!(group_blob(pool, &gid, &sha_of(&d)).await, Some((1, 5_000)));
    assert_eq!(used(pool, PAYER).await, f.total() + 5_000);
    assert_eq!(used(pool, OWNER).await, 0);
}

/// After an unshare the trash purge finds the rows back in the owner's tables
/// and refunds the owner, not the group's owner.
#[tokio::test]
async fn purging_trash_after_an_unshare_refunds_the_owner() {
    let h = harness().await;
    let f = Fixture::new();
    two_versions(&h, &f).await;
    let gid = group_with_everyone(&h).await;
    share(&h, &h.owner, &gid).await.expect("shared");
    assert_eq!(unshare(&h, &h.owner).await.unwrap(), StatusCode::NO_CONTENT);
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

    assert_eq!(user_blob(pool, &sha_of(&f.b)).await, None);
    assert!(!stored(&h, &user_key(&sha_of(&f.b))).await);
    assert_eq!(user_blob(pool, &sha_of(&f.a)).await, Some((1, 30_000)));
    assert!(stored(&h, &user_key(&sha_of(&f.a))).await);
    let left = (f.a.len() + f.c.len()) as i64;
    assert_eq!(used(pool, OWNER).await, left);
    assert_eq!(used(pool, PAYER).await, 0);
    assert_eq!(group_used(pool, &gid).await, 0);
}

/// Deleting a shared save takes it back first, so the group's rows go with
/// the refcounts they carried and the group's owner is refunded.
#[tokio::test]
async fn deleting_a_shared_save_refunds_the_group_owner() {
    let h = harness().await;
    let f = Fixture::new();
    two_versions(&h, &f).await;
    let gid = group_with_everyone(&h).await;
    share(&h, &h.owner, &gid).await.expect("shared");
    let pool = &h.state.pool;

    assert_eq!(
        delete_save_as(&h, &h.member).await,
        Err(StatusCode::NOT_FOUND),
        "a member cannot delete it"
    );
    assert_eq!(
        delete_save_as(&h, &h.owner).await,
        Ok(StatusCode::NO_CONTENT)
    );

    assert_eq!(
        count(pool, "SELECT COUNT(*) FROM saves WHERE id=?", SAVE).await,
        0
    );
    assert_eq!(
        count(
            pool,
            "SELECT COUNT(*) FROM group_blobs WHERE group_id=?",
            &gid
        )
        .await,
        0
    );
    assert_eq!(used(pool, PAYER).await, 0);
    assert_eq!(group_used(pool, &gid).await, 0);
    for bytes in [&f.a, &f.b, &f.c] {
        assert!(!stored(&h, &group_key(&gid, &sha_of(bytes))).await);
    }
}

/// A group with no shared saves may still hold an object nothing points at;
/// deleting the group takes it off the disk with the rows.
#[tokio::test]
async fn deleting_a_group_purges_stray_objects() {
    let h = harness().await;
    let f = Fixture::new();
    two_versions(&h, &f).await;
    let gid = group_with_everyone(&h).await;
    let pool = &h.state.pool;

    let sha = sha_of(&f.a);
    let key = group_key(&gid, &sha);
    let path = h.state.config.storage.data_dir.join(&key);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, &f.a).unwrap();
    assert!(stored(&h, &key).await);
    sqlx::query(
        "INSERT INTO group_blobs (group_id, sha256, size_bytes, refcount) VALUES (?,?,?,1)",
    )
    .bind(&gid)
    .bind(&sha)
    .bind(f.a.len() as i64)
    .execute(pool)
    .await
    .unwrap();

    let code = groups::delete(st(&h), Extension(h.payer.clone()), Path(gid.clone()))
        .await
        .expect("deleted");
    assert_eq!(code, StatusCode::NO_CONTENT);
    assert!(!stored(&h, &key).await);
    assert_eq!(
        count(
            pool,
            "SELECT COUNT(*) FROM group_blobs WHERE group_id=?",
            &gid
        )
        .await,
        0
    );
}

/// The group's owner removes the member who shared a save: it goes back to
/// that member, the owner stops paying, and the save is private again.
#[tokio::test]
async fn removing_a_member_takes_their_shared_save_back() {
    let h = harness().await;
    let f = Fixture::new();
    two_versions(&h, &f).await;
    let gid = group_with_everyone(&h).await;
    share(&h, &h.owner, &gid).await.expect("shared");

    assert_eq!(
        remove_member_as(&h, &h.payer, &gid, OWNER).await.unwrap(),
        StatusCode::NO_CONTENT
    );
    assert_back_with_owner(&h, &f, &gid).await;
    assert_eq!(used(&h.state.pool, PAYER).await, 0);
    assert_eq!(group_used(&h.state.pool, &gid).await, 0);

    let (code, _) = acquire_as(&h, &h.member, 2)
        .await
        .expect_err("no longer shared with the member");
    assert_eq!(code, StatusCode::NOT_FOUND);
    let (code, body) = acquire_as(&h, &h.owner, 2)
        .await
        .expect_err("nothing to host");
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(body["code"], "not_shared");
    let got = download_as(&h, &h.owner, 2).await;
    assert_eq!(got[1].1, f.c);
}

/// A member leaves while hosting a save they shared themselves: the lease
/// ends, announced to the group, and the save follows them out.
#[tokio::test]
async fn a_hosting_member_leaving_ends_the_lease_and_takes_the_save() {
    let h = harness().await;
    let f = Fixture::new();
    two_versions(&h, &f).await;
    let gid = group_with_everyone(&h).await;
    share(&h, &h.owner, &gid).await.expect("shared");
    let mut payer_rx = h.state.events.subscribe(h.payer.user_id);
    let d = vec![4u8; 5_000];
    host(
        &h,
        &h.owner,
        &[("world.db", &f.a), ("world.fwl", &d)],
        Some(2),
    )
    .await;
    assert!(lease_as(&h, &h.member)
        .await
        .is_some_and(|l| l.pushed_since));
    let _ = payer_rx.try_recv();
    while payer_rx.try_recv().is_ok() {}

    assert_eq!(
        remove_member_as(&h, &h.owner, &gid, OWNER).await.unwrap(),
        StatusCode::NO_CONTENT
    );
    let pool = &h.state.pool;
    assert_eq!(
        count(
            pool,
            "SELECT COUNT(*) FROM shared_saves WHERE save_id=?",
            SAVE
        )
        .await,
        0
    );
    assert_eq!(
        count(
            pool,
            "SELECT COUNT(*) FROM save_leases WHERE save_id=? AND released_at IS NULL",
            SAVE
        )
        .await,
        0
    );
    assert_eq!(used(pool, PAYER).await, 0);
    assert_eq!(used(pool, OWNER).await, f.total() + 5_000);
    let ended = std::iter::from_fn(|| payer_rx.try_recv().ok())
        .filter_map(|fr| match fr {
            hoard_server::routes::events::Frame::Lease(e) => Some(e),
            _ => None,
        })
        .find(|e| !e.live)
        .expect("the group heard the lease end");
    assert!(ended.holder_user_id.is_none());
}

/// The owner removes the member hosting somebody else's save: the lease ends
/// and another member can host, even though the departing member had pushed.
#[tokio::test]
async fn removing_the_host_frees_the_lease_for_the_next_member() {
    let h = harness().await;
    let f = Fixture::new();
    two_versions(&h, &f).await;
    let gid = group_with_everyone(&h).await;
    share(&h, &h.owner, &gid).await.expect("shared");
    let d = vec![4u8; 5_000];
    host(
        &h,
        &h.member,
        &[("world.db", &f.a), ("world.fwl", &d)],
        Some(2),
    )
    .await;
    assert!(lease_as(&h, &h.owner).await.is_some_and(|l| l.pushed_since));

    assert_eq!(
        remove_member_as(&h, &h.payer, &gid, MEMBER).await.unwrap(),
        StatusCode::NO_CONTENT
    );
    assert!(lease_as(&h, &h.owner).await.is_none());
    let lease = acquire_as(&h, &h.owner, 3).await.expect("free to host");
    assert_eq!(lease.holder_user_id, OWNER);
    // Still shared: the departing member owned nothing in the group.
    assert_eq!(
        count(
            &h.state.pool,
            "SELECT COUNT(*) FROM shared_saves WHERE save_id=?",
            SAVE
        )
        .await,
        1
    );
}
