//! Which release is the newest this machine should run, and is this binary
//! behind it? Used by the service's updater (`hoardd::updater`), `hoard
//! upgrade`, the `hoard` status panel (the amber `cli` dot), the window's own
//! probe and the graphical installer.
//!
//! The answer depends on the update [`Channel`] the user chose
//! ([`crate::prefs::Prefs::prerelease_updates`], HRD-D-0024):
//!
//! - **Stable** asks GitHub for the "latest release", the same source the
//!   `install.sh` / `install.ps1` one-liners resolve. GitHub never names a
//!   pre-release there.
//! - **Pre-release** lists the recent releases and takes the highest SemVer,
//!   full releases included, so `1.2.0` still wins over `1.2.0-2` when it
//!   ships.
//!
//! Every network path here is best-effort with a short timeout: a check that
//! fails or times out must never block the CLI, so callers treat `None` as
//! "assume up to date".

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::install::fetch::Asset;

/// GitHub repo the releases live under (matches `install.sh`'s `REPO`).
const REPO: &str = "DevOfPie/hoard";

/// GitHub's API root. Every discovery takes it as a parameter so the tests can
/// point it at a canned server; production always passes this.
pub const GITHUB_API: &str = "https://api.github.com";

/// How many releases the pre-release channel looks at. One page, newest first:
/// the highest version is always among the most recent ones, and a second
/// request would double the rate-limit cost of every check.
const LISTED: u32 = 30;

/// How long a cached "latest version" answer is trusted before we re-check. Keeps
/// the status panel instant on repeated `hoard` runs and stays well under
/// GitHub's unauthenticated rate limit.
const CACHE_TTL: Duration = Duration::from_secs(6 * 60 * 60);

/// This binary's version (compile-time).
pub fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// A version string as SemVer, tolerant of surrounding whitespace and the tag's
/// leading `v`. `None` for anything SemVer does not accept (`1.2`, `nightly`).
///
/// The pre-release suffix is kept, and it is what this is for: `1.2.0-1` is a
/// test build *before* `1.2.0`, so an install on it must be offered `1.2.0`
/// when it ships. Dropping the suffix made the two equal, and a tester stayed
/// on the pre-release for ever (HRD-F-0031).
pub fn parse(v: &str) -> Option<semver::Version> {
    let v = v.trim();
    semver::Version::parse(v.strip_prefix('v').unwrap_or(v)).ok()
}

/// True when `candidate` is strictly newer than `base` by SemVer precedence:
/// `1.1.7 < 1.2.0-1 < 1.2.0-2 < 1.2.0`. Build metadata (`+…`) is ignored, as
/// SemVer says. Unparseable input on either side → `false`: never nag, and never
/// install, on a version we can't compare.
pub fn is_newer(candidate: &str, base: &str) -> bool {
    match (parse(candidate), parse(base)) {
        (Some(a), Some(b)) => a.cmp_precedence(&b).is_gt(),
        _ => false,
    }
}

/// Which releases this machine updates to.
///
/// Stored in the update-check cache and the service's ledger, which is why it
/// serialises: an answer fetched on one channel must never be served on the
/// other. A record written before channels existed reads as `Stable`, which is
/// what it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    /// Full releases only: GitHub's "latest release".
    #[default]
    Stable,
    /// Pre-releases too: the highest SemVer among the recent releases.
    Prerelease,
}

impl Channel {
    pub fn from_prerelease(opted_in: bool) -> Self {
        if opted_in {
            Channel::Prerelease
        } else {
            Channel::Stable
        }
    }

    /// The channel the user chose, read from `prefs.json` now. Read on every
    /// check rather than once, so the switch takes effect without a restart. An
    /// unreadable prefs file means `Stable`: nobody gets test builds by accident.
    pub fn from_prefs() -> Self {
        crate::prefs::Prefs::load_default()
            .map(|(p, _)| Self::from_prerelease(p.prerelease_updates))
            .unwrap_or_default()
    }
}

/// A published release, as GitHub describes it: the part discovery needs.
#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

impl Release {
    /// The tag without its `v`: `v1.2.0-1` → `1.2.0-1`.
    pub fn version(&self) -> String {
        self.tag_name.trim().trim_start_matches('v').to_string()
    }
}

/// The newest release on `channel`, with its files.
///
/// **The one place that decides which release is "the latest"**, so the service,
/// the CLI, the window and the installer cannot disagree about it. The caller
/// brings the client, since each has its own timeout and User-Agent; `api` is
/// [`GITHUB_API`] outside tests.
pub async fn discover(client: &reqwest::Client, api: &str, channel: Channel) -> Result<Release> {
    let api = api.trim_end_matches('/');
    match channel {
        Channel::Stable => get_json(client, &format!("{api}/repos/{REPO}/releases/latest")).await,
        Channel::Prerelease => {
            let url = format!("{api}/repos/{REPO}/releases?per_page={LISTED}");
            let listed: Vec<Release> = get_json(client, &url).await?;
            highest(listed)
                .ok_or_else(|| anyhow!("no published release with a SemVer tag at {url}"))
        }
    }
}

/// The highest SemVer among `releases`, drafts left out. Full releases compete
/// with pre-releases on equal terms: that is what lets `1.2.0` supersede
/// `1.2.0-2`, and what keeps an opted-in machine from being stuck on test builds
/// once the release ships. A tag that is not SemVer is skipped, not guessed at.
pub fn highest(releases: Vec<Release>) -> Option<Release> {
    releases
        .into_iter()
        .filter(|r| !r.draft)
        .filter_map(|r| parse(&r.tag_name).map(|v| (v, r)))
        .max_by(|(a, _), (b, _)| a.cmp_precedence(b))
        .map(|(_, r)| r)
}

pub(crate) async fn get_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
) -> Result<T> {
    let resp = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .with_context(|| format!("asking GitHub for {url}"))?;
    if !resp.status().is_success() {
        bail!(
            "GitHub answered {} for {url}; the release may not be published yet",
            resp.status()
        );
    }
    resp.json().await.with_context(|| format!("parsing {url}"))
}

/// The client for the short checks. The pre-release channel reads a whole page
/// of releases, assets and all, so it gets longer than the single release the
/// stable one reads. GitHub requires a User-Agent.
fn check_client(channel: Channel) -> Option<reqwest::Client> {
    let timeout = match channel {
        Channel::Stable => Duration::from_millis(2500),
        Channel::Prerelease => Duration::from_secs(8),
    };
    reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(concat!("hoard-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()
}

/// The newest version on `channel` (no cache). Best-effort; `None` on any
/// network/parse error or non-2xx.
pub async fn fetch_latest(channel: Channel) -> Option<String> {
    fetch_latest_from(GITHUB_API, channel).await
}

/// [`fetch_latest`] against another API root; for tests.
pub async fn fetch_latest_from(api: &str, channel: Channel) -> Option<String> {
    let client = check_client(channel)?;
    match discover(&client, api, channel).await {
        Ok(release) => Some(release.version()),
        Err(err) => {
            tracing::debug!(error = %format!("{err:#}"), ?channel, "update check failed");
            None
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Cache {
    latest: String,
    checked_at: u64,
    /// The channel `latest` was fetched on. A file from before channels existed
    /// has none and was fetched on the stable one.
    #[serde(default)]
    channel: Channel,
}

fn cache_path() -> Option<std::path::PathBuf> {
    Some(
        crate::config::CliConfig::cache_dir()
            .ok()?
            .join("update-check.json"),
    )
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The cached answer **for `channel`**. One fetched on the other channel is no
/// answer at all: after opting out, a cached `1.2.0-2` would otherwise keep the
/// amber dot lit for six hours on a version the user just said they don't want.
fn read_cache(path: &Path, channel: Channel) -> Option<Cache> {
    let txt = std::fs::read_to_string(path).ok()?;
    let cache: Cache = serde_json::from_str(&txt).ok()?;
    (cache.channel == channel).then_some(cache)
}

fn write_cache(path: &Path, latest: &str, channel: Channel) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let c = Cache {
        latest: latest.to_string(),
        checked_at: now_secs(),
        channel,
    };
    if let Ok(txt) = serde_json::to_string(&c) {
        let _ = std::fs::write(path, txt);
    }
}

/// Latest version on `channel`, served from the on-disk cache when it's fresh
/// (< 6h) and was fetched on the same channel, otherwise re-fetched and
/// re-cached. Best-effort: it falls back to a stale cache value (same channel
/// only) if the refresh fails, or `None` if there's nothing to go on.
pub async fn cached_latest(channel: Channel) -> Option<String> {
    match cache_path() {
        Some(path) => cached_latest_in(&path, GITHUB_API, channel).await,
        None => fetch_latest(channel).await,
    }
}

async fn cached_latest_in(path: &Path, api: &str, channel: Channel) -> Option<String> {
    if let Some(c) = read_cache(path, channel) {
        if now_secs().saturating_sub(c.checked_at) < CACHE_TTL.as_secs() {
            return Some(c.latest);
        }
        // Stale: try to refresh, but keep the old answer if the network is down.
        return match fetch_latest_from(api, channel).await {
            Some(v) => {
                write_cache(path, &v, channel);
                Some(v)
            }
            None => Some(c.latest),
        };
    }
    let v = fetch_latest_from(api, channel).await?;
    write_cache(path, &v, channel);
    Some(v)
}

/// The newest version on the user's channel if it's ahead of this binary, else
/// `None`. What the status panel uses to decide the amber dot. Uses the cache,
/// so it's instant on repeated runs.
pub async fn available_update() -> Option<String> {
    let latest = cached_latest(Channel::from_prefs()).await?;
    is_newer(&latest, current()).then_some(latest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_compares_semver_not_lexically() {
        assert!(is_newer("1.0.10", "1.0.9"));
        assert!(is_newer("1.1.0", "1.0.99"));
        assert!(is_newer("2.0.0", "1.9.9"));
        assert!(!is_newer("1.0.3", "1.0.3"));
        assert!(!is_newer("1.0.2", "1.0.3"));
    }

    /// The ordering the pre-release channel stands on:
    /// `1.1.7 < 1.2.0-1 < 1.2.0-2 < 1.2.0`.
    #[test]
    fn prereleases_order_between_their_neighbours() {
        let chain = ["1.1.7", "1.2.0-1", "1.2.0-2", "1.2.0"];
        for (i, lower) in chain.iter().enumerate() {
            for higher in &chain[i + 1..] {
                assert!(is_newer(higher, lower), "{higher} > {lower}");
                assert!(!is_newer(lower, higher), "{lower} < {higher}");
            }
            assert!(!is_newer(lower, lower), "{lower} is not newer than itself");
        }
        // Numeric identifiers compare as numbers, not text.
        assert!(is_newer("1.2.0-10", "1.2.0-9"));
        // Build metadata carries no precedence.
        assert!(!is_newer("1.2.0+build.5", "1.2.0"));
    }

    #[test]
    fn tolerates_v_prefix() {
        assert_eq!(parse("v1.0.4"), parse("1.0.4"));
        assert!(parse("v1.0.4").is_some());
        assert!(is_newer("v1.0.4", "1.0.3"));
        assert!(is_newer("v1.2.0", "v1.2.0-2"));
        assert!(is_newer(" 1.2.0-2 ", "v1.2.0-1"));
    }

    #[test]
    fn unparseable_never_nags() {
        assert!(!is_newer("garbage", "1.0.3"));
        assert!(!is_newer("1.0.4", "nightly"));
        assert!(!is_newer("1.3", "1.2.0"));
        assert!(!is_newer("", "1.2.0"));
    }

    // ---- discovery by channel, against a canned GitHub

    use crate::testing::{canned_github, release_json, LATEST_PATH, LIST_PATH};

    /// GitHub as it looks the day a second pre-release ships: `latest` still
    /// names the last full release, and the list has everything, newest first,
    /// plus a draft nobody should see.
    async fn github_with_prereleases() -> (String, crate::testing::Asked) {
        canned_github(vec![
            (
                LATEST_PATH,
                release_json("v1.1.7", false, false).to_string(),
            ),
            (
                LIST_PATH,
                serde_json::json!([
                    release_json("v1.3.0", false, true),
                    release_json("v1.2.0-2", true, false),
                    release_json("v1.2.0-1", true, false),
                    release_json("nightly", true, false),
                    release_json("v1.1.7", false, false),
                ])
                .to_string(),
            ),
        ])
        .await
    }

    #[tokio::test]
    async fn stable_asks_for_the_latest_release_only() {
        let (api, asked) = github_with_prereleases().await;
        let client = reqwest::Client::new();
        let rel = discover(&client, &api, Channel::Stable).await.unwrap();
        assert_eq!(rel.version(), "1.1.7");
        assert_eq!(*asked.lock().unwrap(), vec![LATEST_PATH.to_string()]);
    }

    #[tokio::test]
    async fn prerelease_takes_the_highest_published_semver() {
        let (api, asked) = github_with_prereleases().await;
        let client = reqwest::Client::new();
        let rel = discover(&client, &api, Channel::Prerelease).await.unwrap();
        // Not the draft `1.3.0`, not the unparseable `nightly`, not `1.1.7`.
        assert_eq!(rel.version(), "1.2.0-2");
        assert_eq!(rel.assets.len(), 1, "the files come along");
        assert_eq!(*asked.lock().unwrap(), vec![LIST_PATH.to_string()]);
    }

    /// Opted in and a full release ships: it beats every pre-release of its own
    /// version, so the tester lands on `1.2.0` rather than staying on `-2`.
    #[tokio::test]
    async fn prerelease_channel_still_takes_the_full_release_when_it_ships() {
        let (api, _) = canned_github(vec![(
            LIST_PATH,
            serde_json::json!([
                release_json("v1.2.0-2", true, false),
                release_json("v1.2.0", false, false),
                release_json("v1.2.0-1", true, false),
            ])
            .to_string(),
        )])
        .await;
        assert_eq!(
            fetch_latest_from(&api, Channel::Prerelease)
                .await
                .as_deref(),
            Some("1.2.0")
        );
    }

    #[tokio::test]
    async fn an_empty_or_failed_listing_is_no_answer() {
        let (api, _) = canned_github(vec![(LIST_PATH, "[]".to_string())]).await;
        assert_eq!(fetch_latest_from(&api, Channel::Prerelease).await, None);
        // No route for `latest`: a 404 is no answer either.
        assert_eq!(fetch_latest_from(&api, Channel::Stable).await, None);
    }

    #[tokio::test]
    async fn the_installer_path_follows_the_channel() {
        let (api, _) = github_with_prereleases().await;
        let (stable, _) = crate::install::fetch::release_assets_from(&api, None, Channel::Stable)
            .await
            .unwrap();
        let (pre, assets) =
            crate::install::fetch::release_assets_from(&api, None, Channel::Prerelease)
                .await
                .unwrap();
        assert_eq!(stable, "1.1.7");
        assert_eq!(pre, "1.2.0-2");
        assert_eq!(assets[0].name, "hoard_1.2.0-2_amd64.deb");
    }

    /// The cache answers only the channel that filled it: after switching, the
    /// other channel's answer is fetched, not served from the file.
    #[tokio::test]
    async fn the_update_check_cache_is_keyed_by_channel() {
        let (api, asked) = github_with_prereleases().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-check.json");

        assert_eq!(
            cached_latest_in(&path, &api, Channel::Stable)
                .await
                .as_deref(),
            Some("1.1.7")
        );
        assert_eq!(
            cached_latest_in(&path, &api, Channel::Prerelease)
                .await
                .as_deref(),
            Some("1.2.0-2"),
            "a stable answer must not be served on the pre-release channel"
        );
        // Fresh now, for this channel: served from disk.
        assert_eq!(
            cached_latest_in(&path, &api, Channel::Prerelease)
                .await
                .as_deref(),
            Some("1.2.0-2")
        );
        // And back: the file holds the pre-release answer, so stable asks again.
        assert_eq!(
            cached_latest_in(&path, &api, Channel::Stable)
                .await
                .as_deref(),
            Some("1.1.7")
        );
        assert_eq!(
            *asked.lock().unwrap(),
            vec![
                LATEST_PATH.to_string(),
                LIST_PATH.to_string(),
                LATEST_PATH.to_string()
            ]
        );
    }

    /// A cache written before channels existed was a stable answer.
    #[test]
    fn a_cache_from_before_channels_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-check.json");
        std::fs::write(&path, r#"{"latest":"1.1.7","checked_at":1}"#).unwrap();
        assert_eq!(
            read_cache(&path, Channel::Stable)
                .map(|c| c.latest)
                .as_deref(),
            Some("1.1.7")
        );
        assert!(read_cache(&path, Channel::Prerelease).is_none());
    }

    #[test]
    fn the_channel_follows_the_pref() {
        assert_eq!(Channel::from_prerelease(false), Channel::Stable);
        assert_eq!(Channel::from_prerelease(true), Channel::Prerelease);
        assert_eq!(Channel::default(), Channel::Stable);
    }
}
