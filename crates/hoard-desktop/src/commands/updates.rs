//! Update checks for the desktop client and the user's self-hosted server.
//!
//! Two independent probes:
//!
//! - **Client**: asks GitHub for the newest release on the user's update
//!   channel (`hoard_agent::update::discover`: the "latest release" by default,
//!   the highest pre-release or release when they opted in) and compares it to
//!   our compile-time `CARGO_PKG_VERSION` by SemVer precedence
//!   (`hoard_agent::update::is_newer`), so a pre-release such as `1.2.0-1` sits
//!   below `1.2.0` and above `1.1.7`.
//! - **Server**: hits the user's `<server>/v1/health` (anonymous endpoint)
//!   to read `version`, then compares it with the newest full release (the
//!   stable channel, the only one `hoard-server upgrade` takes), so the UI
//!   nudges the user to upgrade their server only when there is something it
//!   can upgrade to.
//!
//! Both probes are best-effort: a GitHub outage or a self-hosted server
//! that's offline must not break Settings. We swallow errors and report
//! `available = false` instead.

use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Manager, State};

use crate::commands::error::AppError;
use crate::state::AppState;

/// Reported status for one component (client or server).
#[derive(Debug, Clone, Serialize)]
pub struct ComponentUpdate {
    /// Currently-running version (`CARGO_PKG_VERSION` for the client, the
    /// `/v1/health` `version` field for the server).
    pub current: String,
    /// Latest known version, if we could fetch it. `None` means the probe
    /// failed, and the UI should fall back to "no update info" rather than
    /// "you're up to date".
    pub latest: Option<String>,
    /// `true` when `latest` is strictly greater than `current` by SemVer
    /// precedence. An unparseable version on either side is never "available".
    pub available: bool,
    /// Human-readable error from the failed probe, for the Logs view.
    /// Never shown to end users on its own.
    pub error: Option<String>,
}

/// Combined wire shape for the Settings page's "Updates" card.
#[derive(Debug, Clone, Serialize)]
pub struct UpdateReport {
    pub client: ComponentUpdate,
    /// `None` when the user is signed out (no known server URL to probe).
    pub server: Option<ComponentUpdate>,
}

/// A release as discovery returns it. For `check_for_updates` we only need the
/// tag; for `apply_desktop_update` we also need to pick the right downloadable
/// asset.
use hoard_agent::update::Release as GhRelease;

/// The release's files are described by `hoard_agent::install::fetch`: the same
/// GitHub JSON the terminal reads, and having two structs for it is how two updaters
/// that should do the same thing end up drifting apart.
use hoard_agent::install::fetch::Asset as GhAsset;

/// `/v1/health` shape (mirrors `crates/hoard-server/src/routes/health.rs`).
#[derive(serde::Deserialize)]
struct HealthResp {
    version: String,
}

const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// The result of checking a freshly downloaded installer against the release key.
///
/// The key and the verification live in `hoard_agent::install::fetch`, once: two
/// copies of a trust key are two places to rotate it, and one place to forget.
enum SigCheck {
    /// A `<asset>.minisig` was present and its signature matched the bytes.
    Verified,
    /// The release shipped no signature for this asset. We refuse to *auto*-run
    /// (pkexec-as-root) an unverified binary, but still hand the user the
    /// download so they can install it themselves. Transitional: every signed
    /// release takes the `Verified` path.
    Unsigned,
}

// ---- the automatic update: the window is a view, not the owner
//
// What looks, downloads and applies is the service (`hoardd::updater`), for the
// same reason it owns the engine: it is the only thing that is always there. A
// machine whose app has been closed for two weeks has to update anyway, and a
// window cannot promise that.
//
// What stays here are the two things only a window can do:
//
// 1. **Showing** where it stands, including the ugly case: a native package that
//    needs a privilege dialog the background cycle will not open.
// 2. **Being in front of somebody.** `apply_staged_update` is the permission: when
//    the window asks, there is a human at the keyboard and polkit has somebody to
//    ask.
//
// That is where "it updates when you open it" comes from: at start the app asks for
// the state and, when something is downloaded, applies it before letting things
// carry on. By the time you open it there is no download left, only a `rename`.

/// How the update is going, exactly as the service tells it.
///
/// `hoard_core::ipc::UpdateState`'s shape is forwarded rather than translated: the
/// wire is already designed to be read by an interface (named phases, typed
/// reasons, the deadline), and a second shape would be one more place to fall
/// behind.
#[tauri::command]
pub async fn update_status(
    state: State<'_, AppState>,
) -> Result<Option<hoard_core::ipc::UpdateState>, String> {
    match state.daemon.update_state().await {
        Ok(state) => Ok(Some(state)),
        Err(err) => {
            // With no service (or one older than this window, which lasts as long
            // as its relief takes) there is no state to show. `None` and not an
            // error: the app starts anyway, and this screen's older updater is still
            // the safety net.
            tracing::debug!(error = %format!("{err:#}"), "updates: the service didn't report its update state");
            Ok(None)
        }
    }
}

/// Applies whatever the service has downloaded, now. **This is the road with a
/// human in front of it**: it is the only one a `.deb` or an `.rpm` ever gets
/// installed down, because it is the only one where polkit's dialog has somebody to
/// ask.
///
/// It returns straight away with the state of the moment; installing carries on and
/// is followed with [`update_status`].
#[tauri::command]
pub async fn apply_staged_update(
    state: State<'_, AppState>,
    version: Option<String>,
) -> Result<hoard_core::ipc::UpdateState, AppError> {
    state.daemon.apply_update(version).await.map_err(|err| {
        AppError::new("updates.error.title", "updates.error.unknown")
            .with_detail(format!("{err:#}"))
    })
}

/// "Not now", for `hours`. It does not move the deadline: postponing delays the
/// question, not the deadline, which is exactly what makes the deadline mean
/// something.
#[tauri::command]
pub async fn snooze_update(
    state: State<'_, AppState>,
    hours: u32,
) -> Result<hoard_core::ipc::UpdateState, AppError> {
    state.daemon.snooze_update(hours).await.map_err(|err| {
        AppError::new("updates.error.title", "updates.error.unknown")
            .with_detail(format!("{err:#}"))
    })
}

/// Closes this window and opens the freshly installed copy.
///
/// The update screen asks for it when **the service has already been relieved and
/// the window is the one left behind**: the new binary has been on disk for a while,
/// but a live process does not change its executable. It reuses the same relief the
/// window's own updater uses ([`relaunch_then_exit`]), with the same care about the
/// `" (deleted)"` the kernel hangs off `/proc/self/exe` when the file we are running
/// has already been replaced, which is exactly the case here.
#[tauri::command]
pub async fn restart_app(app: AppHandle) {
    let exe_before = std::env::current_exe().ok().map(sanitize_exe_path);
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        // Un respiro para que la respuesta del comando llegue al WebView antes
        // de cortarle el proceso por debajo.
        tokio::time::sleep(Duration::from_millis(300)).await;
        relaunch_then_exit(&app2, exe_before);
    });
}

/// Tauri command. Pulls the latest GitHub release in parallel with the
/// server health probe (when logged in). Returns both halves so the UI
/// can render two badges side-by-side.
#[tauri::command]
pub async fn check_for_updates(state: State<'_, AppState>) -> Result<UpdateReport, String> {
    let server_url = state
        .user
        .lock()
        .unwrap()
        .as_ref()
        .map(|u| u.server_url.clone());

    let (client, server) = tokio::join!(probe_client(), async {
        match server_url {
            Some(url) => Some(probe_server(url).await),
            None => None,
        }
    });

    Ok(UpdateReport { client, server })
}

async fn probe_client() -> ComponentUpdate {
    let current = CLIENT_VERSION.to_string();
    match fetch_gh_latest().await {
        Ok(tag) => {
            // Strip the leading "v" if present so the comparison matches
            // CARGO_PKG_VERSION's bare semver string.
            let latest = tag.trim_start_matches('v').to_string();
            let available = is_newer(&latest, &current);
            ComponentUpdate {
                current,
                latest: Some(latest),
                available,
                error: None,
            }
        }
        Err(e) => ComponentUpdate {
            current,
            latest: None,
            available: false,
            error: Some(e),
        },
    }
}

/// The server's badge. A server upgrades only to full releases
/// (`hoard-server upgrade` has no pre-release channel), so it is compared with
/// the newest **stable** release, not with this client: on a client that
/// follows pre-releases the old comparison lit the badge for an upgrade the
/// server could never take. The stable answer comes through the shared
/// update-check cache (six hours), so this adds at most one GitHub request per
/// six hours, and none on a client whose own probe already asked on stable.
async fn probe_server(url: String) -> ComponentUpdate {
    let (health, newest_stable) = tokio::join!(
        fetch_server_health(&url),
        hoard_agent::update::cached_latest(hoard_agent::update::Channel::Stable)
    );
    match health {
        Ok(server_version) => server_component(server_version, newest_stable),
        Err(e) => ComponentUpdate {
            current: "?".to_string(),
            latest: None,
            available: false,
            error: Some(e),
        },
    }
}

/// Pure half of [`probe_server`]: the server against the newest full release.
fn server_component(server_version: String, newest_stable: Option<String>) -> ComponentUpdate {
    match newest_stable {
        Some(latest) => ComponentUpdate {
            available: is_newer(&latest, &server_version),
            current: server_version,
            latest: Some(latest),
            error: None,
        },
        None => ComponentUpdate {
            current: server_version,
            latest: None,
            available: false,
            error: Some("couldn't learn the newest full release".to_string()),
        },
    }
}

async fn fetch_gh_latest() -> Result<String, String> {
    fetch_gh(PROBE_TIMEOUT).await.map(|r| r.tag_name)
}

/// A full release fetch, used by `apply_desktop_update` to discover the asset
/// list at install time (we don't cache it because the user might leave the
/// app open for days between detection and applying).
async fn fetch_gh_release() -> Result<GhRelease, String> {
    fetch_gh(Duration::from_secs(20)).await
}

/// The newest release on the channel the user chose, read from the prefs now so
/// the switch in Settings applies to the very next probe. The same discovery the
/// service and the CLI use, so the window cannot offer a version they would not.
async fn fetch_gh(timeout: Duration) -> Result<GhRelease, String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("hoard-desktop/", env!("CARGO_PKG_VERSION")))
        .timeout(timeout)
        .build()
        .map_err(|e| e.to_string())?;
    let channel = hoard_agent::update::Channel::from_prefs();
    hoard_agent::update::discover(&client, hoard_agent::update::GITHUB_API, channel)
        .await
        .map_err(|e| format!("{e:#}"))
}

async fn fetch_server_health(server_url: &str) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("hoard-desktop/", env!("CARGO_PKG_VERSION")))
        .timeout(PROBE_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let url = format!("{}/v1/health", server_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    let h: HealthResp = resp.json().await.map_err(|e| e.to_string())?;
    Ok(h.version)
}

/// SemVer precedence, shared with the service and the CLI: the window must not
/// disagree with them about whether `1.2.0` is newer than `1.2.0-1`.
use hoard_agent::update::is_newer;

/// Outcome of `apply_desktop_update`. The UI uses `kind` to decide what to
/// show: on `installer_launched` we close the app so the OS installer can
/// replace it; on `downloaded` we tell the user where the file is.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApplyOutcome {
    /// We spawned the platform installer (pkexec dpkg / msiexec / `open` on
    /// macOS). The UI should prompt the user to wait + restart.
    InstallerLaunched { path: String, version: String },
    /// We downloaded the asset but couldn't auto-launch the installer. The
    /// UI surfaces the path so the user can run it manually.
    Downloaded { path: String, version: String },
    /// A newer release appeared between the moment the modal showed the user a
    /// version and the moment they confirmed. We abort *without downloading*
    /// the stale one and report the version that's actually latest now so the
    /// UI can refresh and re-offer it. Never install an older build than what
    /// GitHub currently calls "latest".
    Superseded { latest: String },
}

/// What to do with the release fetched at confirm time.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Newer than this build, and the version the user agreed to.
    Install,
    /// Newer than this build, but not the version the modal showed: re-offer.
    Superseded,
    /// Not newer than this build. Installing it would be a downgrade or a
    /// reinstall; this happens after opting out of pre-releases with a badge
    /// still showing one.
    NotNewer,
}

/// Pure so it can be tested without Tauri. `expected` is the version the modal
/// showed; `None` from an older UI means "whatever is newest", which is still
/// never older than what runs.
fn install_verdict(fetched: &str, current: &str, expected: Option<&str>) -> Verdict {
    if !is_newer(fetched, current) {
        return Verdict::NotNewer;
    }
    let agreed = expected.is_none_or(|e| {
        matches!(
            (
                hoard_agent::update::parse(fetched),
                hoard_agent::update::parse(e),
            ),
            (Some(a), Some(b)) if a.cmp_precedence(&b).is_eq()
        )
    });
    if agreed {
        Verdict::Install
    } else {
        Verdict::Superseded
    }
}

/// Tauri command. Downloads the right release asset for this OS and tries to
/// launch the platform installer.
///
/// This is the "yes" path of the in-app update modal. We deliberately keep
/// the privilege-escalation choice on the OS: `pkexec` for Linux .deb,
/// `msiexec` for Windows .msi, `open` for macOS .dmg. Each pops the system's
/// usual auth prompt; we never ask the user for a password ourselves.
#[tauri::command]
pub async fn apply_desktop_update(
    app: AppHandle,
    expected_version: Option<String>,
) -> Result<ApplyOutcome, AppError> {
    // Network/HTTP failures fetching the release feed: surface as "unknown",
    // since this branch fires before we even know what version we're trying
    // to install. The raw `reqwest` message goes into `detail`.
    let release = fetch_gh_release().await.map_err(|e| {
        AppError::new("updates.error.title", "updates.error.unknown").with_detail(e)
    })?;
    let version = release.tag_name.trim_start_matches('v').to_string();

    // Re-check at confirm time against what is running and what the modal
    // showed. The badge can be stale (up to 30 min, or longer if the user sat
    // on the dialog, or switched the update channel in between), so the release
    // fetched now is installed only when it is both newer than this build and
    // the one the user agreed to.
    match install_verdict(&version, CLIENT_VERSION, expected_version.as_deref()) {
        Verdict::Install => {}
        Verdict::Superseded => {
            tracing::info!(
                expected = ?expected_version,
                latest = %version,
                "apply_desktop_update: a different release is current now, aborting stale install"
            );
            return Ok(ApplyOutcome::Superseded { latest: version });
        }
        Verdict::NotNewer => {
            tracing::info!(
                current = CLIENT_VERSION,
                latest = %version,
                "apply_desktop_update: the current release is not newer than this build, refusing"
            );
            return Err(
                AppError::new("updates.error.title", "updates.error.not_newer").with_detail(
                    format!("running v{CLIENT_VERSION}, the current release is v{version}"),
                ),
            );
        }
    }

    let asset = pick_asset(&release.assets)
        .ok_or_else(|| {
            let assets = release
                .assets
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            AppError::new("updates.error.title", "updates.error.no_installer")
                .with_detail(format!("v{version} · Assets: {assets}"))
        })?
        .clone();

    // Download into the OS download dir so the file is findable later if the
    // installer asks the user to "Save As".
    let download_dir = app
        .path()
        .download_dir()
        .or_else(|_| app.path().temp_dir())
        .map_err(|e| {
            AppError::new("updates.error.title", "updates.error.download_failed")
                .with_detail(format!("locating a writable directory: {e}"))
        })?;
    tokio::fs::create_dir_all(&download_dir)
        .await
        .map_err(|e| {
            AppError::new("updates.error.title", "updates.error.download_failed")
                .with_detail(e.to_string())
        })?;
    let dest = download_dir.join(&asset.name);

    // Download into memory first so the signature is checked BEFORE the bytes
    // ever hit disk or an installer.
    let bytes = download_bytes(&asset.url).await.map_err(|e| {
        AppError::new("updates.error.title", "updates.error.download_failed").with_detail(e)
    })?;

    // Gate the privileged auto-install behind a minisign check against the
    // embedded release key, the same guarantee the server's `upgrade` gives.
    // A *tampered* artifact (signature present but wrong) aborts here and is
    // never written; an as-yet-unsigned release degrades to a manual download
    // rather than being run as root.
    let sig = verify_installer_signature(&release.assets, &asset.name, &bytes)
        .await
        .map_err(|detail| {
            AppError::new("updates.error.title", "updates.error.signature_invalid")
                .with_detail(detail)
        })?;

    tokio::fs::write(&dest, &bytes).await.map_err(|e| {
        AppError::new("updates.error.title", "updates.error.download_failed")
            .with_detail(format!("writing {}: {e}", dest.display()))
    })?;

    let path_str = dest.to_string_lossy().to_string();

    // Unsigned release: we have the file on disk but won't pkexec-install it.
    // Surface it as a manual download so the user can install it deliberately.
    if matches!(sig, SigCheck::Unsigned) {
        tracing::warn!(
            asset = %asset.name,
            "release asset has no .minisig; refusing to auto-run the installer as root, \
             offering manual install instead"
        );
        return Ok(ApplyOutcome::Downloaded {
            path: path_str,
            version,
        });
    }

    // Capture our own binary path *before* the installer runs. On Linux the
    // .deb install makes dpkg unlink+recreate /usr/bin/hoard-desktop, after
    // which `std::env::current_exe()` resolves to ".../hoard-desktop (deleted)"
    // and spawning that path fails, so the relaunch silently no-ops. That is the
    // "I have to close and reopen it for the version to change" bug. Snapshotting
    // the path here (and sanitizing a stale " (deleted)" suffix just in case)
    // gives us a stable path to the freshly-installed binary.
    let exe_before = std::env::current_exe().ok().map(sanitize_exe_path);

    // Try to launch the platform installer. If that fails, we still succeeded
    // at *downloading* the update, so report Downloaded with the path so the
    // user can do it themselves.
    match launch_installer(&dest).await {
        Ok(()) => {
            // Quit the old process shortly after we return. Without this the
            // user is stuck running 1.3.5 even though dpkg/msiexec already
            // dropped 1.4.0 on disk: that's exactly what the 1.3.5 to 1.4.0
            // upgrade looked like in the wild on Linux (the app just carried
            // on regardless) and is what makes Windows refuse to overlay the .exe
            // until the running copy goes away. The small delay lets the
            // frontend paint the "installed, reopen" toast before we cut the
            // process. On Linux we additionally relaunch the freshly-
            // installed binary so the user doesn't have to dig around the
            // app menu, since the .deb sits at the same /usr/bin path so
            // `current_exe()` already points at the new binary, and
            // `setsid` detaches it from our dying process group.
            let app2 = app.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(Duration::from_millis(1500)).await;
                relaunch_then_exit(&app2, exe_before);
            });
            Ok(ApplyOutcome::InstallerLaunched {
                path: path_str,
                version,
            })
        }
        Err(e) => {
            tracing::warn!(error = %e, "installer launch failed, falling back to manual");
            Ok(ApplyOutcome::Downloaded {
                path: path_str,
                version,
            })
        }
    }
}

/// Try to spawn the new binary detached from the current process, then exit.
/// Failures are logged and swallowed: the worst case is the user has to
/// reopen the app from their app menu, which is exactly the 1.4.0 status
/// quo we're trying to improve.
// `exe_before` is only consumed on the Linux branch (relaunching the binary after
// the .deb); on Windows and macOS the parameter is never read and would be an
// unused variable.
#[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
fn relaunch_then_exit(app: &AppHandle, exe_before: Option<std::path::PathBuf>) {
    #[cfg(target_os = "linux")]
    {
        // Prefer the path we snapshotted before the install. Fall back to a
        // freshly-sanitized `current_exe()` if we somehow didn't capture one.
        let exe = exe_before.or_else(|| std::env::current_exe().ok().map(sanitize_exe_path));
        if let Some(exe) = exe {
            use std::process::{Command, Stdio};
            // `setsid` puts the child in its own session so it survives our
            // imminent `app.exit(0)`. If `setsid` isn't on PATH (unusual on
            // any modern Linux) we just skip the relaunch; exit is still
            // useful on its own because the old process was blocking the
            // user from running the new binary anyway.
            let spawn = Command::new("setsid")
                .arg(&exe)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            if let Err(e) = spawn {
                tracing::warn!(error = %e, exe = %exe.display(), "couldn't relaunch with setsid; user will have to reopen manually");
            }
        }
    }
    // On Windows we *don't* relaunch: msiexec is still running asynchronously
    // and the .exe is mid-replace. Starting a process from the path that's
    // being overwritten races msiexec and usually crashes. The user reopens
    // from the Start menu after the installer finishes.
    //
    // On macOS we leave it to the user too: `open` on the .dmg pops Finder
    // and the user drags into Applications.
    app.exit(0);
}

/// Strip a trailing " (deleted)" marker the Linux kernel appends to
/// `/proc/self/exe` once the on-disk binary has been unlinked (e.g. by dpkg
/// mid-upgrade). `current_exe()` reads that link, so without this we'd try to
/// spawn ".../hoard-desktop (deleted)" and fail.
fn sanitize_exe_path(exe: std::path::PathBuf) -> std::path::PathBuf {
    if let Some(s) = exe.to_str() {
        if let Some(stripped) = s.strip_suffix(" (deleted)") {
            return std::path::PathBuf::from(stripped);
        }
    }
    exe
}

/// The release file this machine should get.
///
/// The choice is not made here. `hoard_agent::install` looks at the machine
/// (available package manager, read-only root, whether it can elevate without
/// hanging) and the route comes out of that; `fetch::asset_for` translates the route
/// into a file. This used to decide on its own by looking only at the distro, which
/// is why on an atomic image (SteamOS, Bazzite) it offered an `.rpm` there is no way
/// to apply: `rpm` is on the `PATH`, but `/usr` is read-only. Those machines get the
/// AppImage now, the same as from the terminal.
fn pick_asset(assets: &[GhAsset]) -> Option<&GhAsset> {
    let probe = hoard_agent::install::Probe::read();
    let delivery = hoard_agent::install::resolve_delivery(&probe);
    hoard_agent::install::fetch::asset_for(delivery, assets)
}

async fn download_bytes(url: &str) -> Result<Vec<u8>, String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("hoard-desktop/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| e.to_string())?;
    let bytes = client
        .get(url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .bytes()
        .await
        .map_err(|e| e.to_string())?;
    Ok(bytes.to_vec())
}

/// Checks the freshly downloaded installer against the release key.
///
/// - `Ok(Verified)`: a signature is present and valid, so it can be auto-installed.
/// - `Ok(Unsigned)`: the release publishes no signature for this file, and the
///   caller stays on a manual download rather than running anything as root.
/// - `Err(detail)`: there was a signature and it does NOT match, or it could not be
///   read. It aborts and the download is discarded: this is the tampered-artifact
///   case.
///
/// The cryptography and the key are `install::fetch`'s, shared with the terminal.
/// What stays here is the tolerance for the `Unsigned` case, which is this updater's
/// decision and not the verification's.
async fn verify_installer_signature(
    assets: &[GhAsset],
    asset_name: &str,
    bytes: &[u8],
) -> Result<SigCheck, String> {
    let sig_name = format!("{asset_name}.minisig");
    let Some(sig_asset) = assets.iter().find(|a| a.name == sig_name) else {
        return Ok(SigCheck::Unsigned);
    };

    let client = reqwest::Client::builder()
        .user_agent(concat!("hoard-desktop/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let sig_text = client
        .get(&sig_asset.url)
        .send()
        .await
        .map_err(|e| format!("downloading signature: {e}"))?
        .error_for_status()
        .map_err(|e| format!("signature download status: {e}"))?
        .text()
        .await
        .map_err(|e| format!("reading signature: {e}"))?;

    hoard_agent::install::fetch::verify(bytes, &sig_text)
        .map_err(|e| format!("{e:#}"))
        .map(|()| SigCheck::Verified)
}

#[cfg(target_os = "linux")]
async fn launch_installer(path: &std::path::Path) -> Result<(), String> {
    // pkexec pops the polkit auth dialog and runs the package manager as root.
    // If the user cancels, it never runs and we get a non-zero exit; we treat
    // that as an error so the UI can fall back to "Downloaded".
    let p = path.to_string_lossy().to_string();
    let is_rpm = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("rpm"));

    // For .rpm prefer dnf (resolves deps) and fall back to plain rpm -U; for
    // .deb use dpkg -i (deps already vendored in the Tauri bundle).
    let args: Vec<&str> = if is_rpm {
        if std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .any(|d| d.join("dnf").is_file())
        {
            vec!["dnf", "install", "-y", &p]
        } else {
            vec!["rpm", "-U", "--force", &p]
        }
    } else {
        vec!["dpkg", "-i", &p]
    };

    let status = tokio::process::Command::new("pkexec")
        .args(&args)
        .status()
        .await
        .map_err(|e| format!("spawning pkexec: {e}"))?;
    if !status.success() {
        return Err(format!("{} exited with status {status}", args.join(" ")));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
async fn launch_installer(path: &std::path::Path) -> Result<(), String> {
    let p = path.to_string_lossy().to_string();
    // Two bundle formats, two ways to start them. `pick_asset` prefers the
    // NSIS `-setup.exe` (it's the one that stops `hoardd` before overwriting
    // it), which runs directly; an `.msi` still has to go through `msiexec`,
    // and handing one to the other just fails.
    if p.to_ascii_lowercase().ends_with(".msi") {
        tokio::process::Command::new("msiexec")
            .args(["/i", &p])
            .spawn()
            .map_err(|e| format!("spawning msiexec: {e}"))?;
    } else {
        tokio::process::Command::new(&p)
            .spawn()
            .map_err(|e| format!("spawning installer: {e}"))?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
async fn launch_installer(path: &std::path::Path) -> Result<(), String> {
    let p = path.to_string_lossy().to_string();
    tokio::process::Command::new("open")
        .arg(&p)
        .spawn()
        .map_err(|e| format!("spawning open: {e}"))?;
    Ok(())
}

// ---- the server upgrade: in-app, for local servers only
//
// Why this exists: pre-1.4.7 the Server card in Settings only knew how to
// *copy* `sudo hoard-server upgrade` to the clipboard. That's a sensible
// fallback when the server is on another box (you have to SSH anyway), but
// for the common self-hosted case where the server runs on the same machine
// as the desktop app it's busywork: the user already has both binaries on
// disk and just wants a button.
//
// We piggy-back on the same `pkexec` flow the desktop installer uses: one
// graphical sudo prompt, no password handling in our own code, no need to
// stash a sudo password in prefs. The wrapper invocation runs the upgrade
// *and* restarts the service in a single auth prompt, so the user doesn't
// have to authenticate twice. If the upgrade succeeds but the restart
// fails (no systemd unit, manual launch, etc.) we still surface success
// with a "restart it yourself" hint.
//
// Gated to Linux because:
//   - `hoard-server upgrade` itself is Linux-x86_64 only (see
//     `crates/hoard-server/src/upgrade.rs::preferred_asset_name`).
//   - `pkexec` is the Linux polkit entry point; Windows/macOS would
//     need entirely different privilege-escalation plumbing.
// On any other platform `apply_server_update` returns the same error
// regardless of `is_local_server`, so the UI falls back to copy-command.

/// Outcome of `apply_server_update`. The UI uses `kind` to decide what to
/// show next: on `upgraded_and_restarted` the panel can flip straight to
/// "up to date"; on `upgraded` it tells the user to restart manually.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
// Only the Linux implementation builds these variants; on Windows and macOS the
// stub returns an error and never creates them, but the type has to exist
// cross-platform (it is the Tauri command's return and its serde shape).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub enum ServerUpgradeOutcome {
    /// The new binary is on disk *and* `systemctl restart hoard-server`
    /// returned 0. The user can keep using the app immediately.
    UpgradedAndRestarted { output: String },
    /// The new binary is on disk but the service restart didn't succeed,
    /// usually because the server isn't running under systemd. The user
    /// needs to restart it themselves with whatever supervisor they use.
    Upgraded {
        output: String,
        restart_error: String,
    },
}

/// Tauri command. Runs `hoard-server upgrade` (and `systemctl restart`)
/// through a single `pkexec` invocation so the user only authenticates
/// once. Only available on Linux; only meaningful when the server is on
/// the same machine. The UI is responsible for gating the button on
/// `is_local_server`, but we don't *require* it because the call simply
/// fails on a non-local box (the local `hoard-server` binary either
/// doesn't exist, or it does and the user is fine to upgrade their copy).
#[tauri::command]
pub async fn apply_server_update() -> Result<ServerUpgradeOutcome, AppError> {
    apply_server_update_impl().await
}

#[cfg(target_os = "linux")]
async fn apply_server_update_impl() -> Result<ServerUpgradeOutcome, AppError> {
    // We launch a shell so we can chain "upgrade && restart" into one
    // pkexec → one polkit prompt. `set -e` makes the script fail fast if
    // the upgrade itself returns non-zero (download failed, binary path
    // not writable, etc.) so we don't restart a half-upgraded server.
    //
    // We capture stdout+stderr so the UI can surface the friendly progress
    // output from `hoard-server upgrade` ("downloading", "Installed
    // hoard-server v…"). On Linux pkexec returns exit code 126 if the user
    // cancels the auth prompt and 127 if the underlying binary isn't found;
    // we surface both as a single human-readable error string.
    let upgrade = tokio::process::Command::new("pkexec")
        .args([
            "sh",
            "-c",
            // Resolve `hoard-server` through PATH inside the elevated
            // shell: polkit doesn't preserve the caller's PATH for the
            // binary it spawns, but `sh` does for *its* exec, and `sh`'s
            // default PATH includes the standard system bin dirs we ship to.
            "set -e; hoard-server upgrade",
        ])
        .output()
        .await
        .map_err(|e| {
            AppError::new("updates.error.title", "updates.error.server_pkexec_failed")
                .with_detail(format!("spawning pkexec: {e}"))
        })?;

    if !upgrade.status.success() {
        let stderr = String::from_utf8_lossy(&upgrade.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&upgrade.stdout).trim().to_string();
        // pkexec emits its own message for "Request dismissed" / "Not
        // authorized"; we pass that through verbatim so the UI can show
        // the user-facing reason rather than a generic "upgrade failed".
        let detail = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            format!("pkexec exited with status {}", upgrade.status)
        };
        return Err(
            AppError::new("updates.error.title", "updates.error.server_pkexec_failed")
                .with_detail(detail),
        );
    }

    let upgrade_output = String::from_utf8_lossy(&upgrade.stdout).trim().to_string();
    tracing::info!(?upgrade_output, "server upgrade pkexec call returned 0");

    // Try the restart in a second pkexec call. Polkit caches the auth
    // grant for ~5 minutes by default, so this typically won't re-prompt
    // but on a stricter polkit policy the user *might* see a second
    // prompt. We accept that as a price for keeping the chain trivial to
    // reason about (one pkexec per privileged op, no shell escapes).
    let restart = tokio::process::Command::new("pkexec")
        .args(["systemctl", "restart", "hoard-server"])
        .output()
        .await;

    match restart {
        Ok(out) if out.status.success() => Ok(ServerUpgradeOutcome::UpgradedAndRestarted {
            output: upgrade_output,
        }),
        Ok(out) => {
            let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let detail = if detail.is_empty() {
                format!("systemctl exited with status {}", out.status)
            } else {
                detail
            };
            tracing::warn!(error = %detail, "server upgraded but restart failed");
            Ok(ServerUpgradeOutcome::Upgraded {
                output: upgrade_output,
                restart_error: detail,
            })
        }
        Err(e) => Ok(ServerUpgradeOutcome::Upgraded {
            output: upgrade_output,
            restart_error: format!("spawning systemctl restart: {e}"),
        }),
    }
}

#[cfg(not(target_os = "linux"))]
async fn apply_server_update_impl() -> Result<ServerUpgradeOutcome, AppError> {
    Err(
        AppError::new("updates.error.title", "updates.error.server_unknown").with_detail(
            "In-app server upgrade is only supported on Linux today. \
             Run `sudo hoard-server upgrade` on the server host."
                .to_string(),
        ),
    )
}

// ---- the remote-triggered server upgrade: any OS, any machine (ADR 0017)
//
// Unlike `apply_server_update` (which runs `pkexec hoard-server upgrade` on
// *this* machine and therefore only helps when the server shares the box),
// this asks the server to upgrade *itself* over HTTP. The desktop can run on
// Windows and still upgrade a headless Linux server across the network.
//
// The server-side handler (`POST /v1/admin/upgrade`) only drops a marker file;
// a root systemd oneshot does the signed binary swap + restart. So all we do
// here is fire the authenticated request and then poll `/v1/health` until the
// reported version changes (the server is briefly unreachable mid-restart,
// which we tolerate) or a timeout elapses.

/// Outcome of `trigger_server_upgrade`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteUpgradeOutcome {
    /// The server came back on a new version within the poll window.
    Confirmed { version: String },
    /// The request was accepted (202) but we couldn't confirm the new
    /// version before the timeout: the server may still be restarting, or
    /// it was already on the latest signed release. The UI tells the user to
    /// re-check in a moment rather than treating this as a failure.
    Scheduled,
}

/// Tauri command. Sends `POST {server_url}/v1/admin/upgrade` with the saved
/// bearer token, then polls `/v1/health` for a version change. Requires an
/// admin token (the server returns 403 otherwise); the UI gates the button
/// on `is_admin` so this is a defence-in-depth check, not the primary one.
#[tauri::command]
pub async fn trigger_server_upgrade(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<RemoteUpgradeOutcome, AppError> {
    let server_url = state
        .user
        .lock()
        .unwrap()
        .as_ref()
        .map(|u| u.server_url.clone())
        .ok_or_else(|| {
            AppError::new("updates.error.title", "updates.error.server_not_logged_in")
        })?;
    // Lent by the service: the keyring item is its own (D.20).
    let token = crate::commands::auth::server_session(&app)
        .await
        .ok()
        .flatten()
        .map(|c| c.token)
        .ok_or_else(|| {
            AppError::new("updates.error.title", "updates.error.server_not_logged_in")
        })?;

    let base = server_url.trim_end_matches('/').to_string();
    let client = reqwest::Client::builder()
        .user_agent(concat!("hoard-desktop/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| {
            AppError::new("updates.error.title", "updates.error.unknown").with_detail(e.to_string())
        })?;

    // Snapshot the current version so we can detect the flip after restart.
    let before = fetch_server_health(&base).await.ok();

    let resp = client
        .post(format!("{base}/v1/admin/upgrade"))
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| {
            AppError::new("updates.error.title", "updates.error.server_unreachable")
                .with_detail(e.to_string())
        })?;

    let status = resp.status();
    if status == reqwest::StatusCode::FORBIDDEN {
        let detail = resp.text().await.unwrap_or_default();
        return Err(
            AppError::new("updates.error.title", "updates.error.server_forbidden")
                .with_detail(detail),
        );
    }
    if !status.is_success() {
        let detail = resp.text().await.unwrap_or_default();
        return Err(
            AppError::new("updates.error.title", "updates.error.server_unknown")
                .with_detail(format!("HTTP {status}: {detail}")),
        );
    }

    // Poll /v1/health for ~90s (30 × 3s). The server is unreachable for a few
    // seconds while systemd restarts it; we ignore probe errors and keep going.
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_secs(3)).await;
        if let Ok(version) = fetch_server_health(&base).await {
            if before.as_deref() != Some(version.as_str()) {
                return Ok(RemoteUpgradeOutcome::Confirmed { version });
            }
        }
    }

    Ok(RemoteUpgradeOutcome::Scheduled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hoard_agent::install::fetch::asset_for;
    use hoard_agent::install::Delivery;

    /// The architecture token Windows bundles carry on the machine running the
    /// test. Hardcoding `x64` tied the two tests below to an x86 runner: on
    /// aarch64 `pick_for_arch` recognises none of the candidates as its own and
    /// returns `None`, which is correct behaviour failing the test for the wrong
    /// reason. What is under test is the preference for NSIS over MSI, not the
    /// machine that compiled it.
    fn arch_token() -> &'static str {
        match std::env::consts::ARCH {
            "aarch64" => "arm64",
            _ => "x64",
        }
    }

    fn assets(names: &[&str]) -> Vec<GhAsset> {
        names
            .iter()
            .map(|n| GhAsset {
                name: (*n).to_string(),
                url: format!("https://example.invalid/{n}"),
            })
            .collect()
    }

    /// The release publishes both and the `.msi` comes FIRST in the list, so the
    /// order cannot come from the release: only the NSIS bundle carries the hook
    /// that stops `hoardd` before overwriting its `.exe`. The preference lives in
    /// `install::fetch` now, but the updater is what breaks when it changes, so the
    /// assertion stays here.
    #[test]
    fn windows_update_takes_the_nsis_installer_not_the_msi() {
        let arch = arch_token();
        let msi = format!("Hoard_1.1.0_{arch}_en-US.msi");
        let nsis = format!("Hoard_1.1.0_{arch}-setup.exe");
        let rel = assets(&[
            &msi,
            &format!("{msi}.sha256"),
            &nsis,
            &format!("{nsis}.sha256"),
        ]);
        assert_eq!(
            asset_for(Delivery::Nsis, &rel).map(|a| a.name.as_str()),
            Some(nsis.as_str())
        );
    }

    #[test]
    fn windows_falls_back_to_the_msi_when_theres_no_nsis() {
        let msi = format!("Hoard_1.1.0_{}_en-US.msi", arch_token());
        let rel = assets(&[&msi]);
        assert_eq!(
            asset_for(Delivery::Nsis, &rel).map(|a| a.name.as_str()),
            Some(msi.as_str())
        );
    }

    /// Every route takes its own file and no other. This used to be resolved by a
    /// list of suffixes with a cascading fallback (`.deb`, then `.rpm`, then the
    /// AppImage), which is what left a machine with an immutable root holding a
    /// package it could not apply. The machine decides the route before it gets
    /// here, and there is no cascade here to contradict it.
    #[test]
    fn each_delivery_takes_its_own_file_and_no_other() {
        let rel = assets(&["b.rpm", "a.deb", "c.AppImage"]);
        assert_eq!(
            asset_for(Delivery::Deb, &rel).map(|a| a.name.as_str()),
            Some("a.deb")
        );
        assert_eq!(
            asset_for(Delivery::Rpm, &rel).map(|a| a.name.as_str()),
            Some("b.rpm")
        );
        assert_eq!(
            asset_for(Delivery::AppImage, &rel).map(|a| a.name.as_str()),
            Some("c.AppImage")
        );
        assert!(asset_for(Delivery::Dmg, &rel).is_none());
    }

    #[test]
    fn newer_minor_beats_older() {
        assert!(is_newer("1.3.0", "1.2.5"));
        assert!(is_newer("2.0.0", "1.99.99"));
    }

    #[test]
    fn equal_versions_are_not_newer() {
        assert!(!is_newer("1.2.2", "1.2.2"));
    }

    #[test]
    fn double_digit_components_compare_correctly() {
        assert!(is_newer("1.10.0", "1.9.9"));
    }

    #[test]
    fn tolerates_v_prefix_and_prerelease() {
        assert!(is_newer("v1.3.0", "1.2.5"));
        assert!(is_newer("1.3.0-rc1", "1.2.5"));
    }

    /// `1.1.7 < 1.2.0-1 < 1.2.0-2 < 1.2.0`: the release beats its own
    /// pre-releases, which the old `(major, minor, patch)` tuple called equal.
    #[test]
    fn prereleases_order_below_their_release() {
        assert!(is_newer("1.2.0-1", "1.1.7"));
        assert!(is_newer("1.2.0-2", "1.2.0-1"));
        assert!(is_newer("1.2.0", "1.2.0-2"));
        assert!(is_newer("v1.2.0", "1.2.0-1"));
        assert!(!is_newer("1.2.0-2", "1.2.0"));
        assert!(!is_newer("1.2.0-1", "1.2.0-1"));
    }

    #[test]
    fn unparseable_is_never_newer() {
        assert!(!is_newer("garbage", "1.2.0"));
        assert!(!is_newer("1.3.0", "?"));
    }

    #[test]
    fn installs_only_the_newer_release_the_user_agreed_to() {
        assert_eq!(install_verdict("1.2.0", "1.2.0-1", Some("1.2.0")), Verdict::Install);
        assert_eq!(install_verdict("v1.2.0", "1.2.0-1", Some("1.2.0")), Verdict::Install);
        assert_eq!(install_verdict("1.2.0", "1.2.0-1", None), Verdict::Install);
        // A newer one landed since the modal opened.
        assert_eq!(install_verdict("1.2.1", "1.2.0-1", Some("1.2.0")), Verdict::Superseded);
        // Opted out with a stale badge: stable is older than what runs.
        assert_eq!(install_verdict("1.1.7", "1.2.0-1", Some("1.2.0-2")), Verdict::NotNewer);
        assert_eq!(install_verdict("1.1.7", "1.2.0-1", None), Verdict::NotNewer);
        // Same version: a reinstall, not an update.
        assert_eq!(install_verdict("1.2.0-1", "1.2.0-1", Some("1.2.0-1")), Verdict::NotNewer);
        // Newer than this build but older than what the modal promised.
        assert_eq!(install_verdict("1.2.0-2", "1.2.0-1", Some("1.2.0")), Verdict::Superseded);
    }

    /// An opted-in client on 1.2.0-2 must not flag a server on the newest
    /// full release: the server has nothing to upgrade to.
    #[test]
    fn the_server_is_compared_with_the_newest_full_release() {
        let up_to_date = server_component("1.1.7".into(), Some("1.1.7".into()));
        assert!(!up_to_date.available);
        assert_eq!(up_to_date.latest.as_deref(), Some("1.1.7"));

        let behind = server_component("1.1.6".into(), Some("1.1.7".into()));
        assert!(behind.available);

        let unknown = server_component("1.1.6".into(), None);
        assert!(!unknown.available, "no answer is not an upgrade");
        assert!(unknown.error.is_some());
    }
}
