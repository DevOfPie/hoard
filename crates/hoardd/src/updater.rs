//! **The service updates itself.**
//!
//! What checks for a new version is the service and not the window, for the same
//! reason the engine lives here (ADR 0021) and native notifications go out from
//! here (D.14.1): **it is the only thing that is always there**. The window was
//! closed, the terminal was not opened for two weeks, and the sync has still been
//! running for days with a bug that was fixed three releases ago.
//!
//! ## The split
//!
//! - The **policy** (when to download, when to apply, when it stops being
//!   optional) is pure and lives in `hoard_agent::install::auto`.
//! - The **mechanics** (which file, which signature, where it goes) live in
//!   `hoard_agent::install::{fetch, stage}`, shared with `hoard install`.
//! - What is left here is the loop: ask, decide, do, get relieved.
//!
//! ## What this loop never does
//!
//! **It opens no dialogs.** A background service that makes a polkit window appear
//! at three in the morning is worse than not updating. In the background cycle
//! everything runs `noninteractive`, so the routes that need a human (`.deb`,
//! `.rpm`, `.dmg`) only move when somebody asks from a client
//! ([`hoard_core::ipc::Request::ApplyUpdate`]), and then yes, with the dialog in
//! front of whoever just asked for it.
//!
//! Past the deadline it is tried anyway, but only down the routes that do not ask
//! (we are root already, or there is a `sudo` with a cached credential). Failing
//! that, the update stays marked mandatory and the first window to open resolves
//! it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use hoard_agent::install::auto::{self, Hold, Ledger, Situation, Stance};
use hoard_agent::install::{stage, Manifest};
use hoard_agent::supervisor::Finished;
use hoard_agent::update::{self, Channel};
use hoard_core::ipc::{UpdateHold, UpdatePhase, UpdateState};
use time::OffsetDateTime;

use crate::engine::Engine;

/// How often GitHub is asked in normal running.
///
/// Half an hour was the cadence of the window's amber badge and it is more than
/// enough here: the service does not close, so over a day that is 24 unauthenticated
/// requests against a limit of 60/h. What matters is not finding out soon, it is
/// finding out **always**.
const POLL: Duration = Duration::from_secs(60 * 60);

/// The short cadence while something is pending that could not be applied yet (a
/// game open, an upload halfway through). It probes the brake, not GitHub: the
/// version is not asked about again until [`POLL`] comes round.
const RETRY: Duration = Duration::from_secs(60);

/// A breather before the first cycle. The service's start already competes with the
/// engine's, the login's and the session's; a 90 MB download the moment you sign in
/// is exactly what not to do.
const WARMUP: Duration = Duration::from_secs(90);

/// The cap on consecutive failures before the attempts get spaced out. Without it,
/// a release that publishes no package for this architecture retries every minute
/// for ever, which is the compression hot loop (6 blobs with no terminal state,
/// retrying since July) written all over again.
const MAX_FAILURES: u32 = 5;

/// How long a **failed** version check waits before it is tried again.
///
/// A check that fails leaves no answer, so without its own clock it stays due
/// and runs on every cycle, and while something is pending that is every
/// [`RETRY`] (60 s): more than GitHub's unauthenticated limit of 60 an hour.
/// Fifteen minutes keeps it at four an hour, and a client that changes the
/// channel still gets a check at once ([`Updater::recheck`]).
const CHECK_BACKOFF: Duration = Duration::from_secs(15 * 60);

// ---- what the updater shows

/// The updater's shared view: what [`hoard_core::ipc::Request::UpdateStatus`]
/// answers.
///
/// It is an `Arc<Mutex<...>>` and not a channel because clients ask whenever they
/// feel like it; there is nobody to push to when nobody is connected, which is half
/// the time.
#[derive(Clone)]
pub struct Updater {
    inner: Arc<Mutex<Live>>,
    /// A client asked to apply now. It wakes the loop, which is what applies:
    /// applying from an IPC connection's thread would leave two applications
    /// treading on each other if the user clicked twice.
    poke: Arc<tokio::sync::Notify>,
}

#[derive(Default)]
struct Live {
    phase: Phase,
    latest: Option<String>,
    staged: Option<String>,
    deadline: Option<OffsetDateTime>,
    mandatory: bool,
    unattended: bool,
    last_error: Option<String>,
    /// The version a client asked to apply, waiting for the loop to pick it up.
    requested: Option<Option<String>>,
    /// A client changed the update channel: the next cycle checks whatever the
    /// gates say. A flag the loop takes, rather than an edit to the ledger, so
    /// it cannot race the cycle writing the same file.
    recheck: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Phase {
    #[default]
    UpToDate,
    Downloading,
    Ready,
    Waiting(Hold),
    Applying,
    Restarting,
    Failed,
    Managed,
}

impl Updater {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Live::default())),
            poke: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Lo que ve un cliente.
    pub fn state(&self) -> UpdateState {
        let live = self.lock();
        UpdateState {
            current: env!("CARGO_PKG_VERSION").to_string(),
            latest: live.latest.clone(),
            staged: live.staged.clone(),
            phase: match live.phase {
                Phase::UpToDate => UpdatePhase::UpToDate,
                Phase::Downloading => UpdatePhase::Downloading,
                Phase::Ready => UpdatePhase::Ready,
                Phase::Waiting(hold) => UpdatePhase::Waiting {
                    hold: match hold {
                        Hold::GameRunning => UpdateHold::GameRunning,
                        Hold::TransferInFlight => UpdateHold::TransferInFlight,
                    },
                },
                Phase::Applying => UpdatePhase::Applying,
                Phase::Restarting => UpdatePhase::Restarting,
                Phase::Failed => UpdatePhase::Failed,
                Phase::Managed => UpdatePhase::Managed,
            },
            deadline: live.deadline,
            mandatory: live.mandatory,
            unattended: live.unattended,
            last_error: live.last_error.clone(),
        }
    }

    /// A client asks to apply now. It returns immediately: the loop is what
    /// applies, and what happened is read afterwards through [`Updater::state`].
    pub fn apply_now(&self, version: Option<String>) {
        self.lock().requested = Some(version);
        self.poke.notify_one();
    }

    /// Check for an update now rather than on the hour: the update channel just
    /// changed ([`hoard_core::ipc::Request::RecheckUpdate`]). The next cycle
    /// asks GitHub past both gates (the hour and the failure backoff), and the
    /// loop is woken to run that cycle.
    pub fn recheck(&self) {
        self.lock().recheck = true;
        self.poke.notify_one();
    }

    fn take_recheck(&self) -> bool {
        std::mem::take(&mut self.lock().recheck)
    }

    /// "Not now", for `hours`. It does not move the deadline.
    pub fn snooze(&self, hours: u32) {
        let until = OffsetDateTime::now_utc() + time::Duration::hours(hours.min(24 * 7) as i64);
        let mut ledger = Ledger::load();
        ledger.snoozed_until = Some(until);
        if let Err(err) = ledger.save() {
            tracing::warn!(error = %format!("{err:#}"), "hoardd: couldn't record the update snooze");
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Live> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn take_request(&self) -> Option<Option<String>> {
        self.lock().requested.take()
    }

    fn set_phase(&self, phase: Phase) {
        self.lock().phase = phase;
    }

    fn fail(&self, error: String) {
        let mut live = self.lock();
        live.phase = Phase::Failed;
        live.last_error = Some(error);
    }
}

impl Default for Updater {
    fn default() -> Self {
        Self::new()
    }
}

// =======================================================================
// El bucle
// =======================================================================

/// Why the loop stops. There is only one reason: something was applied and the new
/// binary has to be started.
pub struct Relaunch {
    pub version: String,
}

/// Watches, downloads and applies. It runs under `supervisor::supervise` like
/// everything that outlives a request (D.12): a panic here is a logged incident and
/// a restart, not a service that quietly stops updating for ever.
pub async fn watch(
    updater: Updater,
    engine: Engine,
    notifier: Arc<crate::notify::Notifier>,
    relaunch: tokio::sync::mpsc::Sender<Relaunch>,
) -> Finished {
    tokio::time::sleep(WARMUP).await;
    let mut next_poll = Duration::ZERO;

    loop {
        if next_poll > Duration::ZERO {
            tokio::select! {
                _ = tokio::time::sleep(next_poll) => {}
                // A client asked to apply, so there is no waiting for the hour.
                _ = updater.poke.notified() => {}
            }
        }

        let requested = updater.take_request();
        next_poll = match tick(&updater, &engine, &notifier, requested, &relaunch).await {
            Cadence::Normal => POLL,
            Cadence::Soon => RETRY,
        };
    }
}

/// When to come back.
enum Cadence {
    Normal,
    /// Hay algo pendiente y frenado: se vuelve pronto a mirar el freno.
    Soon,
}

async fn tick(
    updater: &Updater,
    engine: &Engine,
    notifier: &crate::notify::Notifier,
    requested: Option<Option<String>>,
    relaunch: &tokio::sync::mpsc::Sender<Relaunch>,
) -> Cadence {
    let manifest = match Manifest::load_or_observe() {
        Ok(m) => m,
        Err(err) => {
            updater.fail(format!("{err:#}"));
            return Cadence::Normal;
        }
    };

    // Nada nuestro que tocar: lo mantiene el gestor de paquetes de la distro, un
    // Flatpak, un `nix`. Se dice y no se vuelve a mirar.
    if manifest.delivery.is_some_and(|d| !d.is_ours()) {
        updater.set_phase(Phase::Managed);
        return Cadence::Normal;
    }

    let unattended = manifest.applies_unattended();
    let mut ledger = Ledger::load();

    // What we staged is what we're running: the update landed, whoever got to
    // see it. This is the only place that can close the book on Windows, where
    // the installer kills the daemon that launched it: the process that applied
    // the update is never the process that returns from applying it, so
    // without this the deadline, the staged copy and the attempt counter all
    // survive an update that worked.
    let current = update::current();
    if ledger.staged.as_deref() == Some(current) {
        tracing::info!(
            version = current,
            "hoardd: started on the version it had staged"
        );
        ledger.applied(current);
        let _ = ledger.save();
        stage::sweep(current);
    }

    // Freno de mano tras varios fallos seguidos: se sigue mirando, pero al ritmo
    // largo, no al corto.
    let burnt = ledger.failures >= MAX_FAILURES;

    // The prefs are read every cycle, so switching the update channel needs no
    // restart; `check_if_due` treats a switch as a check that is due now.
    let now = OffsetDateTime::now_utc();
    let prefs = hoard_agent::prefs::Prefs::load_default()
        .map(|(p, _)| p)
        .unwrap_or_default();
    let channel = Channel::from_prerelease(prefs.prerelease_updates);
    let forced = updater.take_recheck();
    if check_if_due(&mut ledger, now, channel, forced, update::GITHUB_API).await {
        let _ = ledger.save();
    }
    // What the ledger saw on the other channel is no answer on this one.
    let latest = latest_on(&ledger, channel);

    let situation = Situation {
        current: update::current().to_string(),
        latest: latest.clone(),
        staged: ledger.staged.clone(),
        first_seen_at: ledger.first_seen_at,
        unattended,
        transfer_in_flight: engine.transfers_in_flight(),
        game_running: game_running(engine).await,
    };
    let stance = auto::decide(now, &situation);

    {
        let mut live = updater.lock();
        live.latest = latest;
        live.staged.clone_from(&ledger.staged);
        live.deadline = ledger.deadline();
        live.mandatory = matches!(stance, Stance::Force { .. });
        live.unattended = unattended;
    }

    match stance {
        Stance::Idle => {
            updater.set_phase(Phase::UpToDate);
            Cadence::Normal
        }

        Stance::Stage { version } => {
            if burnt {
                tracing::debug!(
                    version = %version,
                    failures = ledger.failures,
                    "hoardd: update staging is backed off after repeated failures"
                );
                return Cadence::Normal;
            }
            updater.set_phase(Phase::Downloading);
            tracing::info!(version = %version, "hoardd: downloading the update");
            match stage::stage(&version, &manifest).await {
                Ok(staged) => {
                    ledger.staged = Some(staged.version.clone());
                    ledger.staged_at = Some(OffsetDateTime::now_utc());
                    ledger.failures = 0;
                    ledger.last_error = None;
                    let _ = ledger.save();
                    updater.lock().staged = Some(staged.version);
                    updater.set_phase(Phase::Ready);
                    // It is on disk now: the next cycle decides whether to apply it,
                    // and there is no reason to wait an hour to ask.
                    Cadence::Soon
                }
                Err(err) => {
                    let message = format!("{err:#}");
                    tracing::warn!(version = %version, error = %message, "hoardd: couldn't stage the update");
                    ledger.failures += 1;
                    ledger.last_error = Some(message.clone());
                    let _ = ledger.save();
                    updater.fail(message);
                    Cadence::Normal
                }
            }
        }

        Stance::Waiting { version, hold } => {
            // A client asking to apply **now** must not fall on deaf ears: without
            // this the request was lost in silence and the button did nothing, which
            // is worse than a disabled button.
            //
            // The two brakes are treated differently because they are not the same.
            // "A game is open" is a courtesy of ours, and the user can waive it, since
            // they asked. "An upload is halfway through" is no courtesy: swapping the
            // binaries there kills the process doing it, so the request is stored and
            // served as soon as it finishes, which is seconds.
            if let Some(asked) = requested {
                match hold {
                    Hold::GameRunning => {
                        tracing::info!(version = %version, "hoardd: a client asked to update with a game running, honouring it");
                        return apply(updater, &mut ledger, &manifest, &version, false, relaunch)
                            .await;
                    }
                    Hold::TransferInFlight => {
                        updater.lock().requested = Some(asked);
                    }
                }
            }
            tracing::debug!(version = %version, ?hold, "hoardd: update is staged and waiting");
            updater.set_phase(Phase::Waiting(hold));
            Cadence::Soon
        }

        Stance::Ask { version } => {
            updater.set_phase(Phase::Ready);
            // A client asked to apply, so somebody is in front of it and the routes
            // that need a dialog can open one.
            if let Some(asked) = requested {
                if asked.as_deref().is_none_or(|v| v == version) {
                    return apply(updater, &mut ledger, &manifest, &version, false, relaunch).await;
                }
            }
            if ledger
                .snoozed_until
                .is_none_or(|until| OffsetDateTime::now_utc() >= until)
            {
                tracing::info!(version = %version, "hoardd: an update is ready and needs someone to approve it");
                // **Once per version, and only in this case.** It is the only road
                // that does not finish on its own: without this notice, somebody who
                // installed from a `.deb` and does not open the app for a week hears
                // nothing until the deadline passes, and the deadline can only take
                // over their screen if they get around to opening it.
                if ledger.notified.as_deref() != Some(version.as_str()) {
                    notifier
                        .announce(crate::notify::Kind::UpdateReady {
                            version: version.clone(),
                        })
                        .await;
                    ledger.notified = Some(version.clone());
                    let _ = ledger.save();
                }
            }
            Cadence::Normal
        }

        Stance::ApplyQuietly { version } | Stance::Force { version } => {
            if burnt && requested.is_none() {
                return Cadence::Normal;
            }
            // `noninteractive` even here: the background cycle has no window to
            // paint a dialog in, so a `pkexec` launched from here would wait for ever
            // on somebody who will never see it. Only when a client asks explicitly is
            // asking allowed.
            let interactive = requested.is_some();
            apply(
                updater,
                &mut ledger,
                &manifest,
                &version,
                !interactive,
                relaunch,
            )
            .await
        }
    }
}

/// Asks GitHub for the newest release on `channel`, when a check is due, and
/// notes the answer in `ledger`. Returns whether the ledger changed.
///
/// Due means: a client asked (`forced`, after changing the channel), or the
/// channel on record is not this one, or the last successful check is more
/// than [`POLL`] old. Only `forced` skips the [`CHECK_BACKOFF`] after a failed
/// attempt, so GitHub down never turns into a request a minute. A cycle that
/// comes back early because a game is open is looking at the brake, not at the
/// version, and asks nothing.
///
/// Opting out never downgrades: the stable answer replaces the pre-release in
/// the ledger, and [`auto::decide`] only moves to a version newer than the one
/// running. Until an answer arrives on the new channel, [`latest_on`] hides the
/// old one from `decide`, so a failed check cannot install it either.
async fn check_if_due(
    ledger: &mut Ledger,
    now: OffsetDateTime,
    channel: Channel,
    forced: bool,
    api: &str,
) -> bool {
    let elapsed = |at: Option<OffsetDateTime>, gap: Duration| {
        at.is_none_or(|at| now - at >= time::Duration::seconds(gap.as_secs() as i64))
    };
    let wanted = ledger.channel != channel || elapsed(ledger.last_check_at, POLL);
    let due = forced || (wanted && elapsed(ledger.last_attempt_at, CHECK_BACKOFF));
    if !due {
        return false;
    }
    ledger.last_attempt_at = Some(now);
    if let Some(latest) = update::fetch_latest_from(api, channel).await {
        ledger.observe(&latest, now);
        ledger.channel = channel;
    } else {
        tracing::debug!(?channel, "hoardd: the update check failed, backing off");
    }
    true
}

/// The newest version the ledger knows **on `channel`**, or `None` when its
/// answer came from the other one.
///
/// This is what keeps a switch safe whatever the network does. After opting
/// out, the ledger still holds the pre-release it saw (and may have staged)
/// until a stable answer replaces it; handed to [`auto::decide`], a staged
/// `1.2.0-2` above a running `1.2.0-1` would be applied, the opposite of what
/// the user just asked. Hidden, `decide` sees nothing to do, clients are shown
/// nothing to apply, and the first successful check on the new channel puts a
/// real answer back. Masking rather than clearing keeps the rule the same in
/// both directions and leaves the ledger to the one function that writes it.
fn latest_on(ledger: &Ledger, channel: Channel) -> Option<String> {
    if ledger.channel == channel {
        ledger.latest_seen.clone()
    } else {
        None
    }
}

/// Aplica lo bajado y pide el relevo.
async fn apply(
    updater: &Updater,
    ledger: &mut Ledger,
    manifest: &Manifest,
    version: &str,
    noninteractive: bool,
    relaunch: &tokio::sync::mpsc::Sender<Relaunch>,
) -> Cadence {
    let Some(staged) = stage::already_staged(version, manifest) else {
        // What was downloaded is gone (a cache clean, a full disk). It is forgotten
        // and the next cycle downloads it again.
        ledger.staged = None;
        ledger.staged_at = None;
        let _ = ledger.save();
        return Cadence::Soon;
    };

    updater.set_phase(Phase::Applying);
    tracing::info!(version, noninteractive, "hoardd: applying the update");

    // The attempt is written down *before* it happens, which is backwards
    // everywhere except here: on Windows there is no after. The NSIS installer
    // stops `hoardd.exe` before overwriting it, so the run that applies an
    // update is killed while it waits for that installer and never reaches
    // either arm below. Left uncounted, an install that keeps failing is
    // retried every hour forever, and every retry force-closes the app the user
    // is looking at. A cycle that sees what we staged is what we're now
    // running clears this again.
    ledger.failures += 1;
    ledger.last_error = Some(format!("applying {version} never reported back"));
    let _ = ledger.save();

    let mut manifest = manifest.clone();
    match stage::apply(&staged, &mut manifest, noninteractive).await {
        Ok(()) => {
            ledger.applied(version);
            let _ = ledger.save();
            {
                let mut live = updater.lock();
                live.phase = Phase::Restarting;
                live.staged = None;
                live.mandatory = false;
                live.last_error = None;
            }
            tracing::info!(
                version,
                "hoardd: update applied, relaunching on the new binary"
            );
            // The relief does not happen here: there is an engine to stop and a
            // socket to release, and `run` owns those. A full or closed channel means
            // a relief is under way already.
            let _ = relaunch.try_send(Relaunch {
                version: version.to_string(),
            });
            Cadence::Normal
        }
        Err(err) => {
            let message = format!("{err:#}");
            tracing::warn!(version, error = %message, "hoardd: couldn't apply the update");
            // Already counted above; this only puts the real reason in place of
            // the placeholder.
            ledger.last_error = Some(message.clone());
            let _ = ledger.save();
            updater.fail(message);
            // A privilege failure is not the updater failing: it means somebody has
            // to be in front of it. The first window to open resolves it, so it is
            // left marked pending rather than silent.
            Cadence::Normal
        }
    }
}

/// Is any game open right now? The engine is asked, since it is what correlates
/// process to folder; with no engine there are no games to speak of.
async fn game_running(engine: &Engine) -> bool {
    crate::engine::slot_status(engine)
        .await
        .iter()
        .any(|s| s.process_running)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_updater_says_nothing_is_pending() {
        let u = Updater::new();
        let s = u.state();
        assert_eq!(s.phase, UpdatePhase::UpToDate);
        assert!(!s.mandatory);
        assert_eq!(s.latest, None);
        assert_eq!(s.current, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn a_client_request_is_taken_exactly_once() {
        let u = Updater::new();
        u.apply_now(Some("1.2.3".into()));
        assert_eq!(u.take_request(), Some(Some("1.2.3".into())));
        // The second read comes back empty: two presses of the button must not turn
        // into two applications treading on each other.
        assert_eq!(u.take_request(), None);
    }

    #[test]
    fn every_hold_survives_the_trip_to_the_wire() {
        for (hold, expected) in [
            (Hold::GameRunning, UpdateHold::GameRunning),
            (Hold::TransferInFlight, UpdateHold::TransferInFlight),
        ] {
            let u = Updater::new();
            u.set_phase(Phase::Waiting(hold));
            assert_eq!(u.state().phase, UpdatePhase::Waiting { hold: expected });
        }
    }

    #[test]
    fn a_failure_is_visible_to_clients() {
        let u = Updater::new();
        u.fail("no package for aarch64".into());
        let s = u.state();
        assert_eq!(s.phase, UpdatePhase::Failed);
        assert_eq!(s.last_error.as_deref(), Some("no package for aarch64"));
    }

    // ---- the channel, through the cycle's own check and the real policy

    use hoard_agent::testing::{canned_github, release_json, Asked, LATEST_PATH, LIST_PATH};

    /// GitHub with `latest` naming `stable` and the list holding `listed`.
    async fn github(stable: &str, listed: &[(&str, bool)]) -> (String, Asked) {
        let list: Vec<String> = listed
            .iter()
            .map(|(tag, pre)| release_json(tag, *pre, false).to_string())
            .collect();
        canned_github(vec![
            (LATEST_PATH, release_json(stable, false, false).to_string()),
            (LIST_PATH, format!("[{}]", list.join(","))),
        ])
        .await
    }

    /// What the cycle decides after checking on `channel`, for a machine
    /// running `current`: the tick's own [`latest_on`] and the real policy.
    fn decide_on(
        current: &str,
        ledger: &Ledger,
        channel: Channel,
        now: OffsetDateTime,
    ) -> Stance {
        auto::decide(
            now,
            &Situation {
                current: current.to_string(),
                latest: latest_on(ledger, channel),
                staged: ledger.staged.clone(),
                first_seen_at: ledger.first_seen_at,
                unattended: true,
                transfer_in_flight: false,
                game_running: false,
            },
        )
    }

    #[tokio::test]
    async fn opted_in_stages_the_next_prerelease() {
        let (api, asked) = github("v1.1.7", &[("v1.2.0-2", true), ("v1.2.0-1", true)]).await;
        let now = OffsetDateTime::now_utc();
        let mut ledger = Ledger::default();

        assert!(check_if_due(&mut ledger, now, Channel::Prerelease, false, &api).await);
        assert_eq!(ledger.latest_seen.as_deref(), Some("1.2.0-2"));
        assert_eq!(ledger.channel, Channel::Prerelease);
        assert_eq!(
            decide_on("1.2.0-1", &ledger, Channel::Prerelease, now),
            Stance::Stage {
                version: "1.2.0-2".into()
            }
        );
        assert_eq!(*asked.lock().unwrap(), vec![LIST_PATH.to_string()]);
    }

    #[tokio::test]
    async fn stable_ignores_newer_prereleases() {
        let (api, asked) = github("v1.1.7", &[("v1.2.0-2", true), ("v1.2.0-1", true)]).await;
        let now = OffsetDateTime::now_utc();
        let mut ledger = Ledger::default();

        assert!(check_if_due(&mut ledger, now, Channel::Stable, false, &api).await);
        assert_eq!(ledger.latest_seen.as_deref(), Some("1.1.7"));
        assert_eq!(decide_on("1.1.7", &ledger, Channel::Stable, now), Stance::Idle);
        assert_eq!(*asked.lock().unwrap(), vec![LATEST_PATH.to_string()]);
    }

    /// Opting out on a pre-release: nothing is downgraded while GitHub's latest is
    /// older than what runs, and the full release is offered the day it ships.
    #[tokio::test]
    async fn opting_out_after_a_prerelease_waits_for_the_release() {
        let now = OffsetDateTime::now_utc();
        // The ledger of a tester who took 1.2.0-2 an hour ago on the pre-release
        // channel, and whose check is otherwise not due.
        let mut ledger = Ledger {
            latest_seen: Some("1.2.0-2".into()),
            last_check_at: Some(now),
            channel: Channel::Prerelease,
            ..Ledger::default()
        };

        let (api, asked) = github("v1.1.7", &[("v1.2.0-2", true)]).await;
        assert!(
            check_if_due(&mut ledger, now, Channel::Stable, false, &api).await,
            "switching channel makes the check due at once"
        );
        assert_eq!(*asked.lock().unwrap(), vec![LATEST_PATH.to_string()]);
        assert_eq!(ledger.channel, Channel::Stable);
        assert_eq!(ledger.latest_seen.as_deref(), Some("1.1.7"));
        assert_eq!(
            decide_on("1.2.0-2", &ledger, Channel::Stable, now),
            Stance::Idle,
            "no downgrade"
        );

        let later = now + time::Duration::hours(2);
        let (api, _) = github("v1.2.0", &[("v1.2.0", false), ("v1.2.0-2", true)]).await;
        assert!(check_if_due(&mut ledger, later, Channel::Stable, false, &api).await);
        assert_eq!(
            decide_on("1.2.0-2", &ledger, Channel::Stable, later),
            Stance::Stage {
                version: "1.2.0".into()
            }
        );
    }

    /// The hour's gate still holds on an unchanged channel: a cycle that comes
    /// back early for the brake asks GitHub nothing.
    #[tokio::test]
    async fn an_unchanged_channel_waits_out_the_hour() {
        let (api, asked) = github("v1.1.7", &[]).await;
        let now = OffsetDateTime::now_utc();
        let mut ledger = Ledger {
            latest_seen: Some("1.1.7".into()),
            last_check_at: Some(now),
            ..Ledger::default()
        };
        assert!(!check_if_due(&mut ledger, now, Channel::Stable, false, &api).await);
        assert!(asked.lock().unwrap().is_empty());
    }

    /// Opting out while GitHub fails: the staged pre-release is not applied,
    /// clients are shown nothing to apply, and the failed check is not retried
    /// every cycle.
    #[tokio::test]
    async fn opting_out_while_github_fails_applies_nothing_and_backs_off() {
        let now = OffsetDateTime::now_utc();
        // A tester on 1.2.0-1 with 1.2.0-2 seen, staged, and past its deadline:
        // on the pre-release channel this is a forced install.
        let mut ledger = Ledger {
            latest_seen: Some("1.2.0-2".into()),
            first_seen_at: Some(now - time::Duration::days(30)),
            staged: Some("1.2.0-2".into()),
            last_check_at: Some(now),
            last_attempt_at: Some(now),
            channel: Channel::Prerelease,
            ..Ledger::default()
        };
        assert_eq!(
            decide_on("1.2.0-1", &ledger, Channel::Prerelease, now),
            Stance::Force {
                version: "1.2.0-2".into()
            },
            "the fixture is one where applying would happen"
        );

        // Opted out; GitHub has no `latest` to give (404).
        let (api, asked) = canned_github(vec![]).await;
        assert!(check_if_due(&mut ledger, now, Channel::Stable, true, &api).await);
        assert_eq!(asked.lock().unwrap().len(), 1, "the switch asked once");
        assert_eq!(ledger.channel, Channel::Prerelease, "no answer, no switch on record");
        assert_eq!(
            decide_on("1.2.0-1", &ledger, Channel::Stable, now),
            Stance::Idle,
            "the stale pre-release must not be applied"
        );
        assert_eq!(latest_on(&ledger, Channel::Stable), None, "nor shown to clients");

        // The cycles that follow (every RETRY while something is pending) do not
        // ask again until the backoff has passed.
        for minutes in [1, 2, 5, 14] {
            let t = now + time::Duration::minutes(minutes);
            assert!(!check_if_due(&mut ledger, t, Channel::Stable, false, &api).await);
        }
        assert_eq!(asked.lock().unwrap().len(), 1, "no retry storm");
        let t = now + time::Duration::minutes(15);
        assert!(check_if_due(&mut ledger, t, Channel::Stable, false, &api).await);
        assert_eq!(asked.lock().unwrap().len(), 2, "tried again after the backoff");
        assert_eq!(decide_on("1.2.0-1", &ledger, Channel::Stable, t), Stance::Idle);
    }

    /// A stable check that keeps failing is spaced out too, not only a switch.
    #[tokio::test]
    async fn a_failing_check_on_the_same_channel_backs_off() {
        let (api, asked) = canned_github(vec![]).await;
        let now = OffsetDateTime::now_utc();
        let mut ledger = Ledger::default();
        assert!(check_if_due(&mut ledger, now, Channel::Stable, false, &api).await);
        let soon = now + time::Duration::minutes(1);
        assert!(!check_if_due(&mut ledger, soon, Channel::Stable, false, &api).await);
        assert_eq!(asked.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_recheck_is_taken_exactly_once() {
        let u = Updater::new();
        u.recheck();
        assert!(u.take_recheck());
        assert!(!u.take_recheck());
    }
}
