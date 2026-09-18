//! Who plays a shared world this session, decided without asking twice
//! (HRD-D-0002).
//!
//! A game with shared worlds starts and the engine asks once, through
//! `WorldClaimWanted`: which world, hosting or viewing. Then it waits for one
//! of four things. The answer (`ClaimWorld`, `DismissWorld`). A minute with no
//! answer while the game has exactly one shared world whose lease is free,
//! after which it hosts on its own. A write to a world nobody claimed, which
//! takes the lease if it is free (evidence beats a prompt). Or the game
//! closing. A viewer's writes, and a host's under somebody else's lease, never
//! push: on close they go to a side copy and the folder goes back to the
//! shared head. A host's lease is given back once its final flush is up.
//!
//! The engine calls in at four places in `agent.rs` (GameStarted, GameStopped,
//! the fs event, the reconcile pass), at the two answers, at the server's word
//! on the lease and when a side copy lands; every decision is here, over the
//! slot, so it runs in a test with no process and no socket.
//! Nothing here restores mid-session: the kernel forbids it, and a shared
//! save is kept current while the game is closed by the level-triggered pull,
//! so launch needs no pull of its own.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use hoard_core::ipc::{AgentEvent, WorldChoice, WorldLease, WorldPrompt, WorldRole};
use hoard_core::kernel::fileclass::Scope;
use hoard_core::kernel::session::RECENT_SAVE_GRACE_SECS;
use hoard_core::kernel::LeaseObs;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::agent::{SaveSlot, WatchedSave};
use crate::lease::LeaseHandle;

/// How long an unanswered prompt waits before the one free world is hosted.
pub const AUTO_HOST_DELAY_SECS: u64 = 60;

/// One session of a game with shared worlds, on each of that game's shared
/// slots. Made on GameStarted, dropped when there is nothing left to do after
/// GameStopped: the release, or the side copy.
#[derive(Debug, Clone)]
pub(crate) struct WorldSession {
    pub started_at: Instant,
    /// `started_at` on the wall clock, for the status: an `Instant` means
    /// nothing outside this process.
    pub raised_at: OffsetDateTime,
    /// The prompt for this game went out with this slot in it.
    pub prompted: bool,
    /// "Not playing": no auto-host, no second prompt. Evidence still claims.
    pub dismissed: bool,
    /// A role was taken this session: by the answer, by the clock, or by a
    /// write. Acquire once.
    pub claimed: bool,
    /// Writes seen while viewing; the second one is told.
    pub writes: u32,
    pub view_writing_told: bool,
    /// Wrote under somebody else's lease: the session's changes stay local.
    pub side_copy: bool,
    /// Wrote while the lease was still unknown; claims when it reads free.
    pub wrote_unclaimed: bool,
    /// The side copy is on its way; the session ends when it lands.
    pub side_copy_started: bool,
    /// The lease task was asked who holds it, once, when the prompt found
    /// the lease unknown.
    pub refresh_asked: bool,
    pub auto_host_deadline: Option<Instant>,
    /// GameStopped came; what is left is the release or the side copy.
    pub stopped: bool,
}

impl WorldSession {
    fn new(started_at: Instant) -> Self {
        Self {
            started_at,
            raised_at: OffsetDateTime::now_utc(),
            prompted: false,
            dismissed: false,
            claimed: false,
            writes: 0,
            view_writing_told: false,
            side_copy: false,
            wrote_unclaimed: false,
            side_copy_started: false,
            refresh_asked: false,
            auto_host_deadline: None,
            stopped: false,
        }
    }

    /// Between GameStarted and GameStopped.
    pub fn live(&self) -> bool {
        !self.stopped
    }
}

pub(crate) fn lease_for_prompt(obs: LeaseObs) -> WorldLease {
    match obs {
        LeaseObs::Unknown => WorldLease::Unknown,
        LeaseObs::Free => WorldLease::Free,
        LeaseObs::Mine => WorldLease::Mine,
        LeaseObs::Other => WorldLease::Other,
    }
}

/// The shared slots of `game_slug` on this machine, in a stable order.
fn shared_of(slots: &HashMap<String, SaveSlot>, game_slug: &str) -> Vec<String> {
    let mut ids: Vec<String> = slots
        .values()
        .filter(|s| s.save.shared.is_some() && !s.save.track_only && s.save.game_slug == game_slug)
        .map(|s| s.save.save_id.clone())
        .collect();
    ids.sort();
    ids
}

/// GameStarted on `save_id`: open a session on every shared world of that
/// game, ask once, and start the clock when there is exactly one world and
/// nobody holds it. A second GameStarted of the same game (two of its saves
/// matched the process) finds the sessions open and asks nothing.
///
/// No pull happens here on purpose. The reducer keeps a shared save at the
/// server's head while the game is closed, level-triggered, and the kernel
/// never restores mid-session; by the time the process is seen the folder is
/// as current as it is going to be.
pub(crate) fn on_game_started(
    slots: &mut HashMap<String, SaveSlot>,
    save_id: &str,
    now: Instant,
    events_tx: &mpsc::Sender<AgentEvent>,
) {
    let Some(game_slug) = slots.get(save_id).map(|s| s.save.game_slug.clone()) else {
        return;
    };
    let ids = shared_of(slots, &game_slug);
    if ids.is_empty() {
        return;
    }
    let already = ids.iter().any(|id| {
        slots
            .get(id)
            .and_then(|s| s.session.as_ref())
            .is_some_and(|w| w.live())
    });
    if already {
        return;
    }
    let mut open = Vec::with_capacity(ids.len());
    for id in &ids {
        let Some(slot) = slots.get_mut(id) else {
            continue;
        };
        // A side copy still on its way belongs to the last session: it keeps
        // its session until the copy lands, and this prompt goes out without
        // it. Otherwise the answer would land on a session that is not the one
        // that wrote, and the move could take files from under the game. The
        // landing opens this world's session (`on_side_copied`).
        if slot
            .session
            .as_ref()
            .is_some_and(|w| !w.live() && w.side_copy_started)
        {
            tracing::info!(save_id = %id, "agent: side copy still landing; not asking about this world yet");
            slot.relaunch_pending = true;
            continue;
        }
        open.push(id.clone());
    }
    open_sessions(slots, &game_slug, &open, ids.len(), false, now, events_tx);
}

/// Opens a session on each of `open` (shared worlds of `game_slug`, `total`
/// of them on this machine) and asks about the ones with no role yet. With
/// `dismissed`, those open as "not playing" instead, and nothing is asked.
fn open_sessions(
    slots: &mut HashMap<String, SaveSlot>,
    game_slug: &str,
    open: &[String],
    total: usize,
    dismissed: bool,
    now: Instant,
    events_tx: &mpsc::Sender<AgentEvent>,
) {
    let mut worlds = Vec::with_capacity(open.len());
    for id in open {
        let Some(slot) = slots.get_mut(id) else {
            continue;
        };
        let mut session = WorldSession::new(now);
        // A role is for one session: last time's View does not decide this
        // one. Two exceptions. A role chosen before the process was seen
        // (`ClaimWorld` outside a session) is this session's answer. Writes
        // that could not be set aside keep the slot a viewer until they are.
        let pinned = std::mem::take(&mut slot.role_pinned);
        if !pinned && !slot.local_only_pending {
            slot.role = WorldRole::Host;
        }
        // A lease this machine still holds (an earlier session that never
        // released, a restart) is a role already taken, not one to ask about.
        session.claimed = pinned || slot.lease == LeaseObs::Mine;
        session.prompted = !session.claimed;
        if dismissed && !session.claimed {
            session.dismissed = true;
            session.prompted = false;
        }
        let ask = session.prompted;
        slot.session = Some(session);
        if !ask {
            tracing::info!(save_id = %id, role = ?slot.role, "agent: shared world already has a role; not asking");
            continue;
        }
        worlds.push(WorldChoice {
            save_id: id.clone(),
            label: slot.save.label.clone(),
            group_name: slot
                .save
                .shared
                .as_ref()
                .map(|r| r.group_name.clone())
                .unwrap_or_default(),
            holder: slot.lease_holder.clone(),
            lease: lease_for_prompt(slot.lease),
        });
    }
    if worlds.len() == 1 && total == 1 {
        if let Some(slot) = slots.get_mut(&worlds[0].save_id) {
            // `Unknown` arms too: the reconcile pass asks the server first
            // thing, and the clock only fires on a lease that reads free.
            let free = matches!(slot.lease, LeaseObs::Free | LeaseObs::Unknown);
            if let Some(session) = slot.session.as_mut() {
                if free && !session.claimed {
                    session.auto_host_deadline =
                        Some(now + std::time::Duration::from_secs(AUTO_HOST_DELAY_SECS));
                }
            }
        }
    }
    if worlds.is_empty() {
        return;
    }
    tracing::info!(game_slug = %game_slug, worlds = worlds.len(), "agent: asking which shared world");
    let _ = events_tx.try_send(AgentEvent::WorldClaimWanted {
        game_slug: game_slug.to_string(),
        worlds,
    });
}

/// GameStopped on `save_id`: its session stops, and so does every sibling's
/// whose own process is not running. Detection is per folder, so a game
/// found through one world's activity ends through that world alone; the
/// siblings' sessions were opened on its start and would outlive it.
pub(crate) fn on_game_stopped_all(slots: &mut HashMap<String, SaveSlot>, save_id: &str) {
    let Some(game_slug) = slots.get(save_id).map(|s| s.save.game_slug.clone()) else {
        return;
    };
    for id in shared_of(slots, &game_slug) {
        let Some(slot) = slots.get_mut(&id) else {
            continue;
        };
        if id != save_id && slot.is_running {
            continue;
        }
        on_game_stopped(slot);
    }
}

/// GameStopped on one slot: the clock stops; what is left (release, side
/// copy) is the reconcile pass's, which has the reducer's state to judge it by.
pub(crate) fn on_game_stopped(slot: &mut SaveSlot) {
    slot.relaunch_pending = false;
    if let Some(session) = slot.session.as_mut() {
        session.stopped = true;
        session.auto_host_deadline = None;
    }
}

/// The user answered `ClaimWorld`. `Host` asks for the lease; `View` on a
/// lease this machine still holds gives it back, or the reducer, which reads
/// the lease and not the role, would go on pushing the viewer's writes, and
/// `View` with an acquire still out cancels it. An answer with no session at
/// all is kept for the next one; one given to a session that stopped and
/// still owes its flush or side copy ends with that session.
pub(crate) fn on_claim_world(
    slot: &mut SaveSlot,
    role: WorldRole,
    events_tx: &mpsc::Sender<AgentEvent>,
    lease: Option<&LeaseHandle>,
) {
    let id = slot.save.save_id.clone();
    slot.role = role;
    slot.role_pinned = slot.session.is_none();
    on_claim(slot);
    match role {
        WorldRole::Host => {
            if let Some(lease) = lease {
                request_acquire(slot, lease);
            }
        }
        WorldRole::View => {
            let holds = slot.lease == LeaseObs::Mine;
            if holds {
                tracing::info!(save_id = %id, "agent: viewing a world this machine hosts; giving the lease back");
            }
            give_back(slot, lease, holds);
        }
    }
    let _ = events_tx.try_send(AgentEvent::WorldClaimed {
        save_id: id,
        game_slug: slot.save.game_slug.clone(),
        role,
        auto: false,
    });
}

/// A role was taken this session, by the user or by `ForceWorld`.
pub(crate) fn on_claim(slot: &mut SaveSlot) {
    if let Some(session) = slot.session.as_mut() {
        session.claimed = true;
        session.auto_host_deadline = None;
    }
}

/// `ForceWorld`: the lease is taken off its holder and asked for here, as a
/// host. Taken outside a session, like `ClaimWorld`, the role is the next
/// one's, and not a push's lease to give back once idle.
pub(crate) fn on_force_world(slot: &mut SaveSlot, lease: Option<&LeaseHandle>) {
    slot.role = WorldRole::Host;
    slot.role_pinned = slot.session.is_none();
    on_claim(slot);
    if let Some(lease) = lease {
        // The task runs them in order: the takeover, then the acquire with
        // this machine's head.
        lease.force(slot.save.save_id.clone());
        request_acquire(slot, lease);
    }
}

/// Asks for the lease. The verdict clears `lease_requested`; a release asked
/// for before is overtaken, so the verdict's `Mine` is kept. Not behind the
/// head: the answer is known, and the pull asks again (`catch_up`).
pub(crate) fn request_acquire(slot: &mut SaveSlot, lease: &LeaseHandle) {
    if let Some(base) = slot.stale_base {
        tracing::debug!(save_id = %slot.save.save_id, base, "agent: behind the head; the acquire waits for the pull");
        return;
    }
    slot.lease_requested = true;
    slot.release_requested = false;
    lease.acquire(slot.save.save_id.clone(), slot.known_version.unwrap_or(0));
}

/// Stops wanting the lease: an acquire still out is cancelled, and with
/// `release` the lease is given back. Either way `release_requested` marks
/// what comes back as asked for, not lost, and a `Mine` verdict the cancel
/// was too late for is given back when it lands (`on_lease`).
fn give_back(slot: &mut SaveSlot, lease: Option<&LeaseHandle>, release: bool) {
    let Some(lease) = lease else {
        return;
    };
    let id = slot.save.save_id.clone();
    if slot.lease_requested {
        // No verdict is owed any more: a retry the task still queued is
        // dropped, and one already on the wire is released on arrival.
        lease.cancel(id.clone());
        slot.lease_requested = false;
        slot.release_requested = true;
    }
    if release {
        lease.release(id);
        slot.release_requested = true;
    }
}

/// The user gave the world back (`ReleaseWorld`): the lease goes, an acquire
/// still out is cancelled, and the answer is not a lost lease. A role pinned
/// for the next launch (`ClaimWorld` or `ForceWorld` outside a session) goes
/// with it: giving the world back is not hosting it next time, and a pin left
/// standing kept every later sessionless push's lease (HRD-F-0028).
pub(crate) fn on_release_world(
    slot: &mut SaveSlot,
    events_tx: &mpsc::Sender<AgentEvent>,
    lease: Option<&LeaseHandle>,
) {
    slot.role_pinned = false;
    give_back(slot, lease, true);
    let _ = events_tx.try_send(AgentEvent::WorldReleased {
        save_id: slot.save.save_id.clone(),
        game_slug: slot.save.game_slug.clone(),
    });
}

/// The server's word on the lease, from the lease task or the live stream.
/// A `verdict` answers an acquire and clears `lease_requested`; an
/// observation (a refresh, a frame) leaves the flag to the answer still
/// owed, or a stop in between would end the session and the acquire's
/// `Mine` would land on nobody.
///
/// A `Mine` verdict on a viewer's slot, or on one that asked to give the
/// lease back meanwhile, is released at once and never read as ours: the
/// reducer, which reads the lease and not the role, would push under it.
pub(crate) fn on_lease(
    slot: &mut SaveSlot,
    obs: LeaseObs,
    holder: Option<String>,
    verdict: bool,
    events_tx: &mpsc::Sender<AgentEvent>,
    lease: Option<&LeaseHandle>,
) {
    let was = slot.lease;
    let asked = slot.release_requested;
    let unwanted = obs == LeaseObs::Mine && verdict && (slot.role == WorldRole::View || asked);
    if unwanted {
        tracing::info!(save_id = %slot.save.save_id, "agent: an acquire nobody wants any more landed; giving the lease back");
        if let Some(lease) = lease {
            slot.release_requested = true;
            lease.release(slot.save.save_id.clone());
        }
        // Held only for as long as the release takes: nothing pushes under it.
        slot.lease = LeaseObs::Unknown;
        slot.lease_holder = None;
    } else {
        slot.lease = obs;
        slot.lease_holder = holder;
        if obs == LeaseObs::Mine {
            slot.stale_base = None;
        }
    }
    if obs != LeaseObs::Mine {
        slot.release_requested = false;
    }
    if verdict {
        slot.lease_requested = false;
    }
    if obs != LeaseObs::Other {
        slot.hosted_elsewhere_notified = false;
    }
    // The lease this machine held is gone and nobody here gave it back:
    // forced, or expired while the renew could not get through.
    if was == LeaseObs::Mine && obs != LeaseObs::Mine && !asked {
        tracing::warn!(save_id = %slot.save.save_id, ?obs, "agent: hosting lease lost");
        let _ = events_tx.try_send(AgentEvent::WorldLeaseLost {
            save_id: slot.save.save_id.clone(),
            game_slug: slot.save.game_slug.clone(),
            holder: slot.lease_holder.clone(),
        });
    }
}

/// The acquire was refused as stale at `base`: the server's head is ahead of
/// the folder, and asking again with the same head gets the same answer. With
/// no session running the head comes down now: a pull when nothing is
/// pending, and pending writes are set aside first (`on_reconciled`), since
/// they could never go up without the lease. Mid-session the kernel defers
/// the pull until the game closes, like any other.
pub(crate) fn on_stale(slot: &mut SaveSlot, base: i64) {
    tracing::info!(save_id = %slot.save.save_id, base, "agent: the world is behind; pulling the head before hosting");
    slot.stale_base = Some(base);
    // Only writes to the world hold the pull back: the owner's other files
    // survive the merge and go up without the lease (HRD-D-0019).
    if !crate::agent::world_pending(slot) {
        slot.pull_pending = true;
    }
}

/// The pull after a stale refusal landed: `known_version` moved past the base
/// the server refused. A host who claimed (this session, or pinned for the
/// next) asks again with the new head; anyone else only stops being behind.
/// A pending push asks on its own through the reducer's hold.
fn catch_up(slot: &mut SaveSlot, lease: Option<&LeaseHandle>) {
    let Some(base) = slot.stale_base else {
        return;
    };
    if !slot.known_version.is_some_and(|v| v > base) {
        return;
    }
    slot.stale_base = None;
    let claimed = slot.role_pinned || slot.session.as_ref().is_some_and(|w| w.claimed);
    if slot.role != WorldRole::Host
        || !claimed
        || slot.lease_requested
        || slot.lease == LeaseObs::Mine
    {
        return;
    }
    tracing::info!(save_id = %slot.save.save_id, "agent: caught up with the head; asking for the lease again");
    if let Some(lease) = lease {
        request_acquire(slot, lease);
    }
}

/// Behind the head with writes in the folder and no session to own them: they
/// cannot go up (no lease without the head) and the pull must not walk over
/// them, so they go to a side copy, as a viewer's do. The same for the owner's
/// writes to the world while somebody else hosts it: the lease they need is
/// taken, and the owner's other files wait on them (HRD-D-0019, resolution 1).
/// The copy runs under a stopped session made for it, which is what
/// `on_side_copied` ends and what keeps its renames from reading as writes.
/// Not with the game running, not under an upload, and not again once a copy
/// failed (`local_only_pending`). Not while the folder was written within the
/// kernel's recent-save grace either: a game `is_running` does not see may
/// still have it open, and a later pass sets the writes aside once the folder
/// is quiet.
///
/// A world whose push is held for a file that cannot be read counts as the
/// owner's does: once somebody else hosts, its writes can never go up, and
/// they would hold the pull back for as long as the file stays unreadable.
fn set_aside_behind(slot: &mut SaveSlot, now: Instant) -> bool {
    let hosted_elsewhere = (slot.save.owns_whole_folder() || slot.world_held.active())
        && slot.lease == LeaseObs::Other;
    if (slot.stale_base.is_none() && !hosted_elsewhere)
        || slot.session.is_some()
        || !slot.has_pending
        || slot.is_running
        || slot.in_flight.is_some()
        || slot.local_only_pending
        || slot.lease == LeaseObs::Mine
        // Only the world is set aside, the owner's included (HRD-D-0019).
        || !crate::agent::world_pending(slot)
    {
        return false;
    }
    let wall = OffsetDateTime::now_utc();
    if slot
        .last_fs_event_at
        .is_some_and(|t| (wall - t).whole_seconds() < RECENT_SAVE_GRACE_SECS)
    {
        tracing::debug!(save_id = %slot.save.save_id, "agent: behind the head, but the folder was written recently; not setting it aside yet");
        return false;
    }
    tracing::info!(save_id = %slot.save.save_id, "agent: behind the head with local writes; setting them aside before the pull");
    let mut session = WorldSession::new(now);
    session.claimed = true;
    session.stopped = true;
    session.side_copy_started = true;
    slot.session = Some(session);
    true
}

/// A shared world seated with no version on this machine is one just adopted
/// (or never pulled since): the folder was made or picked for it a moment ago,
/// and that touch is not a session. Stamped as ours so the recency veto lets
/// the first pull through; `has_pending` and a running game still veto it.
pub(crate) fn on_seated(slot: &mut SaveSlot) {
    if slot.save.shared.is_some() && slot.known_version.is_none() {
        slot.last_restore_at = Some(OffsetDateTime::now_utc());
    }
}

/// The service is stopping and `closing()` gives back every lease held here:
/// the `Free` that follows, from the task or a live frame, was asked for.
pub(crate) fn on_stopping(slot: &mut SaveSlot) {
    if slot.lease == LeaseObs::Mine || slot.lease_requested {
        slot.release_requested = true;
    }
}

/// Whether the reducer's hold for the lease may turn into an acquire. Only a
/// host asks, once per hold, with something to push; not while a session is
/// still being asked (the claim flow acquires then, and says so); never for
/// writes that were meant to stay local; and not behind the head, where the
/// server's answer is known until the pull lands (`catch_up`).
pub(crate) fn may_request_lease(slot: &SaveSlot) -> bool {
    let being_asked = slot
        .session
        .as_ref()
        .is_some_and(|w| w.live() && !w.claimed);
    slot.save.shared.is_some()
        && slot.has_pending
        && slot.role == WorldRole::Host
        && !slot.lease_requested
        && !slot.local_only_pending
        && slot.stale_base.is_none()
        && !being_asked
}

/// How long after a side copy lands its renames may still reach the watcher:
/// the debouncer's window, with room.
const SIDE_COPY_TAIL: std::time::Duration = std::time::Duration::from_secs(10);

/// A watcher hit while the side copy moves the world's files, or in the
/// debouncer's tail once it landed, is the copy's own touch: not pending, not
/// evidence. Marked pending it would veto the pull that refills the folder.
pub(crate) fn hit_is_side_copy(slot: &SaveSlot, now: Instant) -> bool {
    if slot
        .session
        .as_ref()
        .is_some_and(|w| !w.live() && w.side_copy_started)
    {
        return true;
    }
    // A relaunch opened a session at the landing: its writes are the game's.
    slot.session.is_none()
        && slot
            .side_copy_landed_at
            .is_some_and(|at| now.saturating_duration_since(at) < SIDE_COPY_TAIL)
}

/// The user answered "not playing".
pub(crate) fn on_dismiss(slot: &mut SaveSlot) {
    if let Some(session) = slot.session.as_mut() {
        session.dismissed = true;
        session.auto_host_deadline = None;
    }
}

/// A role taken on `save_id` answers the game's prompt for its other worlds
/// too: choosing one is "not playing" the rest, and the prompt leaves the
/// status whole instead of lingering with the leftovers. Evidence still
/// claims a dismissed world, as always.
pub(crate) fn dismiss_siblings(slots: &mut HashMap<String, SaveSlot>, save_id: &str) {
    let Some(game_slug) = slots.get(save_id).map(|s| s.save.game_slug.clone()) else {
        return;
    };
    for slot in slots.values_mut() {
        if slot.save.save_id == save_id || slot.save.game_slug != game_slug {
            continue;
        }
        if let Some(session) = slot.session.as_mut() {
            if session.live() && session.prompted && !session.claimed && !session.dismissed {
                session.dismissed = true;
                session.auto_host_deadline = None;
            }
        }
    }
}

/// The prompts still waiting for an answer, one per game, for the status.
/// A world is in it while its session is live, was prompted, and nobody
/// answered for it: the answer, the clock, a write or GameStopped all take it
/// out. `auto_host_at` is the clock on the wall, from the one slot that has
/// it armed.
pub(crate) fn pending_prompts(slots: &HashMap<String, SaveSlot>, now: Instant) -> Vec<WorldPrompt> {
    let wall = OffsetDateTime::now_utc();
    let mut by_game: HashMap<String, WorldPrompt> = HashMap::new();
    let mut ids: Vec<&String> = slots.keys().collect();
    ids.sort();
    for id in ids {
        let slot = &slots[id];
        let Some(session) = slot.session.as_ref() else {
            continue;
        };
        if !session.live() || !session.prompted || session.claimed || session.dismissed {
            continue;
        }
        let prompt = by_game
            .entry(slot.save.game_slug.clone())
            .or_insert_with(|| WorldPrompt {
                game_slug: slot.save.game_slug.clone(),
                worlds: Vec::new(),
                auto_host_at: None,
                raised_at: session.raised_at,
            });
        prompt.raised_at = prompt.raised_at.min(session.raised_at);
        if let Some(deadline) = session.auto_host_deadline {
            prompt.auto_host_at = Some(wall + deadline.saturating_duration_since(now));
        }
        prompt.worlds.push(WorldChoice {
            save_id: slot.save.save_id.clone(),
            label: slot.save.label.clone(),
            group_name: slot
                .save
                .shared
                .as_ref()
                .map(|r| r.group_name.clone())
                .unwrap_or_default(),
            holder: slot.lease_holder.clone(),
            lease: lease_for_prompt(slot.lease),
        });
    }
    let mut out: Vec<WorldPrompt> = by_game.into_values().collect();
    out.sort_by(|a, b| a.game_slug.cmp(&b.game_slug));
    out
}

/// A write landed on the folder during a session: evidence. A host with no
/// claim takes a free lease now rather than waiting out the clock; under
/// somebody else's lease the session goes local (the hold already tells the
/// user); a viewer's second write is told once, since a game saving into a
/// copy is what the user least expects.
pub(crate) fn on_write(
    slot: &mut SaveSlot,
    events_tx: &mpsc::Sender<AgentEvent>,
    lease: Option<&LeaseHandle>,
) {
    if slot.save.shared.is_none() {
        return;
    }
    let Some(session) = slot.session.as_mut() else {
        return;
    };
    if !session.live() {
        return;
    }
    match slot.role {
        WorldRole::View => {
            session.writes = session.writes.saturating_add(1);
            if session.writes >= 2 && !session.view_writing_told {
                session.view_writing_told = true;
                let _ = events_tx.try_send(AgentEvent::ViewSessionWriting {
                    save_id: slot.save.save_id.clone(),
                    game_slug: slot.save.game_slug.clone(),
                });
            }
        }
        WorldRole::Host => match slot.lease {
            LeaseObs::Other => session.side_copy = true,
            LeaseObs::Free if !session.claimed => {
                session.claimed = true;
                session.auto_host_deadline = None;
                acquire_auto(slot, events_tx, lease, "a write with nobody hosting");
            }
            // Nobody knows who holds it yet: the write is kept as evidence and
            // claims once the refresh reads free (`on_reconciled`).
            LeaseObs::Unknown if !session.claimed => session.wrote_unclaimed = true,
            _ => {}
        },
    }
}

/// [`on_write`] for a watcher hit on `paths`, the files it saw change
/// ([`crate::agent::FsHit`]): only a write to the shared world is evidence of
/// playing it (HRD-D-0019). A character or another world written during the
/// session claims nothing. A hit with no path, or one that names no file under
/// the folder, is taken as a write, as before, and so is anything but a file
/// the world can reach into (a directory, or a path already gone).
pub(crate) fn on_write_at(
    slot: &mut SaveSlot,
    paths: &[PathBuf],
    events_tx: &mpsc::Sender<AgentEvent>,
    lease: Option<&LeaseHandle>,
) {
    let world = slot.save.world();
    let touches_world = |path: &Path| {
        let rel = path
            .strip_prefix(&slot.save.local_path)
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        rel.is_empty()
            || hoard_core::kernel::fileclass::included(world, &rel)
            || (!path.is_file() && hoard_core::kernel::fileclass::reaches_beneath(world, &rel))
    };
    if paths.is_empty() || paths.iter().any(|p| touches_world(p)) {
        on_write(slot, events_tx, lease);
    }
}

/// Hosts on the engine's own decision: the lease is asked for and the user
/// is told it was the engine, not them.
fn acquire_auto(
    slot: &mut SaveSlot,
    events_tx: &mpsc::Sender<AgentEvent>,
    lease: Option<&LeaseHandle>,
    why: &str,
) {
    let id = slot.save.save_id.clone();
    tracing::info!(save_id = %id, why, "agent: hosting the shared world on our own");
    if let Some(lease) = lease {
        request_acquire(slot, lease);
    }
    let _ = events_tx.try_send(AgentEvent::WorldClaimed {
        save_id: id,
        game_slug: slot.save.game_slug.clone(),
        role: WorldRole::Host,
        auto: true,
    });
}

/// What the reconcile pass has to launch for this slot, beyond what it did
/// on the slot itself.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Followup {
    Nothing,
    /// Move the session's writes aside and bring the head back.
    SideCopy,
}

/// Once per reconcile pass, after the reducer ran: the clock, the release,
/// the side copy. Everything that needs the reducer's word (`has_pending`,
/// `in_flight`) is judged here and nowhere else.
pub(crate) fn on_reconciled(
    slot: &mut SaveSlot,
    now: Instant,
    events_tx: &mpsc::Sender<AgentEvent>,
    lease: Option<&LeaseHandle>,
) -> Followup {
    catch_up(slot, lease);
    if maybe_release_held_world(slot, events_tx, lease) {
        return Followup::Nothing;
    }
    let Some(session) = slot.session.as_mut() else {
        if set_aside_behind(slot, now) {
            return Followup::SideCopy;
        }
        maybe_release_after_push(slot, lease);
        return Followup::Nothing;
    };
    if session.live() {
        // The prompt found the lease unknown: ask once, so the clock has an
        // answer to read before it fires.
        if slot.lease == LeaseObs::Unknown && !session.refresh_asked {
            session.refresh_asked = true;
            if let Some(lease) = lease {
                lease.refresh(slot.save.save_id.clone());
            }
        }
        // A write seen while the lease was unknown claims as soon as it reads
        // free; if somebody else holds it, the write was theirs to refuse.
        // "Not playing" does not stop it: the write says otherwise, as it
        // does in `on_write`.
        if session.wrote_unclaimed && !session.claimed {
            match slot.lease {
                LeaseObs::Free if slot.role == WorldRole::Host => {
                    session.wrote_unclaimed = false;
                    session.claimed = true;
                    session.auto_host_deadline = None;
                    acquire_auto(slot, events_tx, lease, "a write with nobody hosting");
                    return Followup::Nothing;
                }
                LeaseObs::Other => {
                    session.wrote_unclaimed = false;
                    session.side_copy = true;
                }
                _ => {}
            }
        }
        maybe_auto_host(slot, now, events_tx, lease);
        return Followup::Nothing;
    }
    if maybe_release_after_stop(slot, events_tx, lease) {
        return Followup::Nothing;
    }
    let Some(session) = slot.session.as_mut() else {
        return Followup::Nothing;
    };
    // The copy is on its way: only its landing ends the session.
    if session.side_copy_started {
        return Followup::Nothing;
    }
    if slot.lease == LeaseObs::Mine || slot.lease_requested {
        // Ours, or about to be: the release waits for the final flush.
        return Followup::Nothing;
    }
    let local_only = slot.role == WorldRole::View || slot.lease == LeaseObs::Other;
    // Only writes to the world are set aside: a member's pending is all world,
    // and the owner's other files go up without the lease (HRD-D-0019).
    if local_only && crate::agent::world_pending(slot) {
        // An upload still streaming the folder (the final flush of a lease
        // lost meanwhile) reads the files the copy would move: it goes first.
        if slot.in_flight.is_some() {
            return Followup::Nothing;
        }
        if let Some(session) = slot.session.as_mut() {
            session.side_copy_started = true;
        }
        return Followup::SideCopy;
    }
    end_session(slot);
    Followup::Nothing
}

/// A session is over: the role it chose goes with it, unless its writes are
/// still in the folder with nowhere to go (`local_only_pending`), which keeps
/// the slot a viewer so nothing pushes them.
fn end_session(slot: &mut SaveSlot) {
    slot.session = None;
    slot.role = if slot.local_only_pending {
        WorldRole::View
    } else {
        WorldRole::Host
    };
}

/// The session's local-only writes stay in the folder: the session is over,
/// but they must never go up as the shared head. `local_only_pending` holds
/// the slot a viewer, and every acquire off, until the reducer sees them
/// versioned or a later side copy takes them.
fn end_session_local_only(slot: &mut SaveSlot) {
    if !slot.local_only_pending {
        tracing::warn!(
            save_id = %slot.save.save_id,
            "agent: a session's local-only writes stay in the folder; this world stays a viewer until they are set aside"
        );
    }
    slot.local_only_pending = true;
    end_session(slot);
}

/// The minute is up with nobody hosting: host. `true` when it did.
pub(crate) fn maybe_auto_host(
    slot: &mut SaveSlot,
    now: Instant,
    events_tx: &mpsc::Sender<AgentEvent>,
    lease: Option<&LeaseHandle>,
) -> bool {
    let Some(session) = slot.session.as_mut() else {
        return false;
    };
    let Some(deadline) = session.auto_host_deadline else {
        return false;
    };
    if now < deadline || !session.live() {
        return false;
    }
    if session.claimed || session.dismissed || slot.role != WorldRole::Host {
        session.auto_host_deadline = None;
        return false;
    }
    if slot.lease != LeaseObs::Free {
        // Somebody took it, or the server never answered: the clock is spent
        // and evidence is what is left.
        session.auto_host_deadline = None;
        return false;
    }
    session.claimed = true;
    session.auto_host_deadline = None;
    acquire_auto(slot, events_tx, lease, "no answer to the prompt");
    true
}

/// With no session, a pending push asks for the lease (`may_request_lease`):
/// edits made between sessions, or a final flush a service stop cut short,
/// are the owner's and must go up rather than wait for the next launch. Once
/// that push lands with nothing pending or in flight, the lease goes back, or
/// the world stays hosted here with no game running. A role the user pinned
/// for the next launch (`ClaimWorld` or `ForceWorld` outside a session) keeps
/// it, and so does a game already running whose session has not opened yet.
fn maybe_release_after_push(slot: &mut SaveSlot, lease: Option<&LeaseHandle>) {
    if slot.session.is_some()
        || slot.lease != LeaseObs::Mine
        || slot.role_pinned
        || slot.is_running
        || slot.has_pending
        || slot.in_flight.is_some()
        || slot.lease_requested
        || slot.release_requested
    {
        return;
    }
    tracing::info!(save_id = %slot.save.save_id, "agent: pushed with no session running; releasing the hosting lease");
    give_back(slot, lease, true);
}

/// A world this machine hosts whose push is held for a file that cannot go up
/// (`kernel::WorldHeld`) and has parked, with no game running here: the lease
/// goes back, so another member can host rather than wait on a push that may
/// never come.
///
/// Parked, not merely held. A file locked for a few seconds (an antivirus
/// scan, an indexer) holds the first attempts too, and a lease given back then
/// lets a member host from the old head: the next attempt reads `Other` and
/// the whole last session goes to the side copy. While the push climbs its
/// ladder the lease stays here; the reducer holds on `HOLD_WORLD_HELD` ahead
/// of the lease, so nothing is asked of it meanwhile.
///
/// The rule: parked (`needs_attention`), the lease reads `Mine`, the game is
/// not running and no session is live, and nothing is in flight or already
/// asked of the lease task. It does not wait for `has_pending` to clear, which
/// is what the other two releases wait for and what a held push never does.
/// A role pinned for the next launch does not keep it, and is cleared with
/// it: a pin is a wish to host, a host that cannot push is not hosting, and a
/// pin left standing would take the lease at the next launch without asking
/// (the same invariant as HRD-F-0028).
///
/// The writes stay pending. A parked push retries only on a change to the
/// save or the user's word, so this is not a loop of acquires. If the head
/// moves meanwhile, the held writes are set aside before the pull
/// (`set_aside_behind`), as an owner's are, and the head comes down.
/// `true` when it released.
fn maybe_release_held_world(
    slot: &mut SaveSlot,
    events_tx: &mpsc::Sender<AgentEvent>,
    lease: Option<&LeaseHandle>,
) -> bool {
    if !slot.world_held.needs_attention
        || slot.lease != LeaseObs::Mine
        || slot.is_running
        || slot.session.as_ref().is_some_and(|w| w.live())
        || slot.in_flight.is_some()
        || slot.lease_requested
        || slot.release_requested
    {
        return false;
    }
    tracing::info!(
        save_id = %slot.save.save_id,
        held = slot.world_held.consecutive,
        parked = slot.world_held.needs_attention,
        "agent: the world's push is held and no game is running; releasing the hosting lease so another member can host"
    );
    slot.role_pinned = false;
    give_back(slot, lease, true);
    let _ = events_tx.try_send(AgentEvent::WorldReleased {
        save_id: slot.save.save_id.clone(),
        game_slug: slot.save.game_slug.clone(),
    });
    true
}

/// After GameStopped on a world this machine hosts: give the lease back once
/// the final flush is up, and not while anything is pending or in flight.
/// `true` when the session ended here.
pub(crate) fn maybe_release_after_stop(
    slot: &mut SaveSlot,
    events_tx: &mpsc::Sender<AgentEvent>,
    lease: Option<&LeaseHandle>,
) -> bool {
    let Some(session) = slot.session.as_ref() else {
        return false;
    };
    if session.live() || slot.lease != LeaseObs::Mine {
        return false;
    }
    // An acquire still unanswered lands here whatever the lease reads now:
    // the session waits for it, or its `Mine` would arrive on nobody.
    if slot.has_pending || slot.in_flight.is_some() || slot.lease_requested {
        return false;
    }
    let id = slot.save.save_id.clone();
    let session_secs = session.started_at.elapsed().as_secs();
    tracing::info!(save_id = %id, session_secs, "agent: session over and flushed; releasing the hosting lease");
    give_back(slot, lease, true);
    let _ = events_tx.try_send(AgentEvent::WorldReleased {
        save_id: id,
        game_slug: slot.save.game_slug.clone(),
    });
    end_session(slot);
    true
}

/// Where a session's side copy goes: the same tree the restore merge uses,
/// so one retention sweep covers both.
pub(crate) fn side_copy_dir(root: &Path, save_id: &str, at: OffsetDateTime) -> PathBuf {
    let ts = at
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown-ts".to_string())
        .replace(':', "-");
    root.join(save_id).join(ts)
}

/// Moves the world's files (the share's list, nothing else, even when the
/// save's walk takes the whole folder as the owner's does) into
/// `dir`, so the folder holds nothing newer than the head and the next pull
/// puts the head back without an mtime contest. Rename first, copy and
/// remove across filesystems. Returns how many moved.
pub(crate) async fn move_world_aside(save: &WatchedSave, dir: &Path) -> anyhow::Result<u64> {
    let shields = crate::savefilter::shields_for_slug(&save.game_slug);
    let files = crate::backup::walk_source(
        &save.local_path,
        Scope {
            shields: &shields,
            include: save.world(),
        },
    )?;
    let mut moved = 0;
    for f in files {
        let dest = dir.join(&f.relative_path);
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        if tokio::fs::rename(&f.absolute_path, &dest).await.is_err() {
            crate::restore::copy_then_remove(&f.absolute_path, &dest).await?;
        }
        moved += 1;
    }
    Ok(moved)
}

/// The side copy landed: the session's bytes are safe there, so the folder
/// may be pulled over. `has_pending` is cleared for that reason alone; the
/// reducer then owes the head a pull, which it runs once the folder is quiet.
/// The renames touched the folder, and their last watcher hits may still be
/// on their way: `last_restore_at` marks the touch as ours for the pull's
/// veto, and `side_copy_landed_at` keeps the hits from marking it pending.
pub(crate) fn on_side_copied(
    slots: &mut HashMap<String, SaveSlot>,
    save_id: &str,
    moved: u64,
    now: Instant,
    events_tx: &mpsc::Sender<AgentEvent>,
) {
    let Some(slot) = slots.get_mut(save_id) else {
        return;
    };
    // Only the session that asked for the copy is ended by it.
    if !slot
        .session
        .as_ref()
        .is_some_and(|w| !w.live() && w.side_copy_started)
    {
        return;
    }
    if moved > 0 {
        // The files are in the side copy, not in the folder: nothing local is
        // unversioned any more, and the head has to come back.
        slot.has_pending = false;
        slot.local_only_pending = false;
        // A held world push is moot: the files it could not push are in the
        // side copy.
        if std::mem::take(&mut slot.world_held).active() {
            let _ = events_tx.try_send(AgentEvent::BackupAttentionCleared {
                save_id: slot.save.save_id.clone(),
                game_slug: slot.save.game_slug.clone(),
            });
        }
        // The owner's writes outside the world are still unversioned. Marked
        // now they would veto the very pull that refills the world, so they
        // are marked again once it lands (HRD-D-0019).
        slot.recheck_pending_after_pull = slot.save.owns_whole_folder();
        slot.known_version = None;
        slot.version_base = None;
        slot.pull_pending = true;
        slot.last_restore_at = Some(OffsetDateTime::now_utc());
        slot.side_copy_landed_at = Some(now);
        end_session(slot);
    } else if slot.has_pending && world_on_disk(&slot.save) {
        // Nothing moved and something is pending: the include list missed
        // what the game wrote, and it stays where it is.
        end_session_local_only(slot);
    } else if slot.has_pending {
        // Nothing moved because nothing of the world is here: the pending
        // mark was about files outside it, which never go up with the world
        // and which the pull does not touch. Nothing is left to lose, so the
        // head comes down, rather than the slot waiting as a viewer on
        // writes that do not exist.
        tracing::info!(save_id = %slot.save.save_id, "agent: no file of the shared world is here; pulling the head");
        slot.has_pending = false;
        slot.local_only_pending = false;
        // An owner's pending writes outside the world are still unversioned:
        // marked again once the pull lands, as when the copy moved files.
        slot.recheck_pending_after_pull = slot.save.owns_whole_folder();
        slot.pull_pending = true;
        end_session(slot);
    } else {
        end_session(slot);
    }
    after_side_copy(slots, save_id, now, events_tx);
}

/// Is any file of the shared world in the save's folder? What
/// [`move_world_aside`] would move; a folder that cannot be walked counts as
/// holding some, the side that keeps a slot from pushing.
fn world_on_disk(save: &WatchedSave) -> bool {
    let shields = crate::savefilter::shields_for_slug(&save.game_slug);
    crate::backup::walk_source(
        &save.local_path,
        Scope {
            shields: &shields,
            include: save.world(),
        },
    )
    .map_or(true, |files| !files.is_empty())
}

/// The side copy could not be made: the bytes stay where they are, pending,
/// and the session is over, but the slot stays a viewer for them.
pub(crate) fn on_side_copy_failed(
    slots: &mut HashMap<String, SaveSlot>,
    save_id: &str,
    now: Instant,
    events_tx: &mpsc::Sender<AgentEvent>,
) {
    let Some(slot) = slots.get_mut(save_id) else {
        return;
    };
    if !slot
        .session
        .as_ref()
        .is_some_and(|w| !w.live() && w.side_copy_started)
    {
        return;
    }
    if slot.has_pending {
        end_session_local_only(slot);
    } else {
        end_session(slot);
    }
    after_side_copy(slots, save_id, now, events_tx);
}

/// The game relaunched while the copy was on its way (`on_game_started`
/// skipped this world): now that the old session is over, the new one
/// opens, and is asked about, as the launch would have. Unless the user
/// already chose a sibling world from the launch's prompt: that answer was
/// for this game, and the landing world opens as "not playing".
fn after_side_copy(
    slots: &mut HashMap<String, SaveSlot>,
    save_id: &str,
    now: Instant,
    events_tx: &mpsc::Sender<AgentEvent>,
) {
    let Some(slot) = slots.get_mut(save_id) else {
        return;
    };
    if !std::mem::take(&mut slot.relaunch_pending) {
        return;
    }
    let game_slug = slot.save.game_slug.clone();
    let ids = shared_of(slots, &game_slug);
    let sibling_chosen = ids.iter().any(|id| {
        id != save_id
            && slots
                .get(id)
                .and_then(|s| s.session.as_ref())
                .is_some_and(|w| w.live() && w.claimed)
    });
    tracing::info!(save_id = %save_id, sibling_chosen, "agent: side copy landed with the game running; opening its session now");
    open_sessions(
        slots,
        &game_slug,
        &[save_id.to_string()],
        ids.len(),
        sibling_chosen,
        now,
        events_tx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::test_slot;
    use hoard_core::kernel;
    use std::time::Duration;

    fn world(save_id: &str, game: &str) -> WatchedSave {
        WatchedSave {
            save_id: save_id.into(),
            game_slug: game.into(),
            display_name: game.into(),
            label: save_id.into(),
            local_path: PathBuf::from("/tmp/saves").join(save_id),
            steam_install_dir: None,
            processes: vec![],
            shared_processes: false,
            policy: Default::default(),
            allow_device_local: None,
            known_version: Some(3),
            set_hash: None,
            world_hash: None,
            track_only: false,
            shared: Some(crate::state::SharedRef {
                group_id: "g1".into(),
                group_name: "friends".into(),
                owner_user_id: "u-owner".into(),
                owner_username: "owner".into(),
                include: Vec::new(),
                caller_owns: false,
            }),
            include: Vec::new(),
        }
    }

    fn slots(saves: Vec<WatchedSave>) -> HashMap<String, SaveSlot> {
        saves
            .into_iter()
            .map(|s| (s.save_id.clone(), test_slot(s)))
            .collect()
    }

    fn drain(rx: &mut mpsc::Receiver<AgentEvent>) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        while let Ok(e) = rx.try_recv() {
            out.push(e);
        }
        out
    }

    #[tokio::test(start_paused = true)]
    async fn the_prompt_goes_out_once_with_every_shared_world_of_the_game() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut s = slots(vec![
            world("w1", "valheim"),
            world("w2", "valheim"),
            world("other", "factorio"),
        ]);
        s.get_mut("w2").unwrap().lease = LeaseObs::Other;
        s.get_mut("w2").unwrap().lease_holder = Some("bob".into());
        let now = Instant::now();
        on_game_started(&mut s, "w1", now, &tx);
        // The second save of the same game matched the same process.
        on_game_started(&mut s, "w2", now, &tx);
        let events = drain(&mut rx);
        assert_eq!(events.len(), 1, "{events:?}");
        let AgentEvent::WorldClaimWanted { game_slug, worlds } = &events[0] else {
            panic!("{events:?}");
        };
        assert_eq!(game_slug, "valheim");
        assert_eq!(worlds.len(), 2);
        assert_eq!(worlds[1].holder.as_deref(), Some("bob"));
        assert_eq!(worlds[1].lease, WorldLease::Other);
        assert_eq!(worlds[0].group_name, "friends");
        assert!(s["w1"].session.as_ref().unwrap().prompted);
        assert!(s["other"].session.is_none());
        // Two worlds: no clock.
        assert!(s["w1"]
            .session
            .as_ref()
            .unwrap()
            .auto_host_deadline
            .is_none());
        assert!(s["w2"]
            .session
            .as_ref()
            .unwrap()
            .auto_host_deadline
            .is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_prompted_session_is_in_the_status_until_it_is_answered() {
        let (tx, _rx) = mpsc::channel(8);
        let mut s = slots(vec![
            world("w1", "valheim"),
            world("w2", "valheim"),
            world("f1", "factorio"),
        ]);
        s.get_mut("w1").unwrap().lease = LeaseObs::Free;
        s.get_mut("w2").unwrap().lease = LeaseObs::Other;
        s.get_mut("w2").unwrap().lease_holder = Some("bob".into());
        let now = Instant::now();
        assert!(pending_prompts(&s, now).is_empty());

        on_game_started(&mut s, "w1", now, &tx);
        let prompts = pending_prompts(&s, now);
        assert_eq!(prompts.len(), 1, "{prompts:?}");
        assert_eq!(prompts[0].game_slug, "valheim");
        assert_eq!(prompts[0].worlds.len(), 2);
        assert_eq!(prompts[0].worlds[1].holder.as_deref(), Some("bob"));
        assert_eq!(prompts[0].worlds[1].lease, WorldLease::Other);
        // Two worlds: no clock, so no deadline on the wire.
        assert!(prompts[0].auto_host_at.is_none());

        // Hosting one world answers for the game: the other is "not playing".
        on_claim(s.get_mut("w1").unwrap());
        dismiss_siblings(&mut s, "w1");
        assert!(pending_prompts(&s, now).is_empty());
        assert!(s["w2"].session.as_ref().unwrap().dismissed);

        // One free world of another game: the clock is armed and it is on the
        // wall, sixty seconds out.
        s.get_mut("f1").unwrap().lease = LeaseObs::Free;
        on_game_started(&mut s, "f1", now, &tx);
        let prompts = pending_prompts(&s, now);
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].game_slug, "factorio");
        let at = prompts[0].auto_host_at.expect("the clock is armed");
        let secs = (at - OffsetDateTime::now_utc()).whole_seconds();
        assert!((58..=60).contains(&secs), "{secs}");
        assert!(prompts[0].raised_at <= OffsetDateTime::now_utc());

        // "Not playing" takes it out too, and so does the game closing.
        on_dismiss(s.get_mut("f1").unwrap());
        assert!(pending_prompts(&s, now).is_empty());
        let mut again = slots(vec![world("g1", "grounded")]);
        on_game_started(&mut again, "g1", now, &tx);
        assert_eq!(pending_prompts(&again, now).len(), 1);
        on_game_stopped(again.get_mut("g1").unwrap());
        assert!(pending_prompts(&again, now).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_game_without_shared_worlds_gets_no_prompt() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut plain = world("p1", "factorio");
        plain.shared = None;
        let mut s = slots(vec![plain]);
        on_game_started(&mut s, "p1", Instant::now(), &tx);
        assert!(drain(&mut rx).is_empty());
        assert!(s["p1"].session.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn the_clock_is_armed_only_for_one_free_world() {
        let (tx, _rx) = mpsc::channel(8);
        for (lease, armed) in [
            (LeaseObs::Free, true),
            (LeaseObs::Unknown, true),
            (LeaseObs::Other, false),
            (LeaseObs::Mine, false),
        ] {
            let mut s = slots(vec![world("w1", "valheim")]);
            s.get_mut("w1").unwrap().lease = lease;
            let now = Instant::now();
            on_game_started(&mut s, "w1", now, &tx);
            let session = s["w1"].session.as_ref().unwrap();
            assert_eq!(
                session.auto_host_deadline,
                armed.then(|| now + Duration::from_secs(AUTO_HOST_DELAY_SECS)),
                "{lease:?}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_clock_hosts_the_one_free_world_and_says_it_was_the_engine() {
        let (tx, mut rx) = mpsc::channel(8);
        let (lease, mut seen) = LeaseHandle::probe();
        let mut s = slots(vec![world("w1", "valheim")]);
        let start = Instant::now();
        on_game_started(&mut s, "w1", start, &tx);
        drain(&mut rx);
        let slot = s.get_mut("w1").unwrap();
        // Unknown at launch: the first pass asks the server; the clock reads
        // the answer.
        assert_eq!(
            on_reconciled(slot, start, &tx, Some(&lease)),
            Followup::Nothing
        );
        tokio::task::yield_now().await;
        assert_eq!(seen.try_recv().unwrap(), "refresh w1");
        slot.lease = LeaseObs::Free;
        let early = start + Duration::from_secs(AUTO_HOST_DELAY_SECS - 1);
        assert!(!maybe_auto_host(slot, early, &tx, Some(&lease)));
        assert!(drain(&mut rx).is_empty());
        let due = start + Duration::from_secs(AUTO_HOST_DELAY_SECS);
        assert!(maybe_auto_host(slot, due, &tx, Some(&lease)));
        tokio::task::yield_now().await;
        assert_eq!(seen.try_recv().unwrap(), "acquire w1");
        assert!(slot.lease_requested);
        let events = drain(&mut rx);
        assert!(
            matches!(
                events.as_slice(),
                [AgentEvent::WorldClaimed {
                    role: WorldRole::Host,
                    auto: true,
                    ..
                }]
            ),
            "{events:?}"
        );
        // Once.
        assert!(!maybe_auto_host(
            slot,
            due + Duration::from_secs(5),
            &tx,
            Some(&lease)
        ));
        assert!(slot.session.as_ref().unwrap().claimed);
    }

    #[tokio::test(start_paused = true)]
    async fn dismissed_or_answered_or_taken_never_auto_hosts() {
        let (tx, mut rx) = mpsc::channel(8);
        let (lease, mut seen) = LeaseHandle::probe();
        let start = Instant::now();
        let due = start + Duration::from_secs(AUTO_HOST_DELAY_SECS);
        for case in ["dismissed", "answered", "taken", "stopped"] {
            let mut s = slots(vec![world("w1", "valheim")]);
            s.get_mut("w1").unwrap().lease = LeaseObs::Free;
            on_game_started(&mut s, "w1", start, &tx);
            let slot = s.get_mut("w1").unwrap();
            match case {
                "dismissed" => on_dismiss(slot),
                "answered" => on_claim(slot),
                "taken" => slot.lease = LeaseObs::Other,
                "stopped" => on_game_stopped(slot),
                _ => unreachable!(),
            }
            drain(&mut rx);
            assert!(!maybe_auto_host(slot, due, &tx, Some(&lease)), "{case}");
            tokio::task::yield_now().await;
            assert!(seen.try_recv().is_err(), "{case}: asked for the lease");
            assert!(drain(&mut rx).is_empty(), "{case}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_write_with_nobody_hosting_claims_once() {
        let (tx, mut rx) = mpsc::channel(8);
        let (lease, mut seen) = LeaseHandle::probe();
        let mut s = slots(vec![world("w1", "valheim")]);
        s.get_mut("w1").unwrap().lease = LeaseObs::Free;
        on_game_started(&mut s, "w1", Instant::now(), &tx);
        drain(&mut rx);
        let slot = s.get_mut("w1").unwrap();
        on_write(slot, &tx, Some(&lease));
        on_write(slot, &tx, Some(&lease));
        tokio::task::yield_now().await;
        assert_eq!(seen.try_recv().unwrap(), "acquire w1");
        assert!(seen.try_recv().is_err(), "acquired twice");
        let events = drain(&mut rx);
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(matches!(
            events[0],
            AgentEvent::WorldClaimed {
                auto: true,
                role: WorldRole::Host,
                ..
            }
        ));
        // The clock is spent by the claim.
        assert!(slot.session.as_ref().unwrap().auto_host_deadline.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_write_under_somebody_elses_lease_goes_local() {
        let (tx, mut rx) = mpsc::channel(8);
        let (lease, mut seen) = LeaseHandle::probe();
        let mut s = slots(vec![world("w1", "valheim")]);
        s.get_mut("w1").unwrap().lease = LeaseObs::Other;
        on_game_started(&mut s, "w1", Instant::now(), &tx);
        drain(&mut rx);
        let slot = s.get_mut("w1").unwrap();
        on_write(slot, &tx, Some(&lease));
        tokio::task::yield_now().await;
        assert!(seen.try_recv().is_err());
        assert!(
            drain(&mut rx).is_empty(),
            "the hold tells the user, not this"
        );
        assert!(slot.session.as_ref().unwrap().side_copy);
    }

    #[tokio::test(start_paused = true)]
    async fn a_viewer_is_told_on_the_second_write_once() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut s = slots(vec![world("w1", "valheim")]);
        s.get_mut("w1").unwrap().lease = LeaseObs::Other;
        on_game_started(&mut s, "w1", Instant::now(), &tx);
        drain(&mut rx);
        let slot = s.get_mut("w1").unwrap();
        slot.role = WorldRole::View;
        on_claim(slot);
        on_write(slot, &tx, None);
        assert!(drain(&mut rx).is_empty(), "one write is a save at exit");
        on_write(slot, &tx, None);
        let events = drain(&mut rx);
        assert!(
            matches!(events.as_slice(), [AgentEvent::ViewSessionWriting { .. }]),
            "{events:?}"
        );
        on_write(slot, &tx, None);
        assert!(drain(&mut rx).is_empty());
        assert_eq!(slot.session.as_ref().unwrap().writes, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn the_lease_goes_back_only_once_nothing_is_pending_or_in_flight() {
        let (tx, mut rx) = mpsc::channel(8);
        let (lease, mut seen) = LeaseHandle::probe();
        let mut s = slots(vec![world("w1", "valheim")]);
        s.get_mut("w1").unwrap().lease = LeaseObs::Mine;
        let now = Instant::now();
        on_game_started(&mut s, "w1", now, &tx);
        drain(&mut rx);
        let slot = s.get_mut("w1").unwrap();
        slot.has_pending = true;
        // Still playing: nothing.
        assert!(!maybe_release_after_stop(slot, &tx, Some(&lease)));
        on_game_stopped(slot);
        // The final flush is owed.
        assert!(!maybe_release_after_stop(slot, &tx, Some(&lease)));
        slot.has_pending = false;
        slot.in_flight = Some(kernel::Op::Backup);
        // And in flight.
        assert_eq!(
            on_reconciled(slot, now, &tx, Some(&lease)),
            Followup::Nothing
        );
        assert!(slot.session.is_some());
        slot.in_flight = None;
        assert_eq!(
            on_reconciled(slot, now, &tx, Some(&lease)),
            Followup::Nothing
        );
        tokio::task::yield_now().await;
        assert_eq!(seen.try_recv().unwrap(), "release w1");
        let events = drain(&mut rx);
        assert!(
            matches!(events.as_slice(), [AgentEvent::WorldReleased { .. }]),
            "{events:?}"
        );
        assert!(slot.session.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_viewers_writes_end_in_a_side_copy_and_the_head_comes_back() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut s = slots(vec![world("w1", "valheim")]);
        s.get_mut("w1").unwrap().lease = LeaseObs::Other;
        let now = Instant::now();
        on_game_started(&mut s, "w1", now, &tx);
        drain(&mut rx);
        {
            let slot = s.get_mut("w1").unwrap();
            slot.role = WorldRole::View;
            slot.has_pending = true;
            on_game_stopped(slot);
            assert_eq!(on_reconciled(slot, now, &tx, None), Followup::SideCopy);
            // Asked once; the next pass waits for it to land.
            assert_eq!(on_reconciled(slot, now, &tx, None), Followup::Nothing);
        }
        on_side_copied(&mut s, "w1", 2, now, &tx);
        let slot = &s["w1"];
        assert!(!slot.has_pending);
        assert!(slot.pull_pending);
        assert_eq!(slot.known_version, None);
        assert!(slot.session.is_none());
        assert!(drain(&mut rx).is_empty(), "the game is not running");
    }

    /// The copy's renames reach the watcher too, some after the landing:
    /// none of them marks the emptied folder pending, and the touch reads as
    /// ours to the pull's veto.
    #[tokio::test(start_paused = true)]
    async fn the_side_copys_own_hits_are_not_pending() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut s = slots(vec![world("w1", "valheim")]);
        s.get_mut("w1").unwrap().lease = LeaseObs::Other;
        let now = Instant::now();
        on_game_started(&mut s, "w1", now, &tx);
        drain(&mut rx);
        {
            let slot = s.get_mut("w1").unwrap();
            assert!(!hit_is_side_copy(slot, now), "a write while playing");
            slot.role = WorldRole::View;
            slot.has_pending = true;
            on_game_stopped(slot);
            assert_eq!(on_reconciled(slot, now, &tx, None), Followup::SideCopy);
            assert!(hit_is_side_copy(slot, now), "a rename while the copy runs");
        }
        let landed = now + Duration::from_secs(3);
        on_side_copied(&mut s, "w1", 2, landed, &tx);
        let slot = &s["w1"];
        assert!(slot.session.is_none());
        assert!(!slot.has_pending);
        assert!(slot.last_restore_at.is_some(), "the touch is ours");
        assert!(
            hit_is_side_copy(slot, landed + Duration::from_secs(2)),
            "the debouncer's tail"
        );
        assert!(
            !hit_is_side_copy(slot, landed + SIDE_COPY_TAIL),
            "after the tail a write is a write"
        );
    }

    /// Behind the head with a pending mark and nothing of the world in the
    /// folder (a member's own character beside it): the set-aside before the
    /// pull moves nothing, and that is not a write left local-only. The slot
    /// stays a host, nothing is pending, and the head comes down.
    #[tokio::test(start_paused = true)]
    async fn a_side_copy_with_no_world_on_disk_lets_the_pull_through() {
        let (tx, _rx) = mpsc::channel(8);
        let tmp = tempfile::tempdir().unwrap();
        let character = tmp.path().join("characters_local/Friend.fch");
        std::fs::create_dir_all(character.parent().unwrap()).unwrap();
        std::fs::write(&character, b"friend").unwrap();
        let mut save = world("w1", "valheim");
        save.local_path = tmp.path().to_path_buf();
        let list = crate::worldfiles::template("valheim", "Alpha")
            .unwrap()
            .unwrap();
        save.include = list.clone();
        save.shared.as_mut().unwrap().include = list;
        let mut s = slots(vec![save.clone()]);
        let now = Instant::now();
        {
            let slot = s.get_mut("w1").unwrap();
            slot.lease = LeaseObs::Free;
            slot.has_pending = true;
            on_stale(slot, 3);
            assert!(!slot.pull_pending, "held back by the pending mark");
            assert_eq!(on_reconciled(slot, now, &tx, None), Followup::SideCopy);
        }
        let moved = move_world_aside(&save, &tmp.path().join("aside"))
            .await
            .unwrap();
        assert_eq!(moved, 0);
        on_side_copied(&mut s, "w1", moved, now, &tx);
        let slot = &s["w1"];
        assert!(slot.session.is_none());
        assert!(!slot.has_pending);
        assert!(!slot.local_only_pending);
        assert!(slot.pull_pending);
        assert_eq!(slot.role, WorldRole::Host);
        assert!(!may_request_lease(slot));
        assert_eq!(std::fs::read(&character).unwrap(), b"friend");
    }

    /// The side copy is skipped or fails with writes still in the folder: the
    /// session ends, but the slot stays a viewer and asks for no lease until
    /// they are versioned or set aside, so the next tick cannot push them.
    #[tokio::test(start_paused = true)]
    async fn a_failed_side_copy_keeps_the_writes_local_only() {
        let (tx, mut rx) = mpsc::channel(8);
        let now = Instant::now();
        for moved in [None, Some(0)] {
            let mut s = slots(vec![world("w1", "valheim")]);
            s.get_mut("w1").unwrap().lease = LeaseObs::Free;
            on_game_started(&mut s, "w1", now, &tx);
            drain(&mut rx);
            {
                let slot = s.get_mut("w1").unwrap();
                slot.role = WorldRole::View;
                on_claim(slot);
                slot.has_pending = true;
                on_game_stopped(slot);
                assert_eq!(on_reconciled(slot, now, &tx, None), Followup::SideCopy);
            }
            match moved {
                None => on_side_copy_failed(&mut s, "w1", now, &tx),
                Some(n) => on_side_copied(&mut s, "w1", n, now, &tx),
            }
            let slot = s.get_mut("w1").unwrap();
            assert!(slot.session.is_none(), "{moved:?}");
            assert_eq!(slot.role, WorldRole::View, "{moved:?}");
            assert!(slot.local_only_pending, "{moved:?}");
            assert!(slot.has_pending, "{moved:?}");
            assert!(
                !may_request_lease(slot),
                "{moved:?}: the hold would acquire"
            );
            // The next session starts as a viewer too.
            on_game_started(&mut s, "w1", now, &tx);
            assert_eq!(drain(&mut rx).len(), 1, "{moved:?}: asked again");
            let slot = s.get_mut("w1").unwrap();
            assert_eq!(slot.role, WorldRole::View, "{moved:?}");
            assert!(!may_request_lease(slot), "{moved:?}");
            // Versioned at last (the user hosted and pushed): a host again.
            slot.has_pending = false;
            slot.local_only_pending = false;
            on_game_stopped(slot);
            assert_eq!(on_reconciled(slot, now, &tx, None), Followup::Nothing);
            assert_eq!(slot.role, WorldRole::Host, "{moved:?}");
        }
    }

    /// A lease this machine still holds at launch is a role, not a question:
    /// no prompt. Viewing it anyway gives the lease back, or the reducer
    /// would keep pushing.
    #[tokio::test(start_paused = true)]
    async fn a_world_still_held_is_not_asked_about_and_viewing_it_releases() {
        let (tx, mut rx) = mpsc::channel(8);
        let (lease, mut seen) = LeaseHandle::probe();
        let mut s = slots(vec![world("w1", "valheim")]);
        s.get_mut("w1").unwrap().lease = LeaseObs::Mine;
        let now = Instant::now();
        on_game_started(&mut s, "w1", now, &tx);
        assert!(drain(&mut rx).is_empty(), "hosting already");
        let slot = s.get_mut("w1").unwrap();
        let session = slot.session.as_ref().unwrap();
        assert!(session.claimed && !session.prompted && session.live());
        // The second save of the game matched: still one session, no prompt.
        on_game_started(&mut s, "w1", now, &tx);
        assert!(drain(&mut rx).is_empty());
        let slot = s.get_mut("w1").unwrap();
        on_claim_world(slot, WorldRole::View, &tx, Some(&lease));
        tokio::task::yield_now().await;
        assert_eq!(seen.try_recv().unwrap(), "release w1");
        assert_eq!(slot.role, WorldRole::View);
        assert!(
            !slot.role_pinned,
            "a role taken in a session is the session's"
        );
        let events = drain(&mut rx);
        assert!(
            matches!(
                events.as_slice(),
                [AgentEvent::WorldClaimed {
                    role: WorldRole::View,
                    auto: false,
                    ..
                }]
            ),
            "{events:?}"
        );
    }

    /// A write under a lease still unknown is the claim flow's to turn into
    /// an acquire, not the reducer's hold: the hold waits, the refresh reads
    /// free, and the claim goes out as the engine's with the prompt closed by
    /// it.
    #[tokio::test(start_paused = true)]
    async fn a_write_under_an_unknown_lease_is_acquired_by_the_claim_flow() {
        let (tx, mut rx) = mpsc::channel(8);
        let (lease, mut seen) = LeaseHandle::probe();
        let mut s = slots(vec![world("w1", "valheim")]);
        let now = Instant::now();
        on_game_started(&mut s, "w1", now, &tx);
        drain(&mut rx);
        let slot = s.get_mut("w1").unwrap();
        slot.has_pending = true;
        on_write(slot, &tx, Some(&lease));
        assert!(
            !may_request_lease(slot),
            "the hold acquired around the prompt"
        );
        on_lease(slot, LeaseObs::Free, None, false, &tx, None);
        assert!(!may_request_lease(slot));
        assert_eq!(
            on_reconciled(slot, now, &tx, Some(&lease)),
            Followup::Nothing
        );
        tokio::task::yield_now().await;
        assert_eq!(seen.try_recv().unwrap(), "acquire w1");
        let events = drain(&mut rx);
        assert!(
            matches!(
                events.as_slice(),
                [AgentEvent::WorldClaimed {
                    auto: true,
                    role: WorldRole::Host,
                    ..
                }]
            ),
            "{events:?}"
        );
        assert!(slot.session.as_ref().unwrap().claimed);
        // Claimed: a later hold (the acquire refused as stale, say) may ask.
        slot.lease_requested = false;
        assert!(may_request_lease(slot));
    }

    /// A lease lost during the final flush: the side copy waits for the
    /// upload, which reads the very files it would move.
    #[tokio::test(start_paused = true)]
    async fn the_side_copy_waits_for_the_upload_in_flight() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut s = slots(vec![world("w1", "valheim")]);
        s.get_mut("w1").unwrap().lease = LeaseObs::Mine;
        let now = Instant::now();
        on_game_started(&mut s, "w1", now, &tx);
        drain(&mut rx);
        let slot = s.get_mut("w1").unwrap();
        slot.has_pending = true;
        on_game_stopped(slot);
        slot.in_flight = Some(kernel::Op::Backup);
        on_lease(slot, LeaseObs::Other, Some("bob".into()), false, &tx, None);
        assert_eq!(on_reconciled(slot, now, &tx, None), Followup::Nothing);
        assert!(slot.session.is_some());
        assert!(!slot.session.as_ref().unwrap().side_copy_started);
        slot.in_flight = None;
        assert_eq!(on_reconciled(slot, now, &tx, None), Followup::SideCopy);
    }

    /// The game was found through one world's activity: its stop closes the
    /// siblings' sessions too, except one whose own process still runs.
    #[tokio::test(start_paused = true)]
    async fn a_siblings_session_closes_with_the_game() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut s = slots(vec![
            world("w1", "valheim"),
            world("w2", "valheim"),
            world("w3", "valheim"),
        ]);
        let now = Instant::now();
        on_game_started(&mut s, "w1", now, &tx);
        drain(&mut rx);
        s.get_mut("w3").unwrap().is_running = true;
        on_game_stopped_all(&mut s, "w1");
        assert!(!s["w1"].session.as_ref().unwrap().live());
        assert!(!s["w2"].session.as_ref().unwrap().live());
        assert!(
            s["w3"].session.as_ref().unwrap().live(),
            "its own process runs"
        );
        // And the next launch asks again once the sessions are gone.
        for id in ["w1", "w2"] {
            let slot = s.get_mut(id).unwrap();
            assert_eq!(on_reconciled(slot, now, &tx, None), Followup::Nothing);
            assert!(slot.session.is_none());
        }
    }

    /// A refresh answering between the click and the acquire's verdict is an
    /// observation: the session waits for the verdict, and a stop in between
    /// ends nothing.
    #[tokio::test(start_paused = true)]
    async fn an_observed_lease_does_not_answer_the_acquire() {
        let (tx, mut rx) = mpsc::channel(8);
        let (lease, mut seen) = LeaseHandle::probe();
        let mut s = slots(vec![world("w1", "valheim")]);
        let now = Instant::now();
        on_game_started(&mut s, "w1", now, &tx);
        drain(&mut rx);
        let slot = s.get_mut("w1").unwrap();
        on_claim_world(slot, WorldRole::Host, &tx, Some(&lease));
        drain(&mut rx);
        assert!(slot.lease_requested);
        // The refresh queued before the click answers first.
        on_lease(slot, LeaseObs::Free, None, false, &tx, None);
        assert!(slot.lease_requested, "an observation answers no acquire");
        on_game_stopped(slot);
        assert_eq!(
            on_reconciled(slot, now, &tx, Some(&lease)),
            Followup::Nothing
        );
        assert!(
            slot.session.is_some(),
            "the acquire is still owed an answer"
        );
        // A frame reading mine before the verdict changes nothing either.
        on_lease(slot, LeaseObs::Mine, Some("me".into()), false, &tx, None);
        assert_eq!(
            on_reconciled(slot, now, &tx, Some(&lease)),
            Followup::Nothing
        );
        assert!(slot.session.is_some());
        on_lease(slot, LeaseObs::Mine, Some("me".into()), true, &tx, None);
        assert!(!slot.lease_requested);
        assert_eq!(
            on_reconciled(slot, now, &tx, Some(&lease)),
            Followup::Nothing
        );
        assert!(slot.session.is_none(), "released once the verdict landed");
        tokio::task::yield_now().await;
        let mut lines = Vec::new();
        while let Ok(l) = seen.try_recv() {
            lines.push(l);
        }
        assert_eq!(lines, ["acquire w1", "release w1"]);
        assert!(matches!(
            drain(&mut rx).as_slice(),
            [AgentEvent::WorldReleased { .. }]
        ));
    }

    /// A role chosen before the process was seen is the next session's
    /// answer: kept, not asked again, and gone with that session.
    #[tokio::test(start_paused = true)]
    async fn a_role_chosen_before_launch_is_kept_for_the_session() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut s = slots(vec![world("w1", "valheim")]);
        s.get_mut("w1").unwrap().lease = LeaseObs::Free;
        let now = Instant::now();
        let slot = s.get_mut("w1").unwrap();
        on_claim_world(slot, WorldRole::View, &tx, None);
        drain(&mut rx);
        assert!(slot.role_pinned);
        on_game_started(&mut s, "w1", now, &tx);
        assert!(drain(&mut rx).is_empty(), "answered already");
        let slot = s.get_mut("w1").unwrap();
        assert_eq!(slot.role, WorldRole::View);
        assert!(!slot.role_pinned, "the pin is spent on the session");
        let session = slot.session.as_ref().unwrap();
        assert!(session.claimed && session.auto_host_deadline.is_none());
        // A write is a viewer's: no acquire.
        on_write(slot, &tx, None);
        assert_eq!(slot.session.as_ref().unwrap().writes, 1);
        on_game_stopped(slot);
        assert_eq!(on_reconciled(slot, now, &tx, None), Followup::Nothing);
        assert_eq!(slot.role, WorldRole::Host, "for one session");
        on_game_started(&mut s, "w1", now, &tx);
        assert_eq!(drain(&mut rx).len(), 1, "the next launch asks");
    }

    #[tokio::test(start_paused = true)]
    async fn a_session_with_nothing_owed_just_ends() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut s = slots(vec![world("w1", "valheim")]);
        s.get_mut("w1").unwrap().lease = LeaseObs::Free;
        let now = Instant::now();
        on_game_started(&mut s, "w1", now, &tx);
        drain(&mut rx);
        let slot = s.get_mut("w1").unwrap();
        on_game_stopped(slot);
        assert_eq!(on_reconciled(slot, now, &tx, None), Followup::Nothing);
        assert!(slot.session.is_none());
        assert!(drain(&mut rx).is_empty());
        // And the next launch asks again.
        on_game_started(&mut s, "w1", now, &tx);
        assert_eq!(drain(&mut rx).len(), 1);
    }

    /// Viewing is for one session: the next launch of the same world starts
    /// as a host again, clock and all.
    #[tokio::test(start_paused = true)]
    async fn a_view_choice_ends_with_its_session() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut s = slots(vec![world("w1", "valheim")]);
        s.get_mut("w1").unwrap().lease = LeaseObs::Free;
        let now = Instant::now();
        on_game_started(&mut s, "w1", now, &tx);
        drain(&mut rx);
        let slot = s.get_mut("w1").unwrap();
        slot.role = WorldRole::View;
        on_claim(slot);
        on_game_stopped(slot);
        assert_eq!(on_reconciled(slot, now, &tx, None), Followup::Nothing);
        assert!(slot.session.is_none());
        assert_eq!(slot.role, WorldRole::Host);
        on_game_started(&mut s, "w1", now, &tx);
        assert_eq!(drain(&mut rx).len(), 1);
        assert!(s["w1"]
            .session
            .as_ref()
            .unwrap()
            .auto_host_deadline
            .is_some());
    }

    /// The game relaunched while the side copy was on its way: the old
    /// session keeps the copy, the new launch asks nothing about that world
    /// yet, and the landing ends the old session and opens the new one, with
    /// its prompt. GameStarted is edge-triggered: nothing asks a third time.
    #[tokio::test(start_paused = true)]
    async fn a_relaunch_waits_for_the_side_copy_to_land() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut s = slots(vec![world("w1", "valheim")]);
        s.get_mut("w1").unwrap().lease = LeaseObs::Other;
        let now = Instant::now();
        on_game_started(&mut s, "w1", now, &tx);
        drain(&mut rx);
        {
            let slot = s.get_mut("w1").unwrap();
            slot.role = WorldRole::View;
            slot.has_pending = true;
            on_game_stopped(slot);
            assert_eq!(on_reconciled(slot, now, &tx, None), Followup::SideCopy);
        }
        on_game_started(&mut s, "w1", now, &tx);
        assert!(
            drain(&mut rx).is_empty(),
            "no prompt while the copy is landing"
        );
        {
            let slot = &s["w1"];
            assert!(slot.session.as_ref().unwrap().side_copy_started);
            assert!(!slot.session.as_ref().unwrap().live());
            assert!(slot.relaunch_pending);
        }
        on_side_copied(&mut s, "w1", 2, now, &tx);
        let slot = &s["w1"];
        assert!(!slot.has_pending);
        assert_eq!(slot.role, WorldRole::Host);
        assert!(!slot.relaunch_pending);
        let session = slot.session.as_ref().unwrap();
        assert!(session.live() && session.prompted && !session.side_copy_started);
        assert_eq!(drain(&mut rx).len(), 1, "the landing asks for the relaunch");
        // Stopped again before the copy landed: nothing to open.
        let mut s = slots(vec![world("w1", "valheim")]);
        s.get_mut("w1").unwrap().lease = LeaseObs::Other;
        on_game_started(&mut s, "w1", now, &tx);
        drain(&mut rx);
        {
            let slot = s.get_mut("w1").unwrap();
            slot.role = WorldRole::View;
            slot.has_pending = true;
            on_game_stopped(slot);
            assert_eq!(on_reconciled(slot, now, &tx, None), Followup::SideCopy);
        }
        on_game_started(&mut s, "w1", now, &tx);
        on_game_stopped_all(&mut s, "w1");
        on_side_copied(&mut s, "w1", 2, now, &tx);
        assert!(s["w1"].session.is_none());
        assert!(drain(&mut rx).is_empty());
    }

    /// A write while nobody knows who holds the lease is not lost: it claims
    /// the moment the refresh reads free.
    #[tokio::test(start_paused = true)]
    async fn a_write_under_an_unknown_lease_claims_once_it_reads_free() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut s = slots(vec![world("w1", "valheim")]);
        let now = Instant::now();
        on_game_started(&mut s, "w1", now, &tx);
        drain(&mut rx);
        let slot = s.get_mut("w1").unwrap();
        assert_eq!(slot.lease, LeaseObs::Unknown);
        on_write(slot, &tx, None);
        assert!(drain(&mut rx).is_empty());
        assert!(slot.session.as_ref().unwrap().wrote_unclaimed);
        slot.lease = LeaseObs::Free;
        assert_eq!(on_reconciled(slot, now, &tx, None), Followup::Nothing);
        let events = drain(&mut rx);
        assert!(
            matches!(
                events.as_slice(),
                [AgentEvent::WorldClaimed { auto: true, .. }]
            ),
            "{events:?}"
        );
        assert!(slot.session.as_ref().unwrap().claimed);
    }

    /// Every release this machine asks for (after the final flush,
    /// `ReleaseWorld`, viewing a world it hosts) answers with the lease free:
    /// none of them is a lost lease.
    #[tokio::test(start_paused = true)]
    async fn a_release_this_machine_asked_for_is_not_a_lost_lease() {
        let (tx, mut rx) = mpsc::channel(8);
        let (lease, mut seen) = LeaseHandle::probe();
        let now = Instant::now();
        for path in ["after the flush", "release", "view"] {
            let mut s = slots(vec![world("w1", "valheim")]);
            s.get_mut("w1").unwrap().lease = LeaseObs::Mine;
            on_game_started(&mut s, "w1", now, &tx);
            let slot = s.get_mut("w1").unwrap();
            match path {
                "after the flush" => {
                    on_game_stopped(slot);
                    assert_eq!(
                        on_reconciled(slot, now, &tx, Some(&lease)),
                        Followup::Nothing
                    );
                }
                "release" => on_release_world(slot, &tx, Some(&lease)),
                "view" => on_claim_world(slot, WorldRole::View, &tx, Some(&lease)),
                _ => unreachable!(),
            }
            tokio::task::yield_now().await;
            assert_eq!(seen.try_recv().unwrap(), "release w1", "{path}");
            assert!(slot.release_requested, "{path}");
            drain(&mut rx);
            // The task's answer to the release.
            on_lease(slot, LeaseObs::Free, None, false, &tx, Some(&lease));
            assert!(!slot.release_requested, "{path}");
            assert!(drain(&mut rx).is_empty(), "{path}: told as a lost lease");
        }
    }

    /// H-B: a world whose push is held for a file that cannot be read gives
    /// its lease back once it parks and no game runs here, with the writes
    /// still pending: another member can host. Not while the game runs, not
    /// with a session live, and not while the push is still on its ladder
    /// (H-B-1).
    #[tokio::test(start_paused = true)]
    async fn a_held_world_gives_the_lease_back_with_no_game_running() {
        let (tx, mut rx) = mpsc::channel(8);
        let (lease, mut seen) = LeaseHandle::probe();
        let now = Instant::now();
        let held = |s: &mut HashMap<String, SaveSlot>| {
            let slot = s.get_mut("w1").unwrap();
            slot.lease = LeaseObs::Mine;
            slot.has_pending = true;
            slot.world_held = kernel::WorldHeld {
                consecutive: kernel::reconcile::WORLD_HELD_GIVE_UP_AFTER,
                needs_attention: true,
            };
        };
        for case in ["no session", "session stopped", "parked, pinned"] {
            let mut s = slots(vec![world("w1", "valheim")]);
            held(&mut s);
            if case == "session stopped" {
                on_game_started(&mut s, "w1", now, &tx);
                on_game_stopped(s.get_mut("w1").unwrap());
            }
            if case == "parked, pinned" {
                s.get_mut("w1").unwrap().role_pinned = true;
            }
            drain(&mut rx);
            let slot = s.get_mut("w1").unwrap();
            assert_eq!(
                on_reconciled(slot, now, &tx, Some(&lease)),
                Followup::Nothing
            );
            tokio::task::yield_now().await;
            assert_eq!(seen.try_recv().unwrap(), "release w1", "{case}");
            assert!(slot.has_pending, "{case}: the writes stay pending");
            // M-1: the pin goes with the lease, or the next launch hosts
            // without asking.
            assert!(!slot.role_pinned, "{case}: the pin outlived the release");
            assert!(
                drain(&mut rx)
                    .iter()
                    .any(|e| matches!(e, AgentEvent::WorldReleased { .. })),
                "{case}"
            );
        }
        // Held but still on its ladder (H-B-1): a file locked for seconds must
        // not hand the world to a member who would host from the old head.
        for case in [
            "running",
            "session live",
            "not held",
            "held once",
            "held, session stopped",
        ] {
            let mut s = slots(vec![world("w1", "valheim")]);
            held(&mut s);
            let on_ladder = kernel::WorldHeld {
                consecutive: 1,
                needs_attention: false,
            };
            match case {
                "running" => s.get_mut("w1").unwrap().is_running = true,
                "session live" => on_game_started(&mut s, "w1", now, &tx),
                "held once" => s.get_mut("w1").unwrap().world_held = on_ladder,
                "held, session stopped" => {
                    s.get_mut("w1").unwrap().world_held = on_ladder;
                    on_game_started(&mut s, "w1", now, &tx);
                    on_game_stopped(s.get_mut("w1").unwrap());
                }
                _ => s.get_mut("w1").unwrap().world_held = kernel::WorldHeld::default(),
            }
            let slot = s.get_mut("w1").unwrap();
            on_reconciled(slot, now, &tx, Some(&lease));
            tokio::task::yield_now().await;
            assert!(seen.try_recv().is_err(), "{case}");
        }
    }

    /// H-B: once somebody else hosts, a held world's writes can never go up,
    /// so they are set aside before the pull, as an owner's are.
    #[tokio::test(start_paused = true)]
    async fn a_held_world_hosted_elsewhere_is_set_aside() {
        let now = Instant::now();
        for held in [true, false] {
            let (tx, _rx) = mpsc::channel(8);
            let mut s = slots(vec![world("w1", "valheim")]);
            let slot = s.get_mut("w1").unwrap();
            slot.lease = LeaseObs::Other;
            slot.has_pending = true;
            if held {
                slot.world_held.consecutive = 2;
            }
            let expected = if held {
                Followup::SideCopy
            } else {
                Followup::Nothing
            };
            assert_eq!(on_reconciled(slot, now, &tx, None), expected, "held={held}");
        }
    }

    /// HRD-F-0028: a claim made outside a session and then released leaves no
    /// pin behind, so a later push with no session gives the lease back once
    /// it is up, as any sessionless push does.
    #[tokio::test(start_paused = true)]
    async fn a_released_claim_does_not_keep_a_later_push_s_lease() {
        let (tx, _rx) = mpsc::channel(16);
        let (lease, mut seen) = LeaseHandle::probe();
        let now = Instant::now();
        let mut s = slots(vec![world("w1", "valheim")]);
        let slot = s.get_mut("w1").unwrap();
        on_claim_world(slot, WorldRole::Host, &tx, Some(&lease));
        assert!(slot.role_pinned);
        on_lease(slot, LeaseObs::Mine, None, true, &tx, Some(&lease));
        on_release_world(slot, &tx, Some(&lease));
        on_lease(slot, LeaseObs::Free, None, false, &tx, Some(&lease));
        assert!(!slot.role_pinned);
        tokio::task::yield_now().await;
        while seen.try_recv().is_ok() {}

        // A push with no session takes the lease, and goes up.
        slot.has_pending = true;
        request_acquire(slot, &lease);
        on_lease(slot, LeaseObs::Mine, None, true, &tx, Some(&lease));
        slot.has_pending = false;
        tokio::task::yield_now().await;
        while seen.try_recv().is_ok() {}
        assert_eq!(
            on_reconciled(slot, now, &tx, Some(&lease)),
            Followup::Nothing
        );
        tokio::task::yield_now().await;
        assert_eq!(seen.try_recv().unwrap(), "release w1");
    }

    /// A lease taken off this machine, or gone while the renew could not get
    /// through, is lost, and still is after an earlier release was answered.
    #[tokio::test(start_paused = true)]
    async fn a_forced_or_expired_lease_is_still_lost() {
        let (tx, mut rx) = mpsc::channel(8);
        let (lease, mut seen) = LeaseHandle::probe();
        for (obs, holder) in [(LeaseObs::Other, Some("bob")), (LeaseObs::Free, None)] {
            let mut s = slots(vec![world("w1", "valheim")]);
            let slot = s.get_mut("w1").unwrap();
            slot.lease = LeaseObs::Mine;
            on_release_world(slot, &tx, Some(&lease));
            on_lease(slot, LeaseObs::Free, None, false, &tx, Some(&lease));
            // Hosting again.
            on_claim_world(slot, WorldRole::Host, &tx, Some(&lease));
            on_lease(
                slot,
                LeaseObs::Mine,
                Some("me".into()),
                true,
                &tx,
                Some(&lease),
            );
            assert_eq!(slot.lease, LeaseObs::Mine);
            drain(&mut rx);
            on_lease(
                slot,
                obs,
                holder.map(String::from),
                false,
                &tx,
                Some(&lease),
            );
            let events = drain(&mut rx);
            assert!(
                matches!(events.as_slice(), [AgentEvent::WorldLeaseLost { .. }]),
                "{obs:?}: {events:?}"
            );
        }
        tokio::task::yield_now().await;
        let mut lines = Vec::new();
        while let Ok(l) = seen.try_recv() {
            lines.push(l);
        }
        assert_eq!(
            lines,
            ["release w1", "acquire w1", "release w1", "acquire w1"],
            "a loss is not given back again"
        );
    }

    /// Viewing while the acquire is still out: the task's queued retry is
    /// cancelled, and a verdict that lands `Mine` anyway is given back at once
    /// and never read as ours, whether the session still runs or ended
    /// meanwhile. Nothing of it is a lost lease.
    #[tokio::test(start_paused = true)]
    async fn viewing_while_the_acquire_is_out_ends_with_no_lease() {
        let (tx, mut rx) = mpsc::channel(8);
        let now = Instant::now();
        for stop_first in [false, true] {
            let (lease, mut seen) = LeaseHandle::probe();
            let mut s = slots(vec![world("w1", "valheim")]);
            s.get_mut("w1").unwrap().lease = LeaseObs::Free;
            on_game_started(&mut s, "w1", now, &tx);
            let slot = s.get_mut("w1").unwrap();
            on_claim_world(slot, WorldRole::Host, &tx, Some(&lease));
            on_claim_world(slot, WorldRole::View, &tx, Some(&lease));
            assert!(!slot.lease_requested, "{stop_first}");
            slot.has_pending = true;
            assert!(!may_request_lease(slot), "{stop_first}");
            slot.has_pending = false;
            if stop_first {
                on_game_stopped(slot);
                assert_eq!(
                    on_reconciled(slot, now, &tx, Some(&lease)),
                    Followup::Nothing
                );
                assert!(slot.session.is_none());
                assert_eq!(slot.role, WorldRole::Host);
            }
            drain(&mut rx);
            // The acquire that was already on the wire.
            on_lease(
                slot,
                LeaseObs::Mine,
                Some("me".into()),
                true,
                &tx,
                Some(&lease),
            );
            assert_ne!(slot.lease, LeaseObs::Mine, "{stop_first}: read as ours");
            // The release's answer.
            on_lease(slot, LeaseObs::Free, None, false, &tx, Some(&lease));
            assert_eq!(slot.lease, LeaseObs::Free, "{stop_first}");
            assert!(!slot.release_requested, "{stop_first}");
            assert!(drain(&mut rx).is_empty(), "{stop_first}: told as a loss");
            tokio::task::yield_now().await;
            let mut lines = Vec::new();
            while let Ok(l) = seen.try_recv() {
                lines.push(l);
            }
            assert_eq!(
                lines,
                ["acquire w1", "cancel w1", "release w1"],
                "{stop_first}"
            );
        }
    }

    /// The relaunch asked about the other world while this one's side copy
    /// landed, and the user chose it: the landing world opens as "not
    /// playing", with no second prompt and no clock.
    #[tokio::test(start_paused = true)]
    async fn a_landing_world_is_not_asked_about_once_a_sibling_was_chosen() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut s = slots(vec![world("w1", "valheim"), world("w2", "valheim")]);
        s.get_mut("w1").unwrap().lease = LeaseObs::Other;
        s.get_mut("w2").unwrap().lease = LeaseObs::Free;
        let now = Instant::now();
        on_game_started(&mut s, "w1", now, &tx);
        drain(&mut rx);
        {
            let slot = s.get_mut("w1").unwrap();
            slot.role = WorldRole::View;
            on_claim(slot);
            slot.has_pending = true;
        }
        on_game_stopped_all(&mut s, "w1");
        assert_eq!(
            on_reconciled(s.get_mut("w1").unwrap(), now, &tx, None),
            Followup::SideCopy
        );
        assert_eq!(
            on_reconciled(s.get_mut("w2").unwrap(), now, &tx, None),
            Followup::Nothing
        );
        on_game_started(&mut s, "w2", now, &tx);
        let events = drain(&mut rx);
        let [AgentEvent::WorldClaimWanted { worlds, .. }] = events.as_slice() else {
            panic!("{events:?}");
        };
        assert_eq!(worlds.len(), 1);
        assert_eq!(worlds[0].save_id, "w2");
        on_claim_world(s.get_mut("w2").unwrap(), WorldRole::Host, &tx, None);
        drain(&mut rx);
        on_side_copied(&mut s, "w1", 2, now, &tx);
        assert!(drain(&mut rx).is_empty(), "asked again after the choice");
        let session = s["w1"].session.as_ref().unwrap();
        assert!(session.live() && session.dismissed && !session.prompted);
        assert!(session.auto_host_deadline.is_none());
    }

    /// An answer given after the game closed, while its session still owes
    /// the final flush, is that session's: the next launch asks again.
    #[tokio::test(start_paused = true)]
    async fn an_answer_to_a_stopped_session_does_not_decide_the_next_launch() {
        let (tx, mut rx) = mpsc::channel(8);
        let (lease, _seen) = LeaseHandle::probe();
        let mut s = slots(vec![world("w1", "valheim")]);
        s.get_mut("w1").unwrap().lease = LeaseObs::Mine;
        let now = Instant::now();
        on_game_started(&mut s, "w1", now, &tx);
        {
            let slot = s.get_mut("w1").unwrap();
            slot.has_pending = true;
            on_game_stopped(slot);
            assert_eq!(
                on_reconciled(slot, now, &tx, Some(&lease)),
                Followup::Nothing
            );
            assert!(slot.session.is_some(), "the flush is owed");
            on_claim_world(slot, WorldRole::Host, &tx, None);
            assert!(!slot.role_pinned, "pinned for the next launch");
            slot.has_pending = false;
            assert_eq!(
                on_reconciled(slot, now, &tx, Some(&lease)),
                Followup::Nothing
            );
            assert!(slot.session.is_none());
            on_lease(slot, LeaseObs::Free, None, false, &tx, Some(&lease));
        }
        drain(&mut rx);
        on_game_started(&mut s, "w1", now, &tx);
        assert_eq!(drain(&mut rx).len(), 1, "the next launch asks");
        let session = s["w1"].session.as_ref().unwrap();
        assert!(session.prompted && !session.claimed);
    }

    /// After a service restart nothing runs and nothing was claimed, and the
    /// last session's writes are still pending: the push takes the lease, goes
    /// up, and the lease is given back once it lands, as asked for, not lost. A
    /// role pinned before launch keeps its lease through the same pass.
    #[tokio::test(start_paused = true)]
    async fn a_push_with_no_session_gives_the_lease_back_once_it_lands() {
        let (tx, mut rx) = mpsc::channel(8);
        let (lease, mut seen) = LeaseHandle::probe();
        let now = Instant::now();
        let mut s = slots(vec![world("w1", "valheim")]);
        let slot = s.get_mut("w1").unwrap();
        assert_eq!(slot.lease, LeaseObs::Unknown);
        assert!(slot.session.is_none() && !slot.role_pinned);
        slot.has_pending = true;

        // The reducer holds for the lease; the shell asks (`request_lease`).
        assert!(may_request_lease(slot));
        request_acquire(slot, &lease);
        on_lease(slot, LeaseObs::Free, None, false, &tx, Some(&lease));
        assert_eq!(
            on_reconciled(slot, now, &tx, Some(&lease)),
            Followup::Nothing
        );
        on_lease(
            slot,
            LeaseObs::Mine,
            Some("me".into()),
            true,
            &tx,
            Some(&lease),
        );
        assert_eq!(slot.lease, LeaseObs::Mine);
        // Held while the push is still to go, and while it streams.
        on_reconciled(slot, now, &tx, Some(&lease));
        slot.has_pending = false;
        slot.in_flight = Some(kernel::Op::Backup);
        on_reconciled(slot, now, &tx, Some(&lease));
        assert!(!slot.release_requested, "released under the push");

        // The push landed: nothing pending, nothing in flight.
        slot.in_flight = None;
        on_reconciled(slot, now, &tx, Some(&lease));
        assert!(slot.release_requested);
        // Asked once, not every tick until the answer.
        on_reconciled(slot, now, &tx, Some(&lease));
        on_lease(slot, LeaseObs::Free, None, false, &tx, Some(&lease));
        assert_eq!(slot.lease, LeaseObs::Free);
        assert!(!slot.release_requested);
        assert!(
            drain(&mut rx).is_empty(),
            "the release was told as a lost lease"
        );
        tokio::task::yield_now().await;
        let mut lines = Vec::new();
        while let Ok(l) = seen.try_recv() {
            lines.push(l);
        }
        assert_eq!(lines, ["acquire w1", "release w1"]);

        // A role pinned for the next launch is not a push's lease.
        on_claim_world(slot, WorldRole::Host, &tx, Some(&lease));
        assert!(slot.role_pinned);
        on_lease(
            slot,
            LeaseObs::Mine,
            Some("me".into()),
            true,
            &tx,
            Some(&lease),
        );
        on_reconciled(slot, now, &tx, Some(&lease));
        assert!(!slot.release_requested, "gave back a pinned claim");
        tokio::task::yield_now().await;
        assert_eq!(seen.try_recv().unwrap(), "acquire w1");
        assert!(seen.try_recv().is_err());
    }

    #[tokio::test]
    async fn the_side_copy_moves_only_the_worlds_files() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("save");
        std::fs::create_dir_all(folder.join("worlds_local")).unwrap();
        std::fs::create_dir_all(folder.join("characters_local")).unwrap();
        std::fs::write(folder.join("worlds_local/Alpha.db"), b"world").unwrap();
        std::fs::write(folder.join("worlds_local/Beta.db"), b"other world").unwrap();
        std::fs::write(folder.join("characters_local/me.fch"), b"me").unwrap();
        let mut save = world("w1", "valheim");
        save.local_path = folder.clone();
        save.include = vec!["worlds_local/Alpha.db".into()];
        // A member's row, as `set_shared` writes it: the share's list is the
        // world the side copy takes (HRD-D-0019).
        if let Some(shared) = save.shared.as_mut() {
            shared.include = save.include.clone();
        }
        let dir = side_copy_dir(
            &tmp.path().join("conflicts"),
            "w1",
            OffsetDateTime::UNIX_EPOCH,
        );
        let moved = move_world_aside(&save, &dir).await.unwrap();
        assert_eq!(moved, 1);
        assert_eq!(
            std::fs::read(dir.join("worlds_local/Alpha.db")).unwrap(),
            b"world"
        );
        assert!(!folder.join("worlds_local/Alpha.db").exists());
        assert!(folder.join("worlds_local/Beta.db").exists());
        assert!(folder.join("characters_local/me.fch").exists());
    }

    /// An owner's share of one world out of a folder holding more (HRD-D-0019).
    fn owned_world(folder: &Path) -> WatchedSave {
        let mut save = world("w1", "valheim");
        save.local_path = folder.to_path_buf();
        if let Some(shared) = save.shared.as_mut() {
            shared.include = vec!["worlds_local/Alpha.db".into()];
            shared.caller_owns = true;
        }
        save
    }

    fn owner_folder(tmp: &Path) -> PathBuf {
        let folder = tmp.join("save");
        std::fs::create_dir_all(folder.join("worlds_local")).unwrap();
        std::fs::create_dir_all(folder.join("characters_local")).unwrap();
        std::fs::write(folder.join("worlds_local/Alpha.db"), b"world").unwrap();
        std::fs::write(folder.join("worlds_local/Beta.db"), b"other world").unwrap();
        std::fs::write(folder.join("characters_local/me.fch"), b"me").unwrap();
        folder
    }

    /// L-A: an owner's side copy that finds no file of the world on disk
    /// lets the pull through, and the owner's pending writes outside the
    /// world (a character) are re-checked once it lands, not dropped.
    #[tokio::test]
    async fn a_side_copy_with_no_world_on_disk_keeps_the_owners_edits_for_after_the_pull() {
        let (tx, _rx) = mpsc::channel(8);
        let tmp = tempfile::tempdir().unwrap();
        let folder = owner_folder(tmp.path());
        std::fs::remove_file(folder.join("worlds_local/Alpha.db")).unwrap();
        let now = Instant::now();
        let mut s = slots(vec![owned_world(&folder)]);
        {
            let slot = s.get_mut("w1").unwrap();
            slot.has_pending = true;
            let mut session = WorldSession::new(now);
            session.stopped = true;
            session.side_copy_started = true;
            slot.session = Some(session);
        }
        on_side_copied(&mut s, "w1", 0, now, &tx);
        let slot = &s["w1"];
        assert!(slot.session.is_none());
        assert!(!slot.has_pending && slot.pull_pending);
        assert!(
            slot.recheck_pending_after_pull,
            "the character is re-checked after the pull"
        );
    }

    /// The owner's walk takes the whole folder; its side copy still takes
    /// the world alone.
    #[tokio::test]
    async fn the_owners_side_copy_moves_only_the_world() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = owner_folder(tmp.path());
        let save = owned_world(&folder);
        assert!(save.include.is_empty() && save.owns_whole_folder());
        let dir = side_copy_dir(
            &tmp.path().join("conflicts"),
            "w1",
            OffsetDateTime::UNIX_EPOCH,
        );
        assert_eq!(move_world_aside(&save, &dir).await.unwrap(), 1);
        assert!(dir.join("worlds_local/Alpha.db").exists());
        assert!(!folder.join("worlds_local/Alpha.db").exists());
        assert!(folder.join("worlds_local/Beta.db").exists());
        assert!(folder.join("characters_local/me.fch").exists());
    }

    /// An owner who viewed the world and changed only a character: nothing is
    /// set aside, the change stays pending, and it goes up without the lease.
    #[tokio::test(start_paused = true)]
    async fn an_owner_viewers_character_change_is_not_set_aside_and_backs_up() {
        let (tx, mut rx) = mpsc::channel(8);
        let tmp = tempfile::tempdir().unwrap();
        let folder = owner_folder(tmp.path());
        let now = Instant::now();
        let mut s = slots(vec![owned_world(&folder)]);
        {
            let slot = s.get_mut("w1").unwrap();
            slot.lease = LeaseObs::Other;
            crate::agent::test_sync_now(slot);
        }
        on_game_started(&mut s, "w1", now, &tx);
        drain(&mut rx);
        let slot = s.get_mut("w1").unwrap();
        slot.role = WorldRole::View;
        std::fs::write(folder.join("characters_local/me.fch"), b"me, levelled up").unwrap();
        slot.has_pending = true;
        on_game_stopped(slot);
        assert_eq!(on_reconciled(slot, now, &tx, None), Followup::Nothing);
        assert!(slot.session.is_none());
        assert!(slot.has_pending);
        assert!(folder.join("worlds_local/Alpha.db").exists());
        let decisions = crate::agent::test_decisions(slot);
        assert!(
            decisions.contains(&kernel::Decision::Act(kernel::Action::Backup)),
            "{decisions:?}"
        );
    }

    /// The owner changed the world and a character under somebody else's
    /// lease: the world is set aside, the character stays, and it is pending
    /// again once the pull that refills the world has landed.
    #[tokio::test(start_paused = true)]
    async fn an_owners_world_and_character_change_sets_aside_only_the_world() {
        let (tx, mut rx) = mpsc::channel(8);
        let tmp = tempfile::tempdir().unwrap();
        let folder = owner_folder(tmp.path());
        let now = Instant::now();
        let mut s = slots(vec![owned_world(&folder)]);
        {
            let slot = s.get_mut("w1").unwrap();
            slot.lease = LeaseObs::Other;
            crate::agent::test_sync_now(slot);
        }
        on_game_started(&mut s, "w1", now, &tx);
        drain(&mut rx);
        {
            let slot = s.get_mut("w1").unwrap();
            std::fs::write(folder.join("worlds_local/Alpha.db"), b"world, changed").unwrap();
            std::fs::write(folder.join("characters_local/me.fch"), b"me, levelled up").unwrap();
            slot.has_pending = true;
            on_game_stopped(slot);
            assert_eq!(on_reconciled(slot, now, &tx, None), Followup::SideCopy);
        }
        let dir = side_copy_dir(
            &tmp.path().join("conflicts"),
            "w1",
            OffsetDateTime::UNIX_EPOCH,
        );
        let moved = move_world_aside(&s["w1"].save, &dir).await.unwrap();
        assert_eq!(moved, 1);
        on_side_copied(&mut s, "w1", moved, now, &tx);
        let slot = s.get_mut("w1").unwrap();
        assert!(
            !slot.has_pending,
            "nothing vetoes the pull that refills the world"
        );
        assert!(slot.pull_pending);
        assert!(slot.recheck_pending_after_pull);
        assert_eq!(
            std::fs::read(folder.join("characters_local/me.fch")).unwrap(),
            b"me, levelled up"
        );
        assert_eq!(
            std::fs::read(dir.join("worlds_local/Alpha.db")).unwrap(),
            b"world, changed"
        );

        // The pull lands with the head's world; the character is still newer.
        std::fs::write(folder.join("worlds_local/Alpha.db"), b"world").unwrap();
        crate::agent::after_pull_landed(slot, None);
        assert!(slot.has_pending, "the character stays pending");
        assert!(!slot.recheck_pending_after_pull);
    }

    /// A write to a character during a session on the owner's world is not
    /// evidence of playing it: no lease is asked for. A write to the world is,
    /// and so is a hit that names only the folder, or no path at all. Every hit
    /// goes in the shape the watcher sends it (`agent::fs_hit`).
    #[tokio::test(start_paused = true)]
    async fn a_write_outside_the_world_claims_nothing() {
        use notify_debouncer_mini::{DebouncedEvent, DebouncedEventKind};
        let (tx, mut rx) = mpsc::channel(8);
        let (lease, mut seen) = LeaseHandle::probe();
        let tmp = tempfile::tempdir().unwrap();
        let folder = owner_folder(tmp.path());
        let hit = |paths: &[PathBuf]| {
            let events: Vec<DebouncedEvent> = paths
                .iter()
                .map(|p| DebouncedEvent {
                    path: p.clone(),
                    kind: DebouncedEventKind::Any,
                })
                .collect();
            crate::agent::fs_hit(&folder, None, &events)
                .expect("a hit under the folder")
                .paths
        };
        let session = |rx: &mut mpsc::Receiver<AgentEvent>| {
            let mut s = slots(vec![owned_world(&folder)]);
            s.get_mut("w1").unwrap().lease = LeaseObs::Free;
            on_game_started(&mut s, "w1", Instant::now(), &tx);
            drain(rx);
            s
        };
        assert!(crate::agent::fs_hit(&folder, None, &[]).is_none());

        let mut s = session(&mut rx);
        let slot = s.get_mut("w1").unwrap();
        let character = folder.join("characters_local/me.fch");
        let beta = folder.join("worlds_local/Beta.db");
        on_write_at(
            slot,
            &hit(std::slice::from_ref(&character)),
            &tx,
            Some(&lease),
        );
        on_write_at(slot, &hit(std::slice::from_ref(&beta)), &tx, Some(&lease));
        on_write_at(slot, &hit(&[character, beta.clone()]), &tx, Some(&lease));
        tokio::task::yield_now().await;
        assert!(
            seen.try_recv().is_err(),
            "a write outside the world asked for the lease"
        );
        assert!(!slot.session.as_ref().unwrap().claimed);
        assert!(drain(&mut rx).is_empty());

        // One world file in the batch is enough.
        on_write_at(
            slot,
            &hit(&[beta, folder.join("worlds_local/Alpha.db")]),
            &tx,
            Some(&lease),
        );
        tokio::task::yield_now().await;
        assert_eq!(seen.try_recv().unwrap(), "acquire w1");
        assert!(slot.session.as_ref().unwrap().claimed);

        // A hit on the folder alone, an event with no path, and a directory
        // the world reaches into cannot rule the world out: each claims.
        for paths in [
            vec![folder.clone()],
            vec![PathBuf::new()],
            vec![folder.join("worlds_local")],
        ] {
            let mut s = session(&mut rx);
            let slot = s.get_mut("w1").unwrap();
            on_write_at(slot, &hit(&paths), &tx, Some(&lease));
            tokio::task::yield_now().await;
            assert_eq!(seen.try_recv().unwrap(), "acquire w1", "{paths:?}");
            assert!(slot.session.as_ref().unwrap().claimed, "{paths:?}");
        }
    }

    /// Behind the head with only the owner's files outside the world pending:
    /// nothing is set aside and the pull is not held back for them. A write to
    /// the world behind the head still is set aside.
    #[tokio::test(start_paused = true)]
    async fn behind_the_head_the_owners_other_writes_are_not_set_aside() {
        let (tx, _rx) = mpsc::channel(8);
        let tmp = tempfile::tempdir().unwrap();
        let folder = owner_folder(tmp.path());
        let now = Instant::now();
        let mut s = slots(vec![owned_world(&folder)]);
        let slot = s.get_mut("w1").unwrap();
        crate::agent::test_sync_now(slot);
        std::fs::write(folder.join("characters_local/me.fch"), b"me, levelled up").unwrap();
        slot.has_pending = true;
        on_stale(slot, 3);
        assert!(slot.pull_pending);
        assert_eq!(on_reconciled(slot, now, &tx, None), Followup::Nothing);
        assert!(slot.session.is_none());

        std::fs::write(folder.join("worlds_local/Alpha.db"), b"world, changed").unwrap();
        assert_eq!(on_reconciled(slot, now, &tx, None), Followup::SideCopy);
    }

    /// A Valheim 1.0 world is a folder whose files change names on every save
    /// (`_main.<N>.*`, `*.chunk`): a write, or the deletion of the generation
    /// before, anywhere inside the shared world's folder claims it, and the
    /// same inside another world's folder claims nothing.
    #[tokio::test(start_paused = true)]
    async fn a_write_inside_a_1_0_world_folder_claims_only_that_world() {
        use notify_debouncer_mini::{DebouncedEvent, DebouncedEventKind};
        let (tx, mut rx) = mpsc::channel(8);
        let (lease, mut seen) = LeaseHandle::probe();
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("save");
        for f in [
            "worlds_local/Alpha/_main.5.db2",
            "worlds_local/Alpha/0_0.chunk",
            "worlds_local/Gamma/_main.2.db2",
            "characters_local/me.fch",
        ] {
            let path = folder.join(f);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, f).unwrap();
        }
        let mut save = world("w1", "valheim");
        save.local_path = folder.clone();
        if let Some(shared) = save.shared.as_mut() {
            shared.include = crate::worldfiles::template("valheim", "Alpha")
                .unwrap()
                .unwrap();
            shared.caller_owns = true;
        }
        let hit = |paths: &[PathBuf]| {
            let events: Vec<DebouncedEvent> = paths
                .iter()
                .map(|p| DebouncedEvent {
                    path: p.clone(),
                    kind: DebouncedEventKind::Any,
                })
                .collect();
            crate::agent::fs_hit(&folder, None, &events)
                .expect("a hit under the folder")
                .paths
        };
        let session = |rx: &mut mpsc::Receiver<AgentEvent>| {
            let mut s = slots(vec![save.clone()]);
            s.get_mut("w1").unwrap().lease = LeaseObs::Free;
            on_game_started(&mut s, "w1", Instant::now(), &tx);
            drain(rx);
            s
        };

        // Another world's folder: a write, a generation gone, the folder itself.
        let mut s = session(&mut rx);
        let slot = s.get_mut("w1").unwrap();
        for path in [
            "worlds_local/Gamma/_main.2.db2",
            "worlds_local/Gamma/_main.1.db2",
            "worlds_local/Gamma",
            "characters_local/me.fch",
        ] {
            on_write_at(slot, &hit(&[folder.join(path)]), &tx, Some(&lease));
        }
        tokio::task::yield_now().await;
        assert!(
            seen.try_recv().is_err(),
            "a write in another world's folder asked for the lease"
        );
        assert!(!slot.session.as_ref().unwrap().claimed);
        assert!(drain(&mut rx).is_empty());

        // Inside the shared world's folder, each one claims.
        for path in [
            "worlds_local/Alpha/_main.5.db2",
            "worlds_local/Alpha/0_0.chunk",
            "worlds_local/Alpha/_main.4.db2",
            "worlds_local/Alpha",
        ] {
            let mut s = session(&mut rx);
            let slot = s.get_mut("w1").unwrap();
            on_write_at(slot, &hit(&[folder.join(path)]), &tx, Some(&lease));
            tokio::task::yield_now().await;
            assert_eq!(seen.try_recv().unwrap(), "acquire w1", "{path}");
            assert!(slot.session.as_ref().unwrap().claimed, "{path}");
        }
    }

    /// The side copy of a 1.0 world takes its folder's files, nested paths
    /// kept, and leaves another world's folder where it is.
    #[tokio::test]
    async fn the_side_copy_moves_a_1_0_world_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("save");
        for f in [
            "worlds_local/Alpha/_main.5.fwl2",
            "worlds_local/Alpha/0_0.chunk",
            "worlds_local/Gamma/_main.2.fwl2",
            "characters_local/me.fch",
        ] {
            let path = folder.join(f);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, f).unwrap();
        }
        let mut save = world("w1", "valheim");
        save.local_path = folder.clone();
        save.include = crate::worldfiles::template("valheim", "Alpha")
            .unwrap()
            .unwrap();
        if let Some(shared) = save.shared.as_mut() {
            shared.include = save.include.clone();
        }
        let dir = side_copy_dir(
            &tmp.path().join("conflicts"),
            "w1",
            OffsetDateTime::UNIX_EPOCH,
        );
        assert_eq!(move_world_aside(&save, &dir).await.unwrap(), 2);
        assert!(dir.join("worlds_local/Alpha/_main.5.fwl2").exists());
        assert!(dir.join("worlds_local/Alpha/0_0.chunk").exists());
        assert!(!folder.join("worlds_local/Alpha/0_0.chunk").exists());
        assert!(folder.join("worlds_local/Gamma/_main.2.fwl2").exists());
        assert!(folder.join("characters_local/me.fch").exists());
    }

    /// Finding 3 of the third end-to-end run: with no session, the owner
    /// changed the world and a character while a member hosts. The push holds
    /// for the lease, so the world is set aside; the pull brings the head's
    /// world back and the character goes up without the lease, with no second
    /// copy after it.
    #[tokio::test(start_paused = true)]
    async fn an_owners_world_edit_under_a_members_lease_is_set_aside_with_no_session() {
        use kernel::reconcile::HOLD_LEASE_OTHER;
        let (tx, _rx) = mpsc::channel(8);
        let tmp = tempfile::tempdir().unwrap();
        let folder = owner_folder(tmp.path());
        let now = Instant::now();
        let mut s = slots(vec![owned_world(&folder)]);
        {
            let slot = s.get_mut("w1").unwrap();
            slot.lease = LeaseObs::Other;
            crate::agent::test_sync_now(slot);
            std::fs::write(folder.join("worlds_local/Alpha.db"), b"world, changed").unwrap();
            std::fs::write(folder.join("characters_local/me.fch"), b"me, levelled up").unwrap();
            slot.has_pending = true;
            assert_eq!(
                crate::agent::test_decisions(slot),
                vec![kernel::Decision::Hold {
                    reason: HOLD_LEASE_OTHER
                }]
            );
            assert!(slot.session.is_none() && slot.stale_base.is_none());
            assert_eq!(on_reconciled(slot, now, &tx, None), Followup::SideCopy);
            assert_eq!(
                on_reconciled(slot, now, &tx, None),
                Followup::Nothing,
                "asked once"
            );
        }
        let dir = side_copy_dir(
            &tmp.path().join("conflicts"),
            "w1",
            OffsetDateTime::UNIX_EPOCH,
        );
        let moved = move_world_aside(&s["w1"].save, &dir).await.unwrap();
        assert_eq!(moved, 1);
        on_side_copied(&mut s, "w1", moved, now, &tx);
        let slot = s.get_mut("w1").unwrap();
        assert!(slot.session.is_none());
        assert!(!slot.has_pending && slot.pull_pending);
        assert_eq!(
            std::fs::read(dir.join("worlds_local/Alpha.db")).unwrap(),
            b"world, changed"
        );

        // The pull lands with the head's world; the character is still newer.
        // The merge reports the world it left, equal to the head's
        // (`disk_world_hash`), and the reducer adopts it; the whole folder
        // diverged, so its fingerprint stays.
        std::fs::write(folder.join("worlds_local/Alpha.db"), b"world").unwrap();
        slot.pull_pending = false;
        slot.known_version = Some(3);
        crate::agent::test_sync_world_now(slot);
        crate::agent::after_pull_landed(slot, None);
        assert!(slot.has_pending, "the character stays pending");
        let decisions = crate::agent::test_decisions(slot);
        assert!(
            decisions.contains(&kernel::Decision::Act(kernel::Action::Backup)),
            "{decisions:?}"
        );
        assert_eq!(on_reconciled(slot, now, &tx, None), Followup::Nothing);
    }

    fn lines(seen: &mut mpsc::UnboundedReceiver<String>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(l) = seen.try_recv() {
            out.push(l);
        }
        out
    }

    /// Adopted with no version and claimed at once: the server refuses the
    /// acquire as stale. The adopt's touch does not veto the pull, the head
    /// comes down, the claim asks again with it, and the world is hosted here.
    #[tokio::test(start_paused = true)]
    async fn an_adopted_world_claimed_behind_the_head_pulls_then_hosts() {
        let (tx, mut rx) = mpsc::channel(8);
        let (lease, mut seen) = LeaseHandle::probe();
        let now = Instant::now();
        let mut save = world("w1", "valheim");
        save.known_version = None;
        let mut s = slots(vec![save]);
        let slot = s.get_mut("w1").unwrap();
        on_seated(slot);
        assert!(slot.last_restore_at.is_some(), "the adopt's touch is ours");

        on_claim_world(slot, WorldRole::Host, &tx, Some(&lease));
        on_lease(slot, LeaseObs::Free, None, true, &tx, Some(&lease));
        on_stale(slot, 0);
        assert_eq!(slot.stale_base, Some(0));
        assert!(slot.pull_pending, "the head comes down");
        assert!(!slot.lease_requested);
        assert_eq!(
            on_reconciled(slot, now, &tx, Some(&lease)),
            Followup::Nothing
        );

        // The pull landed.
        slot.pull_pending = false;
        slot.known_version = Some(4);
        assert_eq!(
            on_reconciled(slot, now, &tx, Some(&lease)),
            Followup::Nothing
        );
        assert_eq!(slot.stale_base, None);
        assert!(slot.lease_requested, "asked again with the new head");

        on_lease(
            slot,
            LeaseObs::Mine,
            Some("me".into()),
            true,
            &tx,
            Some(&lease),
        );
        assert_eq!(
            on_reconciled(slot, now, &tx, Some(&lease)),
            Followup::Nothing
        );
        assert_eq!(slot.lease, LeaseObs::Mine);
        assert!(!slot.release_requested, "a pinned claim keeps its lease");
        tokio::task::yield_now().await;
        assert_eq!(lines(&mut seen), ["acquire w1", "acquire w1"]);
        assert!(
            !drain(&mut rx)
                .iter()
                .any(|e| matches!(e, AgentEvent::WorldLeaseLost { .. })),
            "nothing was lost"
        );
    }

    /// Behind the head with writes that cannot go up: they are moved to a side
    /// copy, never pulled over, the head comes down, and the claim hosts with
    /// it. The bytes survive in the copy.
    #[tokio::test(start_paused = true)]
    async fn behind_the_head_with_local_writes_sets_them_aside_then_pulls_and_hosts() {
        let (tx, mut rx) = mpsc::channel(8);
        let (lease, mut seen) = LeaseHandle::probe();
        let now = Instant::now();
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("save");
        std::fs::create_dir_all(folder.join("worlds_local")).unwrap();
        std::fs::write(folder.join("worlds_local/Alpha.db"), b"mine").unwrap();
        let mut save = world("w1", "valheim");
        save.local_path = folder.clone();
        save.include = vec!["worlds_local/Alpha.db".into()];
        // A member's row, as `set_shared` writes it: the share's list is the
        // world the side copy takes (HRD-D-0019).
        if let Some(shared) = save.shared.as_mut() {
            shared.include = save.include.clone();
        }
        let mut s = slots(vec![save]);
        {
            let slot = s.get_mut("w1").unwrap();
            slot.has_pending = true;
            on_claim_world(slot, WorldRole::Host, &tx, Some(&lease));
            on_lease(slot, LeaseObs::Free, None, true, &tx, Some(&lease));
            on_stale(slot, 3);
            assert!(!slot.pull_pending, "not over the writes");
            assert!(
                !may_request_lease(slot),
                "the answer is known until the pull"
            );
            assert_eq!(
                on_reconciled(slot, now, &tx, Some(&lease)),
                Followup::SideCopy
            );
            assert!(
                hit_is_side_copy(slot, now),
                "the copy's renames are not writes"
            );
            assert_eq!(
                on_reconciled(slot, now, &tx, Some(&lease)),
                Followup::Nothing,
                "asked once"
            );
            assert!(slot.session.is_some(), "only the landing ends it");
        }
        let dir = side_copy_dir(
            &tmp.path().join("conflicts"),
            "w1",
            OffsetDateTime::UNIX_EPOCH,
        );
        let moved = move_world_aside(&s["w1"].save, &dir).await.unwrap();
        on_side_copied(&mut s, "w1", moved, now, &tx);
        assert_eq!(
            std::fs::read(dir.join("worlds_local/Alpha.db")).unwrap(),
            b"mine"
        );
        assert!(!folder.join("worlds_local/Alpha.db").exists());

        let slot = s.get_mut("w1").unwrap();
        assert!(slot.session.is_none());
        assert!(!slot.has_pending);
        assert!(
            slot.pull_pending,
            "the head comes down over the emptied folder"
        );
        assert_eq!(slot.stale_base, Some(3), "behind until the pull lands");
        assert_eq!(
            on_reconciled(slot, now, &tx, Some(&lease)),
            Followup::Nothing
        );
        assert!(!slot.lease_requested);

        // The pull landed.
        slot.pull_pending = false;
        slot.known_version = Some(4);
        assert_eq!(
            on_reconciled(slot, now, &tx, Some(&lease)),
            Followup::Nothing
        );
        assert!(slot.lease_requested, "asked again with the new head");
        on_lease(
            slot,
            LeaseObs::Mine,
            Some("me".into()),
            true,
            &tx,
            Some(&lease),
        );
        assert_eq!(slot.lease, LeaseObs::Mine);
        assert_eq!(slot.stale_base, None);
        tokio::task::yield_now().await;
        assert_eq!(lines(&mut seen), ["acquire w1", "acquire w1"]);
        assert!(!drain(&mut rx)
            .iter()
            .any(|e| matches!(e, AgentEvent::WorldLeaseLost { .. })));
    }

    /// Asked to host again, or forced, while the first acquire was refused as
    /// stale: nothing is left waiting on a verdict. The game writes and
    /// stops, the stopped session ends, the writes go aside, the head comes
    /// down, and the next launch's write asks with the new head and is
    /// answered.
    #[tokio::test(start_paused = true)]
    async fn asked_again_behind_the_head_leaves_no_acquire_waiting() {
        for verb in ["host", "force"] {
            let (tx, mut rx) = mpsc::channel(32);
            let (lease, mut seen) = LeaseHandle::probe();
            let now = Instant::now();
            let tmp = tempfile::tempdir().unwrap();
            let folder = tmp.path().join("save");
            std::fs::create_dir_all(folder.join("worlds_local")).unwrap();
            std::fs::write(folder.join("worlds_local/Alpha.db"), b"mine").unwrap();
            let mut save = world("w1", "valheim");
            save.local_path = folder.clone();
            save.include = vec!["worlds_local/Alpha.db".into()];
            let mut s = slots(vec![save]);
            s.get_mut("w1").unwrap().lease = LeaseObs::Free;
            on_game_started(&mut s, "w1", now, &tx);
            let ask = |slot: &mut SaveSlot| match verb {
                "host" => on_claim_world(slot, WorldRole::Host, &tx, Some(&lease)),
                _ => on_force_world(slot, Some(&lease)),
            };
            {
                let slot = s.get_mut("w1").unwrap();
                ask(slot);
                assert!(slot.lease_requested, "{verb}");
                // The engine's `LeaseStale` arm.
                on_lease(slot, LeaseObs::Free, None, true, &tx, Some(&lease));
                on_stale(slot, 3);
                ask(slot);
                assert!(!slot.lease_requested, "{verb}: no verdict is coming");

                // The game writes, and the grace has passed by the time it
                // stops.
                slot.has_pending = true;
                slot.last_fs_event_at = Some(
                    OffsetDateTime::now_utc() - time::Duration::seconds(RECENT_SAVE_GRACE_SECS + 1),
                );
                on_write(slot, &tx, Some(&lease));
                on_game_stopped(slot);
                assert_eq!(
                    on_reconciled(slot, now, &tx, Some(&lease)),
                    Followup::Nothing
                );
                assert!(slot.session.is_none(), "{verb}: the stopped session ends");
                assert_eq!(
                    on_reconciled(slot, now, &tx, Some(&lease)),
                    Followup::SideCopy,
                    "{verb}"
                );
            }
            let dir = side_copy_dir(
                &tmp.path().join("conflicts"),
                "w1",
                OffsetDateTime::UNIX_EPOCH,
            );
            let moved = move_world_aside(&s["w1"].save, &dir).await.unwrap();
            on_side_copied(&mut s, "w1", moved, now, &tx);
            assert_eq!(
                std::fs::read(dir.join("worlds_local/Alpha.db")).unwrap(),
                b"mine"
            );
            {
                let slot = s.get_mut("w1").unwrap();
                assert!(slot.pull_pending && !slot.has_pending, "{verb}");
                // The pull landed.
                slot.pull_pending = false;
                slot.known_version = Some(4);
                assert_eq!(
                    on_reconciled(slot, now, &tx, Some(&lease)),
                    Followup::Nothing
                );
                assert_eq!(slot.stale_base, None, "{verb}");
            }

            on_game_started(&mut s, "w1", now, &tx);
            let slot = s.get_mut("w1").unwrap();
            on_write(slot, &tx, Some(&lease));
            assert!(
                slot.lease_requested,
                "{verb}: asked again with the new head"
            );
            on_lease(
                slot,
                LeaseObs::Mine,
                Some("me".into()),
                true,
                &tx,
                Some(&lease),
            );
            assert!(!slot.lease_requested, "{verb}: answered");
            assert_eq!(slot.lease, LeaseObs::Mine, "{verb}");
            tokio::task::yield_now().await;
            let expected: &[&str] = match verb {
                "host" => &["acquire w1", "acquire w1"],
                _ => &["force w1", "acquire w1", "force w1", "acquire w1"],
            };
            assert_eq!(lines(&mut seen), expected, "{verb}");
            assert!(
                !drain(&mut rx)
                    .iter()
                    .any(|e| matches!(e, AgentEvent::WorldLeaseLost { .. })),
                "{verb}"
            );
        }
    }

    /// Behind the head with writes and no session, a folder written within
    /// the kernel's grace is not moved aside: a game the agent does not see
    /// may still have it open. Once the grace has passed, it is.
    #[tokio::test(start_paused = true)]
    async fn behind_the_head_a_recent_write_is_not_set_aside_until_the_grace_passes() {
        let (tx, _rx) = mpsc::channel(8);
        let now = Instant::now();
        let mut s = slots(vec![world("w1", "valheim")]);
        let slot = s.get_mut("w1").unwrap();
        slot.has_pending = true;
        on_stale(slot, 3);
        slot.last_fs_event_at = Some(OffsetDateTime::now_utc() - time::Duration::seconds(10));
        assert_eq!(on_reconciled(slot, now, &tx, None), Followup::Nothing);
        assert!(slot.session.is_none(), "no side copy under a recent write");
        slot.last_fs_event_at =
            Some(OffsetDateTime::now_utc() - time::Duration::seconds(RECENT_SAVE_GRACE_SECS + 1));
        assert_eq!(on_reconciled(slot, now, &tx, None), Followup::SideCopy);
        assert!(slot.session.as_ref().is_some_and(|w| w.side_copy_started));
    }

    /// A failed side copy of writes behind the head is not retried every
    /// pass: they stay local-only, and nothing moves under a running game.
    #[tokio::test(start_paused = true)]
    async fn behind_the_head_the_side_copy_waits_for_the_game_and_is_not_retried() {
        let (tx, _rx) = mpsc::channel(8);
        let now = Instant::now();
        let mut s = slots(vec![world("w1", "valheim")]);
        {
            let slot = s.get_mut("w1").unwrap();
            slot.has_pending = true;
            slot.is_running = true;
            on_stale(slot, 3);
            assert_eq!(on_reconciled(slot, now, &tx, None), Followup::Nothing);
            slot.is_running = false;
            assert_eq!(on_reconciled(slot, now, &tx, None), Followup::SideCopy);
        }
        on_side_copy_failed(&mut s, "w1", now, &tx);
        let slot = s.get_mut("w1").unwrap();
        assert!(slot.local_only_pending);
        assert_eq!(on_reconciled(slot, now, &tx, None), Followup::Nothing);
    }

    /// Stopping the service while hosting: the lease task gives the lease back
    /// and the server's word says free. That is the stop's answer, not a loss,
    /// with a session running or not.
    #[tokio::test(start_paused = true)]
    async fn stopping_while_hosting_is_not_a_lost_lease() {
        let (tx, mut rx) = mpsc::channel(8);
        let now = Instant::now();
        for live in [false, true] {
            let mut s = slots(vec![world("w1", "valheim")]);
            s.get_mut("w1").unwrap().lease = LeaseObs::Mine;
            if live {
                on_game_started(&mut s, "w1", now, &tx);
            }
            drain(&mut rx);
            let slot = s.get_mut("w1").unwrap();
            on_stopping(slot);
            assert!(slot.release_requested, "live: {live}");
            on_lease(slot, LeaseObs::Free, None, false, &tx, None);
            assert_eq!(slot.lease, LeaseObs::Free);
            assert!(
                drain(&mut rx).is_empty(),
                "live: {live}: told as a lost lease"
            );
        }
    }
}
