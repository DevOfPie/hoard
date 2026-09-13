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
//! the fs event, the reconcile pass) and at the two answers; every decision is
//! here, over the slot, so it runs in a test with no process and no socket.
//! Nothing here restores mid-session: the kernel forbids it, and a shared
//! save is kept current while the game is closed by the level-triggered pull,
//! so launch needs no pull of its own.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use hoard_core::ipc::{AgentEvent, WorldChoice, WorldLease, WorldPrompt, WorldRole};
use hoard_core::kernel::fileclass::Scope;
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
        .filter(|s| s.save.shared && !s.save.track_only && s.save.game_slug == game_slug)
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
            .is_some_and(|w| w.prompted && w.live())
    });
    if already {
        return;
    }
    let mut worlds = Vec::with_capacity(ids.len());
    for id in &ids {
        let Some(slot) = slots.get_mut(id) else {
            continue;
        };
        let mut session = WorldSession::new(now);
        session.prompted = true;
        // A lease this machine still holds (an earlier session that never
        // released, a restart) is a role already taken, not one to ask about.
        session.claimed = slot.lease == LeaseObs::Mine;
        slot.session = Some(session);
        worlds.push(WorldChoice {
            save_id: id.clone(),
            label: slot.save.label.clone(),
            group_name: slot.save.group_name.clone().unwrap_or_default(),
            holder: slot.lease_holder.clone(),
            lease: lease_for_prompt(slot.lease),
        });
    }
    if ids.len() == 1 {
        if let Some(slot) = slots.get_mut(&ids[0]) {
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
    tracing::info!(game_slug = %game_slug, worlds = worlds.len(), "agent: asking which shared world");
    let _ = events_tx.try_send(AgentEvent::WorldClaimWanted { game_slug, worlds });
}

/// GameStopped: the clock stops; what is left (release, side copy) is the
/// reconcile pass's, which has the reducer's state to judge it by.
pub(crate) fn on_game_stopped(slot: &mut SaveSlot) {
    if let Some(session) = slot.session.as_mut() {
        session.stopped = true;
        session.auto_host_deadline = None;
    }
}

/// The user answered `ClaimWorld`. The engine already acquired for `Host`.
pub(crate) fn on_claim(slot: &mut SaveSlot) {
    if let Some(session) = slot.session.as_mut() {
        session.claimed = true;
        session.auto_host_deadline = None;
    }
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
            group_name: slot.save.group_name.clone().unwrap_or_default(),
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
    if !slot.save.shared {
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
            _ => {}
        },
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
        slot.lease_requested = true;
        lease.acquire(id.clone(), slot.known_version.unwrap_or(0));
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
    let Some(session) = slot.session.as_mut() else {
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
        maybe_auto_host(slot, now, events_tx, lease);
        return Followup::Nothing;
    }
    if maybe_release_after_stop(slot, events_tx, lease) {
        return Followup::Nothing;
    }
    let Some(session) = slot.session.as_mut() else {
        return Followup::Nothing;
    };
    if slot.lease == LeaseObs::Mine || slot.lease_requested {
        // Ours, or about to be: the release waits for the final flush.
        return Followup::Nothing;
    }
    let local_only = slot.role == WorldRole::View || slot.lease == LeaseObs::Other;
    if local_only && slot.has_pending {
        if session.side_copy_started {
            return Followup::Nothing;
        }
        session.side_copy_started = true;
        return Followup::SideCopy;
    }
    slot.session = None;
    Followup::Nothing
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
    if slot.has_pending || slot.in_flight.is_some() {
        return false;
    }
    let id = slot.save.save_id.clone();
    let session_secs = session.started_at.elapsed().as_secs();
    tracing::info!(save_id = %id, session_secs, "agent: session over and flushed; releasing the hosting lease");
    if let Some(lease) = lease {
        lease.release(id.clone());
    }
    let _ = events_tx.try_send(AgentEvent::WorldReleased {
        save_id: id,
        game_slug: slot.save.game_slug.clone(),
    });
    slot.session = None;
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

/// Moves the world's files (the save's include list, nothing else) into
/// `dir`, so the folder holds nothing newer than the head and the next pull
/// puts the head back without an mtime contest. Rename first, copy and
/// remove across filesystems. Returns how many moved.
pub(crate) async fn move_world_aside(save: &WatchedSave, dir: &Path) -> anyhow::Result<u64> {
    let shields = crate::savefilter::shields_for_slug(&save.game_slug);
    let files = crate::backup::walk_source(
        &save.local_path,
        Scope {
            shields: &shields,
            include: &save.include,
        },
    )?;
    let mut moved = 0;
    for f in files {
        let dest = dir.join(&f.relative_path);
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        if tokio::fs::rename(&f.absolute_path, &dest).await.is_err() {
            tokio::fs::copy(&f.absolute_path, &dest).await?;
            tokio::fs::remove_file(&f.absolute_path).await?;
        }
        moved += 1;
    }
    Ok(moved)
}

/// The side copy landed: the session's bytes are safe there, so the folder
/// may be pulled over. `has_pending` is cleared for that reason alone; the
/// reducer then owes the head a pull, which it runs once the folder is quiet.
pub(crate) fn on_side_copied(slot: &mut SaveSlot, moved: u64) {
    if moved > 0 {
        // The files are in the side copy, not in the folder: nothing local is
        // unversioned any more, and the head has to come back.
        slot.has_pending = false;
        slot.known_version = None;
        slot.pull_pending = true;
    }
    slot.session = None;
}

/// The side copy could not be made: the bytes stay where they are, pending,
/// and the session is over.
pub(crate) fn on_side_copy_failed(slot: &mut SaveSlot) {
    slot.session = None;
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
            track_only: false,
            shared: true,
            group_name: Some("friends".into()),
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
        plain.shared = false;
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
        let slot = s.get_mut("w1").unwrap();
        slot.role = WorldRole::View;
        slot.has_pending = true;
        on_game_stopped(slot);
        assert_eq!(on_reconciled(slot, now, &tx, None), Followup::SideCopy);
        // Asked once; the next pass waits for it to land.
        assert_eq!(on_reconciled(slot, now, &tx, None), Followup::Nothing);
        on_side_copied(slot, 2);
        assert!(!slot.has_pending);
        assert!(slot.pull_pending);
        assert_eq!(slot.known_version, None);
        assert!(slot.session.is_none());
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
}
