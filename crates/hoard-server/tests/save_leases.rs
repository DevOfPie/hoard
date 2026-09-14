//! The hosting lease on a shared save (`routes::leases`), and the push gate it
//! puts in front of `cas::init` and `cas::commit`.
//!
//! The `shared_blobs.rs` harness: real handlers, a real database, a real store.
//! Three accounts in the group (the save's owner, the group's owner, a member)
//! and one outside it.

use axum::body::Body;
use axum::extract::{Extension, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use hoard_core::ids::Sha256 as Sha256Hex;
use hoard_core::wire::{
    CasCommit, CasFile, CasInit, CasInitOut, CreateGroupRequest, CreateInviteRequest,
    JoinGroupRequest, Lease, LeaseAcquireRequest, ShareSaveRequest,
};
use hoard_server::auth::AuthUser;
use hoard_server::routes::events::Frame;
use hoard_server::routes::health::ServerState;
use hoard_server::routes::{cas, groups, leases, share};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use std::sync::Arc;
use std::time::Instant;
use uuid::Uuid;

const OWNER: &str = "11111111-2222-4333-8444-555555555555";
const PAYER: &str = "22222222-2222-4333-8444-555555555555";
const MEMBER: &str = "33333333-2222-4333-8444-555555555555";
const STRANGER: &str = "44444444-2222-4333-8444-555555555555";
const SAVE: &str = "66666666-7777-4888-8999-aaaaaaaaaaaa";

type Failure = (StatusCode, serde_json::Value);

struct Harness {
    state: Arc<ServerState>,
    owner: AuthUser,
    payer: AuthUser,
    member: AuthUser,
    stranger: AuthUser,
    _dir: tempfile::TempDir,
}

fn auth(id: &str, name: &str) -> AuthUser {
    AuthUser {
        user_id: Uuid::parse_str(id).unwrap(),
        username: name.into(),
        is_admin: false,
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
        owner: auth(OWNER, "jacka"),
        payer: auth(PAYER, "sam"),
        member: auth(MEMBER, "tom"),
        stranger: auth(STRANGER, "eve"),
        _dir: dir,
    }
}

async fn seed(pool: &SqlitePool) {
    for (id, name) in [
        (OWNER, "jacka"),
        (PAYER, "sam"),
        (MEMBER, "tom"),
        (STRANGER, "eve"),
    ] {
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, is_admin, storage_quota_bytes, storage_used_bytes)
             VALUES (?,?,'x',0,1073741824,0)",
        )
        .bind(id)
        .bind(name)
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

fn path() -> Path<String> {
    Path(SAVE.to_string())
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

fn fail((code, Json(body)): (StatusCode, Json<serde_json::Value>)) -> Failure {
    (code, body)
}

async fn init_as(
    h: &Harness,
    who: &AuthUser,
    files: &[(&str, &[u8])],
    base: Option<i64>,
) -> Result<CasInitOut, Failure> {
    cas::init(
        st(h),
        Extension(who.clone()),
        path(),
        Json(CasInit {
            base_version: base,
            files: manifest(files),
        }),
    )
    .await
    .map(|Json(o)| o)
    .map_err(fail)
}

async fn commit_as(
    h: &Harness,
    who: &AuthUser,
    upload_id: String,
    files: &[(&str, &[u8])],
    base: Option<i64>,
) -> Result<hoard_core::wire::Snapshot, Failure> {
    cas::commit(
        st(h),
        Extension(who.clone()),
        path(),
        Json(CasCommit {
            upload_id,
            base_version: base,
            device_name: Some("desk".into()),
            notes: None,
            files: manifest(files),
        }),
    )
    .await
    .map(|(_, Json(s))| s)
    .map_err(fail)
}

/// A full backup by `who`: init, upload what is missing, commit.
async fn backup_as(
    h: &Harness,
    who: &AuthUser,
    files: &[(&str, &[u8])],
    base: Option<i64>,
) -> Result<hoard_core::wire::Snapshot, Failure> {
    let init = init_as(h, who, files, base).await?;
    for m in &init.missing {
        let sha = m.sha256.as_str();
        let bytes = files
            .iter()
            .find(|(_, b)| sha_of(b) == sha)
            .map(|(_, b)| *b)
            .expect("the server only asks for shas from the manifest");
        let code = cas::upload_blob(
            st(h),
            Extension(who.clone()),
            Path((init.upload_id.clone(), sha.to_string())),
            HeaderMap::new(),
            Body::from(bytes.to_vec()),
        )
        .await
        .expect("blob upload");
        assert_eq!(code, StatusCode::NO_CONTENT);
    }
    commit_as(h, who, init.upload_id, files, base).await
}

async fn acquire(h: &Harness, who: &AuthUser, base: i64) -> Result<Lease, Failure> {
    leases::acquire(
        st(h),
        Extension(who.clone()),
        path(),
        HeaderMap::new(),
        Json(LeaseAcquireRequest { base_version: base }),
    )
    .await
    .map(|Json(l)| l)
    .map_err(fail)
}

async fn get_lease(h: &Harness, who: &AuthUser) -> Result<Option<Lease>, Failure> {
    leases::get(st(h), Extension(who.clone()), path())
        .await
        .map(|Json(o)| o.lease)
        .map_err(fail)
}

async fn renew(h: &Harness, who: &AuthUser) -> Result<Lease, Failure> {
    leases::renew(st(h), Extension(who.clone()), path())
        .await
        .map(|Json(l)| l)
        .map_err(fail)
}

async fn release(h: &Harness, who: &AuthUser) -> Result<StatusCode, Failure> {
    leases::release(st(h), Extension(who.clone()), path())
        .await
        .map_err(fail)
}

async fn force(h: &Harness, who: &AuthUser) -> Result<StatusCode, Failure> {
    leases::force(st(h), Extension(who.clone()), path())
        .await
        .map_err(fail)
}

async fn unshare(h: &Harness, who: &AuthUser) -> Result<StatusCode, Failure> {
    share::unshare(st(h), Extension(who.clone()), path())
        .await
        .map_err(fail)
}

/// Move the live lease's heartbeat `secs` into the past.
async fn backdate_renewal(pool: &SqlitePool, secs: i64) {
    sqlx::query(
        "UPDATE save_leases
         SET renewed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ? || ' seconds')
         WHERE save_id = ?",
    )
    .bind(-secs)
    .bind(SAVE)
    .execute(pool)
    .await
    .unwrap();
}

/// Two versions by the owner, then a group owned by the payer with the owner
/// and the member in it, and the save shared into it. Head is 2.
struct Shared {
    h: Harness,
    a: Vec<u8>,
    c: Vec<u8>,
}

async fn shared() -> Shared {
    shared_with(&[]).await
}

/// [`shared`] with an include list on the share.
async fn shared_with(include: &[&str]) -> Shared {
    let h = harness().await;
    let a = vec![1u8; 30_000];
    let b = vec![2u8; 10_000];
    let c = vec![3u8; 20_000];
    backup_as(
        &h,
        &h.owner,
        &[("world.db", &a), ("world.fwl", &b)],
        Some(0),
    )
    .await
    .expect("v1");
    backup_as(
        &h,
        &h.owner,
        &[("world.db", &a), ("world.fwl", &c)],
        Some(1),
    )
    .await
    .expect("v2");

    let (_, Json(g)) = groups::create(
        st(&h),
        Extension(h.payer.clone()),
        Json(CreateGroupRequest {
            name: "valheim crew".into(),
        }),
    )
    .await
    .expect("group");
    for who in [&h.owner, &h.member] {
        let (_, Json(inv)) = groups::create_invite(
            st(&h),
            Extension(h.payer.clone()),
            Path(g.id.clone()),
            Json(CreateInviteRequest::default()),
        )
        .await
        .expect("invite");
        let Json(_) = groups::join(
            st(&h),
            Extension(who.clone()),
            Json(JoinGroupRequest { token: inv.token }),
        )
        .await
        .expect("join");
    }
    let Json(_) = share::share(
        st(&h),
        Extension(h.owner.clone()),
        path(),
        Json(ShareSaveRequest {
            group_id: g.id,
            include: include.iter().map(|s| s.to_string()).collect(),
        }),
    )
    .await
    .expect("shared");
    Shared { h, a, c }
}

impl Shared {
    /// A third version by `who`, who must hold the lease: `a` kept, `d` new.
    async fn push_as(&self, who: &AuthUser) -> Result<hoard_core::wire::Snapshot, Failure> {
        let d = vec![4u8; 5_000];
        backup_as(
            &self.h,
            who,
            &[("world.db", &self.a), ("world.fwl", &d)],
            Some(2),
        )
        .await
    }
}

// ---- acquire, get

#[tokio::test]
async fn a_member_acquires_a_live_lease_everyone_can_read() {
    let s = shared().await;
    let l = acquire(&s.h, &s.h.member, 2).await.expect("acquired");
    assert_eq!(l.holder_user_id, MEMBER);
    assert_eq!(l.holder_username.as_str(), "tom");
    assert_eq!(l.base_version, 2);
    assert!(l.live && !l.pushed_since);
    assert_eq!(l.acquired_at, l.renewed_at);
    assert!(l.holder_device_fp.is_none());

    let seen = get_lease(&s.h, &s.h.owner).await.unwrap().expect("visible");
    assert_eq!(seen.holder_user_id, MEMBER);
    let seen = get_lease(&s.h, &s.h.payer).await.unwrap().expect("visible");
    assert_eq!(seen.holder_user_id, MEMBER);
}

/// The fingerprint travels in the same header the device census reads.
#[tokio::test]
async fn acquire_records_the_device_fingerprint_header() {
    let s = shared().await;
    let mut headers = HeaderMap::new();
    headers.insert("x-hoard-device-fp", "fp-tom-desk".parse().unwrap());
    let Json(l) = leases::acquire(
        st(&s.h),
        Extension(s.h.member.clone()),
        path(),
        headers,
        Json(LeaseAcquireRequest { base_version: 2 }),
    )
    .await
    .expect("acquired");
    assert_eq!(l.holder_device_fp.as_deref(), Some("fp-tom-desk"));
}

#[tokio::test]
async fn a_second_members_acquire_is_409_held_with_the_holder() {
    let s = shared().await;
    acquire(&s.h, &s.h.member, 2).await.expect("acquired");
    let (code, body) = acquire(&s.h, &s.h.payer, 2).await.expect_err("held");
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(body["code"], "held");
    assert_eq!(body["lease"]["holder_user_id"], MEMBER);
    let (code, body) = acquire(&s.h, &s.h.owner, 2).await.expect_err("held");
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(body["code"], "held");
}

/// Taking one's own live lease again is a refresh: same `acquired_at`, and a
/// push already made under it stays recorded.
#[tokio::test]
async fn reacquiring_ones_own_lease_refreshes_it_and_keeps_pushed_since() {
    let s = shared().await;
    let first = acquire(&s.h, &s.h.member, 2).await.expect("acquired");
    s.push_as(&s.h.member).await.expect("pushed");
    backdate_renewal(&s.h.state.pool, 100).await;
    let again = acquire(&s.h, &s.h.member, 3).await.expect("refreshed");
    assert_eq!(again.acquired_at, first.acquired_at);
    // Back to now from 100 s ago (second granularity: `>=`).
    assert!(again.renewed_at >= first.renewed_at);
    assert!((again.renewed_at - first.renewed_at).whole_seconds() < 5);
    assert!(again.pushed_since);
    assert_eq!(again.base_version, 3);
}

#[tokio::test]
async fn acquire_with_a_stale_base_is_409_stale_with_the_head() {
    let s = shared().await;
    let (code, body) = acquire(&s.h, &s.h.member, 1).await.expect_err("stale");
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(body["code"], "stale");
    assert_eq!(body["head_version"], 2);
    assert_eq!(body["base_version"], 1);
    assert!(get_lease(&s.h, &s.h.member).await.unwrap().is_none());
}

// ---- renew, expiry, release

#[tokio::test]
async fn renew_moves_renewed_at_and_is_refused_to_everyone_else() {
    let s = shared().await;
    let l = acquire(&s.h, &s.h.member, 2).await.expect("acquired");
    backdate_renewal(&s.h.state.pool, 100).await;
    let renewed = renew(&s.h, &s.h.member).await.expect("renewed");
    // Back to now, not 100 s ago; the acquisition stays where it was.
    assert!(renewed.renewed_at >= l.renewed_at);
    assert!((renewed.renewed_at - l.renewed_at).whole_seconds() < 5);
    assert_eq!(renewed.acquired_at, l.acquired_at);
    let stored: String = sqlx::query_scalar("SELECT renewed_at FROM save_leases WHERE save_id=?")
        .bind(SAVE)
        .fetch_one(&s.h.state.pool)
        .await
        .unwrap();
    assert_eq!(
        time::OffsetDateTime::parse(&stored, &time::format_description::well_known::Rfc3339)
            .unwrap(),
        renewed.renewed_at
    );

    let (code, body) = renew(&s.h, &s.h.payer).await.expect_err("not theirs");
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(body["code"], "not_holder");
    let (code, body) = renew(&s.h, &s.h.owner).await.expect_err("not theirs");
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(body["code"], "not_holder");
}

#[tokio::test]
async fn an_expired_lease_reads_as_none_and_another_member_takes_it() {
    let s = shared().await;
    acquire(&s.h, &s.h.member, 2).await.expect("acquired");
    backdate_renewal(&s.h.state.pool, leases::LEASE_TTL_SECS + 1).await;
    assert!(get_lease(&s.h, &s.h.owner).await.unwrap().is_none());
    // Expired means gone: the old holder acquires again rather than renews.
    let (code, body) = renew(&s.h, &s.h.member).await.expect_err("expired");
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(body["code"], "not_holder");

    let l = acquire(&s.h, &s.h.payer, 2).await.expect("free");
    assert_eq!(l.holder_user_id, PAYER);
    assert!(!l.pushed_since);
}

#[tokio::test]
async fn release_frees_the_save_for_the_next_member() {
    let s = shared().await;
    acquire(&s.h, &s.h.member, 2).await.expect("acquired");
    let (code, body) = release(&s.h, &s.h.payer).await.expect_err("not theirs");
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(body["code"], "not_holder");

    assert_eq!(
        release(&s.h, &s.h.member).await.unwrap(),
        StatusCode::NO_CONTENT
    );
    assert!(get_lease(&s.h, &s.h.member).await.unwrap().is_none());
    let (_, body) = release(&s.h, &s.h.member).await.expect_err("already gone");
    assert_eq!(body["code"], "not_holder");

    let l = acquire(&s.h, &s.h.payer, 2).await.expect("free");
    assert_eq!(l.holder_user_id, PAYER);
}

// ---- the push gate

#[tokio::test]
async fn a_push_without_the_lease_is_409_lease_required_for_member_and_owner() {
    let s = shared().await;
    for who in [&s.h.member, &s.h.owner, &s.h.payer] {
        let (code, body) = s.push_as(who).await.expect_err("no lease");
        assert_eq!(code, StatusCode::CONFLICT);
        assert_eq!(body["code"], "lease_required");
        assert!(body.get("lease").is_none(), "nobody to name");
    }
    let head: i64 = sqlx::query_scalar("SELECT latest_version_num FROM saves WHERE id=?")
        .bind(SAVE)
        .fetch_one(&s.h.state.pool)
        .await
        .unwrap();
    assert_eq!(head, 2, "nothing landed");

    // With a holder, the refusal names them.
    acquire(&s.h, &s.h.member, 2).await.expect("acquired");
    let (code, body) = s.push_as(&s.h.owner).await.expect_err("not the host");
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(body["code"], "lease_required");
    assert_eq!(body["lease"]["holder_user_id"], MEMBER);
}

#[tokio::test]
async fn with_the_lease_a_push_succeeds_and_pushed_since_flips() {
    let s = shared().await;
    let l = acquire(&s.h, &s.h.member, 2).await.expect("acquired");
    assert!(!l.pushed_since);
    let snap = s.push_as(&s.h.member).await.expect("hosted push");
    assert_eq!(snap.version_num, 3);
    let l = get_lease(&s.h, &s.h.owner)
        .await
        .unwrap()
        .expect("still held");
    assert!(l.pushed_since);
    assert_eq!(l.holder_user_id, MEMBER);
}

/// The commit checks again: a lease lost between init and commit refuses the
/// commit, and the version it would have written never lands.
#[tokio::test]
async fn a_lease_lost_between_init_and_commit_refuses_the_commit() {
    let s = shared().await;
    acquire(&s.h, &s.h.member, 2).await.expect("acquired");
    let files: &[(&str, &[u8])] = &[("world.db", &s.a), ("world.fwl", &s.c)];
    let init = init_as(&s.h, &s.h.member, files, Some(2))
        .await
        .expect("init as host");
    assert!(
        init.missing.is_empty(),
        "the head's content is already there"
    );

    assert_eq!(
        force(&s.h, &s.h.payer).await.unwrap(),
        StatusCode::NO_CONTENT
    );
    acquire(&s.h, &s.h.payer, 2).await.expect("taken over");
    let (code, body) = commit_as(&s.h, &s.h.member, init.upload_id, files, Some(2))
        .await
        .expect_err("lease moved");
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(body["code"], "lease_required");
    assert_eq!(body["lease"]["holder_user_id"], PAYER);
    let head: i64 = sqlx::query_scalar("SELECT latest_version_num FROM saves WHERE id=?")
        .bind(SAVE)
        .fetch_one(&s.h.state.pool)
        .await
        .unwrap();
    assert_eq!(head, 2);
}

// ---- force

#[tokio::test]
async fn force_is_204_before_a_push_and_409_pushed_after_one() {
    let s = shared().await;
    acquire(&s.h, &s.h.member, 2).await.expect("acquired");
    assert_eq!(
        force(&s.h, &s.h.payer).await.unwrap(),
        StatusCode::NO_CONTENT
    );
    assert!(get_lease(&s.h, &s.h.payer).await.unwrap().is_none());
    // The forcing member does not inherit it: acquire runs the head check.
    let l = acquire(&s.h, &s.h.payer, 2).await.expect("free");
    assert_eq!(l.holder_user_id, PAYER);
    assert_eq!(
        release(&s.h, &s.h.payer).await.unwrap(),
        StatusCode::NO_CONTENT
    );

    acquire(&s.h, &s.h.member, 2).await.expect("acquired");
    s.push_as(&s.h.member).await.expect("pushed");
    let (code, body) = force(&s.h, &s.h.payer).await.expect_err("they pushed");
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(body["code"], "pushed");
    assert_eq!(body["lease"]["holder_user_id"], MEMBER);
    let l = get_lease(&s.h, &s.h.payer)
        .await
        .unwrap()
        .expect("still held");
    assert_eq!(l.holder_user_id, MEMBER);
}

#[tokio::test]
async fn force_on_no_live_lease_is_409_not_held() {
    let s = shared().await;
    let (code, body) = force(&s.h, &s.h.member)
        .await
        .expect_err("nothing to force");
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(body["code"], "not_held");

    acquire(&s.h, &s.h.member, 2).await.expect("acquired");
    backdate_renewal(&s.h.state.pool, leases::LEASE_TTL_SECS + 1).await;
    let (code, body) = force(&s.h, &s.h.payer)
        .await
        .expect_err("expired is not held");
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(body["code"], "not_held");
}

// ---- unshare, not shared, stranger

#[tokio::test]
async fn unshare_while_a_member_holds_a_live_lease_is_409_lease_held() {
    let s = shared().await;
    acquire(&s.h, &s.h.member, 2).await.expect("acquired");
    let (code, body) = unshare(&s.h, &s.h.owner).await.expect_err("hosted");
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(body["code"], "lease_held");

    assert_eq!(
        release(&s.h, &s.h.member).await.unwrap(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        unshare(&s.h, &s.h.owner).await.unwrap(),
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn an_unshared_saves_lease_routes_answer_409_not_shared() {
    let h = harness().await;
    let a = vec![1u8; 100];
    backup_as(&h, &h.owner, &[("world.db", &a)], Some(0))
        .await
        .expect("a private push needs no lease");

    let (code, body) = get_lease(&h, &h.owner).await.expect_err("not shared");
    assert_eq!(
        (code, body["code"].as_str()),
        (StatusCode::CONFLICT, Some("not_shared"))
    );
    let (code, body) = acquire(&h, &h.owner, 1).await.expect_err("not shared");
    assert_eq!(
        (code, body["code"].as_str()),
        (StatusCode::CONFLICT, Some("not_shared"))
    );
    let (code, body) = renew(&h, &h.owner).await.expect_err("not shared");
    assert_eq!(
        (code, body["code"].as_str()),
        (StatusCode::CONFLICT, Some("not_shared"))
    );
    let (code, body) = release(&h, &h.owner).await.expect_err("not shared");
    assert_eq!(
        (code, body["code"].as_str()),
        (StatusCode::CONFLICT, Some("not_shared"))
    );
    let (code, body) = force(&h, &h.owner).await.expect_err("not shared");
    assert_eq!(
        (code, body["code"].as_str()),
        (StatusCode::CONFLICT, Some("not_shared"))
    );
}

#[tokio::test]
async fn a_stranger_gets_404_on_every_lease_route() {
    let s = shared().await;
    acquire(&s.h, &s.h.member, 2).await.expect("acquired");
    let eve = &s.h.stranger;
    assert_eq!(
        get_lease(&s.h, eve).await.unwrap_err().0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        acquire(&s.h, eve, 2).await.unwrap_err().0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(renew(&s.h, eve).await.unwrap_err().0, StatusCode::NOT_FOUND);
    assert_eq!(
        release(&s.h, eve).await.unwrap_err().0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(force(&s.h, eve).await.unwrap_err().0, StatusCode::NOT_FOUND);
    let l = get_lease(&s.h, &s.h.owner)
        .await
        .unwrap()
        .expect("untouched");
    assert_eq!(l.holder_user_id, MEMBER);
}

// ---- events

fn drain(rx: &mut tokio::sync::broadcast::Receiver<Frame>) -> Vec<Frame> {
    let mut out = Vec::new();
    while let Ok(f) = rx.try_recv() {
        out.push(f);
    }
    out
}

/// A member's session reaches everyone with a stake in the save: the owner
/// and the other member each get the lease frames and the save frame.
#[tokio::test]
async fn lease_and_save_frames_reach_the_owner_and_the_other_member() {
    let s = shared().await;
    let mut owner_rx = s.h.state.events.subscribe(s.h.owner.user_id);
    let mut payer_rx = s.h.state.events.subscribe(s.h.payer.user_id);
    let mut eve_rx = s.h.state.events.subscribe(s.h.stranger.user_id);

    acquire(&s.h, &s.h.member, 2).await.expect("acquired");
    s.push_as(&s.h.member).await.expect("pushed");
    assert_eq!(
        release(&s.h, &s.h.member).await.unwrap(),
        StatusCode::NO_CONTENT
    );

    for rx in [&mut owner_rx, &mut payer_rx] {
        let frames = drain(rx);
        let leases: Vec<_> = frames
            .iter()
            .filter_map(|f| match f {
                Frame::Lease(e) => Some(e.clone()),
                Frame::Save(_) => None,
            })
            .collect();
        let saves: Vec<_> = frames
            .iter()
            .filter_map(|f| match f {
                Frame::Save(e) => Some(e.clone()),
                Frame::Lease(_) => None,
            })
            .collect();
        assert_eq!(saves.len(), 1);
        assert_eq!(saves[0].save_id, SAVE);
        assert_eq!(saves[0].version_num, 3);

        assert_eq!(leases.len(), 3, "acquired, pushed, released");
        assert_eq!(leases[0].holder_user_id.as_deref(), Some(MEMBER));
        assert!(leases[0].live && !leases[0].pushed_since);
        assert_eq!(leases[1].holder_user_id.as_deref(), Some(MEMBER));
        assert!(leases[1].live && leases[1].pushed_since);
        assert!(leases[2].holder_user_id.is_none());
        assert!(!leases[2].live && leases[2].pushed_since);
    }
    assert!(drain(&mut eve_rx).is_empty(), "a stranger hears nothing");
}

// ---- a list-shaped share (HRD-D-0019)

/// The owner's backup of what the list leaves out goes without the lease and is
/// no lease news: a save frame, no lease frame, and still nobody hosting.
#[tokio::test]
async fn an_owner_backup_past_an_unchanged_world_publishes_no_lease_frame() {
    let s = shared_with(&["world.db"]).await;
    let mut owner_rx = s.h.state.events.subscribe(s.h.owner.user_id);
    let mut member_rx = s.h.state.events.subscribe(s.h.member.user_id);

    let snap = s.push_as(&s.h.owner).await.expect("no lease needed");
    assert_eq!(snap.version_num, 3);
    for rx in [&mut owner_rx, &mut member_rx] {
        let frames = drain(rx);
        assert!(
            !frames.iter().any(|f| matches!(f, Frame::Lease(_))),
            "no lease frame"
        );
        assert_eq!(
            frames
                .iter()
                .filter(|f| matches!(f, Frame::Save(_)))
                .count(),
            1
        );
    }
    assert!(get_lease(&s.h, &s.h.member).await.unwrap().is_none());
}

/// Acquire accepts a head that moved only outside the list, and is stale again
/// once a hosted push changed the world.
#[tokio::test]
async fn acquire_is_stale_only_when_the_world_moved() {
    let s = shared_with(&["world.db"]).await;
    s.push_as(&s.h.owner).await.expect("owner backup, v3");

    let l = acquire(&s.h, &s.h.member, 2)
        .await
        .expect("the world is v2's");
    assert_eq!(l.base_version, 2);
    let world = vec![6u8; 30_000];
    let d = vec![4u8; 5_000];
    let snap = backup_as(&s.h, &s.h.member, &[("world.db", &world)], Some(2))
        .await
        .expect("hosted push on top of the owner's");
    assert_eq!(snap.version_num, 4);
    assert_eq!(
        release(&s.h, &s.h.member).await.unwrap(),
        StatusCode::NO_CONTENT
    );

    let fwl: String = sqlx::query_scalar(
        "SELECT sf.sha256 FROM snapshot_files sf JOIN snapshots s ON s.id = sf.snapshot_id
         WHERE s.save_id = ? AND s.version_num = 4 AND sf.relative_path = 'world.fwl'",
    )
    .bind(SAVE)
    .fetch_one(&s.h.state.pool)
    .await
    .unwrap();
    assert_eq!(fwl, sha_of(&d), "the owner's file came forward");

    for base in [2, 3] {
        let (code, body) = acquire(&s.h, &s.h.payer, base).await.expect_err("stale");
        assert_eq!(code, StatusCode::CONFLICT);
        assert_eq!(body["code"], "stale");
        assert_eq!(body["head_version"], 4);
    }
    acquire(&s.h, &s.h.payer, 4).await.expect("current");
}
