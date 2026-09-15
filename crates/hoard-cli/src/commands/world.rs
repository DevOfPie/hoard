//! `hoard world`: who hosts a shared world. The verbs are engine commands: the
//! service refuses one for a save that is not a shared world here, and
//! otherwise answers once the engine has taken it. `claim` and `force` then wait
//! a few seconds for the lease to settle; the rest, and a verdict that does not
//! arrive in time, show in `hoard sync logs` and `hoard world lease`.
//! The tables (`hoard saves`, `hoard status`) read who hosts from the engine
//! instead, see [`Hosts`].

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use clap::Subcommand;
use serde::Serialize;
use time::format_description::well_known::Rfc3339;

use hoard_core::ipc::{DaemonStatus, Payload, Request, WorldLease, WorldRole};
use hoard_core::wire::Lease;
use hoardd::client::Client;

use super::link;
use crate::output;

#[derive(Subcommand)]
pub enum WorldCommand {
    /// Take a role on a shared world: host it (acquire the lease, your changes
    /// upload), which is the default, or only view it with `--view` (pull,
    /// never push). Hosting waits up to 10 seconds for the server's answer.
    Claim {
        /// Save id (UUID), see `hoard saves`
        save_id: String,
        /// View the world instead of hosting it: your writes to it stay on this
        /// machine
        #[arg(long)]
        view: bool,
    },
    /// Give the hosting lease back
    Release {
        /// Save id (UUID), see `hoard saves`
        save_id: String,
    },
    /// Take the lease off its holder and host. Refused once the holder has
    /// pushed under it: only an idle lease can be taken. Waits up to 10 seconds
    /// for the server's answer.
    Force {
        /// Save id (UUID), see `hoard saves`
        save_id: String,
    },
    /// Not playing this world: answers the service's "host or view?" without
    /// taking a role
    Dismiss {
        /// Save id (UUID), see `hoard saves`
        save_id: String,
    },
    /// Who hosts a shared world right now
    Lease {
        /// Save id (UUID), see `hoard saves`
        save_id: String,
    },
}

/// The lease as agents and scripts see it.
#[derive(Serialize)]
pub struct LeaseOut {
    pub save_id: String,
    /// Null when nobody holds the lease.
    pub lease: Option<LeaseDetail>,
}

/// A held lease.
#[derive(Serialize)]
pub struct LeaseDetail {
    pub holder: String,
    /// The lease is held here: the engine's verdict when it watches the save,
    /// otherwise this machine's fingerprint against the one on the lease, for a
    /// lease this account holds.
    pub here: bool,
    /// RFC3339.
    pub acquired_at: String,
    /// RFC3339: the last heartbeat.
    pub renewed_at: String,
    pub base_version: i64,
    /// Heartbeats are arriving; a quiet lease is free to take.
    pub live: bool,
    /// The holder has pushed under it, so it can no longer be forced.
    pub pushed_since: bool,
}

/// What a world verb came to, as agents and scripts see it. A refusal is the
/// error envelope instead (`held`, `pushed`).
#[derive(Serialize)]
pub struct WorldOut {
    pub save_id: String,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// This machine holds the lease.
    Hosting,
    /// The service took the request and no verdict arrived within the wait.
    Pending,
    /// `claim --view`, `release` and `dismiss`: asked, with nothing to wait on.
    Viewing,
    Releasing,
    Dismissed,
}

/// How long `claim` and `force` wait for the verdict, and how often they look.
const VERDICT_WAIT: Duration = Duration::from_secs(10);
const POLL_EVERY: Duration = Duration::from_millis(500);

pub async fn run(cmd: WorldCommand) -> Result<()> {
    let mut client = link::require("world").await?;
    match cmd {
        WorldCommand::Claim { save_id, view } => {
            let role = if view {
                WorldRole::View
            } else {
                WorldRole::Host
            };
            link::ask(
                &mut client,
                Request::ClaimWorld {
                    save_id: save_id.clone(),
                    role,
                },
            )
            .await?;
            let outcome = match role {
                WorldRole::View => Outcome::Viewing,
                WorldRole::Host => {
                    let verdict = wait_for_verdict(&mut client, &save_id, Wait::default()).await;
                    outcome_of(&save_id, verdict, false)?
                }
            };
            emit(save_id, outcome, false)
        }
        WorldCommand::Release { save_id } => {
            link::ask(
                &mut client,
                Request::ReleaseWorld {
                    save_id: save_id.clone(),
                },
            )
            .await?;
            emit(save_id, Outcome::Releasing, false)
        }
        WorldCommand::Force { save_id } => {
            // Who holds it as the force goes out. The server keeps naming them
            // until the engine's call lands, which can be well behind.
            let forced = match lease(&mut client, &save_id).await {
                Ok(l) => {
                    let engine = match link::ask(&mut client, Request::Status).await {
                        Ok(Payload::Status(status)) => Hosts::from_status(&status).lease(&save_id),
                        _ => None,
                    };
                    match seen(
                        l.as_ref(),
                        engine,
                        &this_device(),
                        this_account().as_deref(),
                    ) {
                        Seen::Other { holder, .. } => Some(holder),
                        _ => None,
                    }
                }
                Err(_) => None,
            };
            link::ask(
                &mut client,
                Request::ForceWorld {
                    save_id: save_id.clone(),
                },
            )
            .await?;
            let verdict = wait_for_verdict(&mut client, &save_id, Wait::forcing(forced)).await;
            let outcome = outcome_of(&save_id, verdict, true)?;
            emit(save_id, outcome, true)
        }
        WorldCommand::Dismiss { save_id } => {
            link::ask(
                &mut client,
                Request::DismissWorld {
                    save_id: save_id.clone(),
                },
            )
            .await?;
            emit(save_id, Outcome::Dismissed, false)
        }
        WorldCommand::Lease { save_id } => {
            let lease = lease(&mut client, &save_id).await?;
            // Asked on the same connection; a service that cannot say leaves
            // `here` to the fingerprint.
            let engine = match link::ask(&mut client, Request::Status).await {
                Ok(Payload::Status(status)) => Hosts::from_status(&status).lease(&save_id),
                _ => None,
            };
            let me = this_account();
            let out = LeaseOut {
                save_id: save_id.clone(),
                lease: lease
                    .as_ref()
                    .map(|l| detail(l, engine, &this_device(), me.as_deref())),
            };
            output::emit(&out, |o| {
                let Some(l) = &o.lease else {
                    println!("{}: nobody is hosting", o.save_id);
                    return;
                };
                println!(
                    "holder:  {}{}\nsince:   {}\nrenewed: {}\nbase:    v{}\nlive:    {}\npushed:  {}",
                    l.holder,
                    if l.here { " (this machine)" } else { "" },
                    l.acquired_at,
                    l.renewed_at,
                    l.base_version,
                    yes_no(l.live),
                    yes_no(l.pushed_since),
                );
            })
        }
    }
}

fn emit(save_id: String, outcome: Outcome, force: bool) -> Result<()> {
    let out = WorldOut { save_id, outcome };
    output::emit(&out, |o| {
        println!("{}", human_line(&o.save_id, o.outcome, force));
    })
}

/// The one line a world verb prints.
fn human_line(save_id: &str, outcome: Outcome, force: bool) -> String {
    match outcome {
        Outcome::Hosting => format!("hosting {save_id}"),
        Outcome::Pending if force => {
            format!("taking the lease on {save_id}; the outcome shows in `hoard sync logs`")
        }
        Outcome::Pending => format!(
            "asked to host {save_id}; the outcome shows in `hoard sync logs` and `hoard world lease {save_id}`"
        ),
        Outcome::Viewing => format!(
            "asked to view {save_id}; the outcome shows in `hoard sync logs` and `hoard world lease {save_id}`"
        ),
        Outcome::Releasing => format!("releasing the lease on {save_id}"),
        Outcome::Dismissed => format!("dismissed {save_id}: no role this session"),
    }
}

/// The lease as one look at it saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seen {
    /// The service could not say.
    Unknown,
    /// Nobody holds it, or the holder's lease has gone quiet.
    Free,
    Mine,
    Other {
        holder: String,
        pushed: bool,
    },
}

/// Where a wait for the verdict ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Hosting,
    Held {
        holder: String,
        pushed: bool,
    },
    /// Nothing settled before the deadline.
    Undecided,
    /// Nothing settled, and the engine says the world is behind the head: the
    /// acquire was refused as stale and a pull runs first.
    Behind,
}

/// Reads the looks of one wait in order. `Mine` settles it at once. `Other`
/// settles it only when two looks in a row agree: right after a takeover the
/// server still names the old holder until the engine's call lands, and one
/// look cannot tell that from a refusal.
#[derive(Default)]
struct Wait {
    /// For `force`: the holder the force was sent against. The engine sends it
    /// behind whatever it already had on the wire, so the server can name them
    /// for seconds; they settle nothing before the deadline.
    forced: Option<String>,
    other: Option<(String, bool)>,
    /// The last look named `forced`.
    still_forced: Option<(String, bool)>,
}

impl Wait {
    fn forcing(holder: Option<String>) -> Self {
        Self {
            forced: holder,
            ..Self::default()
        }
    }

    fn see(&mut self, seen: Seen) -> Option<Verdict> {
        self.still_forced = None;
        match seen {
            Seen::Mine => Some(Verdict::Hosting),
            Seen::Other { holder, pushed } if self.forced.as_ref() == Some(&holder) => {
                self.other = None;
                self.still_forced = Some((holder, pushed));
                None
            }
            Seen::Other { holder, pushed } => {
                let key = (holder, pushed);
                if self.other.as_ref() == Some(&key) {
                    let (holder, pushed) = key;
                    return Some(Verdict::Held { holder, pushed });
                }
                self.other = Some(key);
                None
            }
            Seen::Unknown | Seen::Free => {
                self.other = None;
                None
            }
        }
    }

    /// Where the wait ends when the deadline comes first: held, when the last
    /// look still named the holder a force was sent against.
    fn deadline(self) -> Verdict {
        match self.still_forced {
            Some((holder, pushed)) => Verdict::Held { holder, pushed },
            None => Verdict::Undecided,
        }
    }
}

/// The verdict a sequence of looks reaches for a claim; `Undecided` when it
/// runs out first. The deadline is the caller's: it decides how many looks
/// there are. [`wait_for_verdict`] feeds [`Wait`] the same way, one look at a
/// time.
#[cfg(test)]
fn verdict(looks: impl IntoIterator<Item = Seen>) -> Verdict {
    verdict_of(Wait::default(), looks)
}

/// [`verdict`] for any wait, a force's included.
#[cfg(test)]
fn verdict_of(mut wait: Wait, looks: impl IntoIterator<Item = Seen>) -> Verdict {
    let settled = looks.into_iter().find_map(|seen| wait.see(seen));
    settled.unwrap_or_else(|| wait.deadline())
}

/// Look at the lease every [`POLL_EVERY`] until a verdict or [`VERDICT_WAIT`].
async fn wait_for_verdict(client: &mut Client, save_id: &str, mut wait: Wait) -> Verdict {
    let my_fp = this_device();
    let me = this_account();
    let behind = std::cell::Cell::new(false);
    let polls = async {
        loop {
            tokio::time::sleep(POLL_EVERY).await;
            // The engine's word on whose lease it is, when it has one; the
            // server's row for who holds it and whether they pushed.
            let engine = match link::ask(client, Request::Status).await {
                Ok(Payload::Status(status)) => {
                    behind.set(
                        status
                            .slots
                            .iter()
                            .any(|s| s.save_id == save_id && s.lease_behind),
                    );
                    Hosts::from_status(&status).lease(save_id)
                }
                _ => None,
            };
            let seen = match lease(client, save_id).await {
                Ok(lease) => seen(lease.as_ref(), engine, &my_fp, me.as_deref()),
                Err(err) => {
                    tracing::debug!(error = %format!("{err:#}"), "cli: couldn't read the lease on {save_id}");
                    Seen::Unknown
                }
            };
            if let Some(verdict) = wait.see(seen) {
                return verdict;
            }
        }
    };
    match tokio::time::timeout(VERDICT_WAIT, polls).await {
        Ok(verdict) => verdict,
        Err(_) => timed_out(wait, behind.get()),
    }
}

/// Where a wait ends at the deadline: behind the head when the engine says so,
/// whatever the looks were; otherwise the wait's own deadline.
fn timed_out(wait: Wait, behind: bool) -> Verdict {
    if behind {
        Verdict::Behind
    } else {
        wait.deadline()
    }
}

/// One lease read as a look: a quiet lease is as good as free. Whose it is is
/// the engine's word when it has one. Otherwise ours is a lease held from this
/// machine by this account, as the engine counts it: every account and OS user
/// here shares the fingerprint. With the account unknown, a lease from this
/// machine says nothing either way (see [`held_here`]).
fn seen(lease: Option<&Lease>, engine: Option<WorldLease>, my_fp: &str, me: Option<&str>) -> Seen {
    let other = |l: &Lease| Seen::Other {
        holder: l.holder_username.to_string(),
        pushed: l.pushed_since,
    };
    match lease {
        None => Seen::Free,
        Some(l) if !l.live => Seen::Free,
        Some(l) => match engine {
            Some(WorldLease::Mine) => Seen::Mine,
            Some(WorldLease::Other) => other(l),
            _ if l.holder_device_fp.as_deref() == Some(my_fp) => match me {
                Some(id) if l.holder_user_id == id => Seen::Mine,
                Some(_) => other(l),
                None => Seen::Unknown,
            },
            _ => other(l),
        },
    }
}

/// A verdict as the command's answer. Held is a refusal, and for a takeover a
/// lease the holder has pushed under is its own one.
fn outcome_of(save_id: &str, verdict: Verdict, force: bool) -> Result<Outcome> {
    match verdict {
        Verdict::Hosting => Ok(Outcome::Hosting),
        Verdict::Undecided => Ok(Outcome::Pending),
        Verdict::Behind => Err(output::err(
            "stale",
            format!(
                "{save_id} is behind the latest version; Hoard is pulling it, then claim again"
            ),
        )),
        Verdict::Held {
            holder,
            pushed: true,
        } if force => Err(output::err(
            "pushed",
            format!(
                "{holder} has pushed to {save_id} under their lease, so it cannot be taken; \
                 ask them to release it"
            ),
        )),
        Verdict::Held { holder, .. } => Err(output::err(
            "held",
            format!(
                "{holder} is hosting {save_id}; view it with `hoard world claim {save_id} --view`, \
                 or ask them to release it"
            ),
        )),
    }
}

/// The lease on `save_id`, or `None` when nobody holds one.
pub async fn lease(client: &mut Client, save_id: &str) -> Result<Option<Lease>> {
    match link::ask(
        client,
        Request::GetLease {
            save_id: save_id.to_string(),
        },
    )
    .await?
    {
        Payload::Lease { lease } => Ok(lease.map(|l| *l)),
        other => anyhow::bail!("unexpected answer to a lease request: {other:?}"),
    }
}

/// Who hosts each shared save, as the engine last heard it: every slot's lease
/// from one status read, so a table costs one local call and no server call.
pub struct Hosts {
    slots: HashMap<String, (Option<WorldLease>, Option<String>)>,
}

/// A table's host cell and the lease behind it.
#[derive(Debug, PartialEq)]
pub struct Host {
    /// "hosted here", "hosted by <name>", "nobody" or "unknown".
    pub cell: String,
    /// "mine", "other", "free" or "unknown", for a script to branch on.
    pub lease: &'static str,
}

impl Hosts {
    /// `None` with no service answering: no column rather than a wrong one.
    pub async fn read() -> Option<Hosts> {
        link::status().await.map(|s| Hosts::from_status(&s))
    }

    pub fn from_status(status: &DaemonStatus) -> Hosts {
        Hosts {
            slots: status
                .slots
                .iter()
                .map(|s| (s.save_id.clone(), (s.lease, s.lease_holder.clone())))
                .collect(),
        }
    }

    /// The engine's lease on `save_id`, `None` when it has no shared slot for it.
    pub fn lease(&self, save_id: &str) -> Option<WorldLease> {
        self.slots.get(save_id).and_then(|(lease, _)| *lease)
    }

    /// The host of a shared save. One the engine has no slot for is unknown.
    pub fn of(&self, save_id: &str) -> Host {
        let holder = self.slots.get(save_id).and_then(|(_, h)| h.as_deref());
        host(self.lease(save_id).unwrap_or(WorldLease::Unknown), holder)
    }
}

fn host(lease: WorldLease, holder: Option<&str>) -> Host {
    let (cell, tag) = match lease {
        WorldLease::Mine => ("hosted here".to_string(), "mine"),
        WorldLease::Other => (
            holder
                .map(|h| format!("hosted by {h}"))
                .unwrap_or_else(|| "hosted elsewhere".to_string()),
            "other",
        ),
        WorldLease::Free => ("nobody".to_string(), "free"),
        WorldLease::Unknown => ("unknown".to_string(), "unknown"),
    };
    Host { cell, lease: tag }
}

/// This machine's fingerprint, the one the service stamps on a lease it takes.
fn this_device() -> String {
    hoard_agent::logship::device_identity().fingerprint
}

/// This account's id as the self-hosted session caches it (`session.toml`), with
/// no server call and no keyring. `None` when there is no session, or it was
/// written from `config.toml` alone and never cached the whoami.
pub fn this_account() -> Option<String> {
    hoard_agent::credentials::load_public()
        .ok()
        .flatten()
        .and_then(|(_, user)| user)
        .map(|u| u.user_id)
}

/// Whether the lease is held here. The engine decides by account once it knows
/// who holds the lease; while its slot reads unknown or free (just restarted),
/// or it has no slot, the fingerprint on the lease says, and only for a lease
/// this account holds: two accounts on one computer share the fingerprint. An
/// account that cannot be known here is not the holder.
fn held_here(lease: &Lease, engine: Option<WorldLease>, my_fp: &str, me: Option<&str>) -> bool {
    match engine {
        Some(WorldLease::Mine) => true,
        Some(WorldLease::Other) => false,
        _ => {
            lease.holder_device_fp.as_deref() == Some(my_fp)
                && me == Some(lease.holder_user_id.as_str())
        }
    }
}

fn detail(lease: &Lease, engine: Option<WorldLease>, my_fp: &str, me: Option<&str>) -> LeaseDetail {
    LeaseDetail {
        holder: lease.holder_username.to_string(),
        here: held_here(lease, engine, my_fp, me),
        acquired_at: rfc3339(lease.acquired_at),
        renewed_at: rfc3339(lease.renewed_at),
        base_version: lease.base_version,
        live: lease.live,
        pushed_since: lease.pushed_since,
    }
}

fn rfc3339(t: time::OffsetDateTime) -> String {
    t.format(&Rfc3339).unwrap_or_else(|_| t.to_string())
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hoard_core::ids::Username;
    use time::OffsetDateTime;

    fn lease(fp: Option<&str>, live: bool) -> Lease {
        Lease {
            save_id: "s1".into(),
            holder_user_id: "u2".into(),
            holder_username: Username::parse("alice").unwrap(),
            holder_device_fp: fp.map(String::from),
            acquired_at: OffsetDateTime::UNIX_EPOCH,
            renewed_at: OffsetDateTime::UNIX_EPOCH,
            base_version: 4,
            pushed_since: false,
            live,
        }
    }

    #[test]
    fn a_live_lease_on_this_machine_is_hosted_here() {
        assert_eq!(
            host(WorldLease::Mine, None),
            Host {
                cell: "hosted here".into(),
                lease: "mine"
            }
        );
        // The engine's verdict wins over the fingerprint, both ways.
        let l = lease(Some("fp-them"), true);
        assert!(held_here(&l, Some(WorldLease::Mine), "fp-me", Some("u2")));
        let l = lease(Some("fp-me"), true);
        assert!(!held_here(&l, Some(WorldLease::Other), "fp-me", Some("u2")));
        // With no slot the fingerprint decides.
        assert!(held_here(&l, None, "fp-me", Some("u2")));
        // Just restarted, the engine has not heard who holds it: the
        // fingerprint still says, either way.
        assert!(held_here(
            &l,
            Some(WorldLease::Unknown),
            "fp-me",
            Some("u2")
        ));
        assert!(held_here(&l, Some(WorldLease::Free), "fp-me", Some("u2")));
        assert!(!held_here(
            &l,
            Some(WorldLease::Unknown),
            "fp-other",
            Some("u2")
        ));
    }

    /// Two accounts on one computer share its fingerprint: the fallback also
    /// needs the lease to be this account's, and an account it cannot know is
    /// not the holder. The engine's verdict still wins.
    #[test]
    fn a_lease_of_another_account_on_this_machine_is_not_hosted_here() {
        let l = lease(Some("fp-me"), true);
        assert!(!held_here(&l, None, "fp-me", Some("u3")));
        assert!(!held_here(
            &l,
            Some(WorldLease::Unknown),
            "fp-me",
            Some("u3")
        ));
        assert!(!held_here(&l, Some(WorldLease::Free), "fp-me", Some("u3")));
        assert!(!held_here(&l, None, "fp-me", None));
        assert!(held_here(&l, Some(WorldLease::Mine), "fp-me", None));
        assert_eq!(
            seen(Some(&l), None, "fp-me", Some("u3")),
            other("alice", false)
        );
        assert!(!detail(&l, None, "fp-me", Some("u3")).here);
    }

    #[test]
    fn a_live_lease_elsewhere_names_the_holder() {
        assert_eq!(
            host(WorldLease::Other, Some("alice")).cell,
            "hosted by alice"
        );
        assert_eq!(host(WorldLease::Other, None).cell, "hosted elsewhere");
        assert_eq!(host(WorldLease::Other, None).lease, "other");
        // A server that kept no fingerprint is not held here.
        assert!(!held_here(&lease(None, true), None, "fp-me", Some("u2")));
    }

    /// Every shared save gets a cell: a slot the engine has no lease for, or
    /// no slot at all, is unknown rather than blank.
    #[test]
    fn hosts_read_every_slot_from_one_status() {
        use hoard_core::ipc::AgentSlotStatus;
        let slot = |id: &str, lease: Option<WorldLease>, holder: Option<&str>| AgentSlotStatus {
            save_id: id.into(),
            display_name: id.into(),
            path: "/saves".into(),
            watcher_armed: true,
            process_running: false,
            last_fs_event_at: None,
            next_scheduled_backup_at: None,
            shared: lease.is_some(),
            lease,
            lease_holder: holder.map(String::from),
            lease_behind: false,
        };
        let mut status: DaemonStatus = serde_json::from_value(serde_json::json!({
            "daemon_version": "0", "protocol": 1, "pid": 1, "epoch": "e",
            "uptime_secs": 0, "cursor": 0, "engine": {"running": true}, "slots": []
        }))
        .unwrap();
        status.slots = vec![
            slot("s1", Some(WorldLease::Mine), None),
            slot("s2", Some(WorldLease::Other), Some("bob")),
            slot("s3", Some(WorldLease::Free), None),
            slot("s4", None, None),
        ];
        let hosts = Hosts::from_status(&status);
        assert_eq!(hosts.of("s1").cell, "hosted here");
        assert_eq!(hosts.of("s2").cell, "hosted by bob");
        assert_eq!(hosts.of("s3"), host(WorldLease::Free, None));
        assert_eq!(hosts.of("s4").lease, "unknown");
        assert_eq!(hosts.of("gone").cell, "unknown");
        assert_eq!(hosts.lease("s4"), None);
        assert_eq!(hosts.lease("s2"), Some(WorldLease::Other));
    }

    /// No lease is one `null`, not a row of `false`s that reads as a lease.
    #[test]
    fn no_lease_is_null_and_a_lease_is_whole() {
        let none = LeaseOut {
            save_id: "s1".into(),
            lease: None,
        };
        assert_eq!(
            serde_json::to_value(&none).unwrap(),
            serde_json::json!({"save_id": "s1", "lease": null})
        );
        let some = LeaseOut {
            save_id: "s1".into(),
            lease: Some(detail(
                &lease(Some("fp-me"), true),
                None,
                "fp-me",
                Some("u2"),
            )),
        };
        let v = serde_json::to_value(&some).unwrap();
        assert_eq!(v["lease"]["holder"], "alice");
        assert_eq!(v["lease"]["here"], true);
        assert_eq!(v["lease"]["base_version"], 4);
        assert_eq!(v["lease"]["acquired_at"], "1970-01-01T00:00:00Z");
    }

    #[test]
    fn a_quiet_lease_shows_nothing() {
        // The engine calls a lease nobody renews free; the cell says so.
        assert_eq!(
            host(WorldLease::Free, Some("alice")),
            Host {
                cell: "nobody".into(),
                lease: "free"
            }
        );
    }

    fn other(holder: &str, pushed: bool) -> Seen {
        Seen::Other {
            holder: holder.into(),
            pushed,
        }
    }

    #[test]
    fn a_lease_of_ours_settles_the_wait_at_once() {
        assert_eq!(
            verdict([Seen::Unknown, Seen::Free, Seen::Mine]),
            Verdict::Hosting
        );
        // The old holder seen once, then ours: a takeover that landed.
        assert_eq!(verdict([other("bob", false), Seen::Mine]), Verdict::Hosting);
    }

    #[test]
    fn a_holder_seen_twice_in_a_row_is_held() {
        assert_eq!(
            verdict([other("bob", false), other("bob", false)]),
            Verdict::Held {
                holder: "bob".into(),
                pushed: false
            }
        );
        assert_eq!(
            verdict([
                Seen::Free,
                other("bob", true),
                other("bob", true),
                Seen::Mine
            ]),
            Verdict::Held {
                holder: "bob".into(),
                pushed: true
            }
        );
    }

    /// One look at the old holder is not a verdict, nor two that disagree.
    #[test]
    fn a_holder_seen_once_is_not_a_verdict() {
        assert_eq!(verdict([other("bob", false)]), Verdict::Undecided);
        assert_eq!(
            verdict([other("bob", false), Seen::Free, other("bob", false)]),
            Verdict::Undecided
        );
        assert_eq!(
            verdict([other("bob", false), other("carol", false)]),
            Verdict::Undecided
        );
    }

    #[test]
    fn nothing_heard_by_the_deadline_is_undecided() {
        assert_eq!(verdict([]), Verdict::Undecided);
        assert_eq!(verdict(vec![Seen::Unknown; 20]), Verdict::Undecided);
        assert_eq!(verdict(vec![Seen::Free; 20]), Verdict::Undecided);
    }

    #[test]
    fn a_lease_reads_as_a_look() {
        let me = Some("u2");
        assert_eq!(seen(None, None, "fp-me", me), Seen::Free);
        assert_eq!(
            seen(Some(&lease(Some("fp-me"), false)), None, "fp-me", me),
            Seen::Free
        );
        assert_eq!(
            seen(Some(&lease(Some("fp-me"), true)), None, "fp-me", me),
            Seen::Mine
        );
        assert_eq!(
            seen(Some(&lease(Some("fp-them"), true)), None, "fp-me", me),
            other("alice", false)
        );
    }

    /// Two accounts on one machine share the fingerprint: another account's
    /// lease is theirs, as the engine counts it.
    #[test]
    fn a_lease_from_this_machine_is_ours_only_for_this_account() {
        let here = lease(Some("fp-me"), true);
        assert_eq!(
            seen(Some(&here), None, "fp-me", Some("u9")),
            other("alice", false)
        );
        assert_eq!(seen(Some(&here), None, "fp-me", None), Seen::Unknown);
        // This account from another machine is not ours either.
        assert_eq!(
            seen(
                Some(&lease(Some("fp-them"), true)),
                None,
                "fp-me",
                Some("u2")
            ),
            other("alice", false)
        );
    }

    /// At the deadline a world behind the head answers `Behind`, even for a
    /// force whose last look still named the holder it was sent against.
    #[test]
    fn behind_the_head_outranks_a_held_force_at_the_deadline() {
        let forced = || {
            let mut wait = Wait::forcing(Some("bob".into()));
            assert_eq!(wait.see(other("bob", false)), None);
            wait
        };
        assert_eq!(timed_out(forced(), true), Verdict::Behind);
        assert_eq!(
            timed_out(forced(), false),
            Verdict::Held {
                holder: "bob".into(),
                pushed: false
            }
        );
    }

    /// A force goes out behind whatever the engine already had on the wire, so
    /// the holder it was sent against is not a refusal until the deadline.
    #[test]
    fn a_force_waits_out_the_holder_it_was_sent_against() {
        let forcing = || Wait::forcing(Some("bob".into()));
        assert_eq!(
            verdict_of(
                forcing(),
                [
                    other("bob", false),
                    other("bob", false),
                    other("bob", false),
                    Seen::Mine
                ]
            ),
            Verdict::Hosting
        );
        // Still there at the deadline: held, or pushed under.
        assert_eq!(
            verdict_of(forcing(), vec![other("bob", false); 20]),
            Verdict::Held {
                holder: "bob".into(),
                pushed: false
            }
        );
        assert_eq!(
            verdict_of(forcing(), [other("bob", true), other("bob", true)]),
            Verdict::Held {
                holder: "bob".into(),
                pushed: true
            }
        );
        // Someone else read twice took it in between.
        assert_eq!(
            verdict_of(
                forcing(),
                [
                    other("bob", false),
                    other("carol", false),
                    other("carol", false)
                ]
            ),
            Verdict::Held {
                holder: "carol".into(),
                pushed: false
            }
        );
        // The last look said nothing about the holder: no verdict.
        assert_eq!(
            verdict_of(forcing(), [other("bob", false), Seen::Unknown]),
            Verdict::Undecided
        );
        assert_eq!(
            verdict_of(forcing(), [other("bob", false), Seen::Free]),
            Verdict::Undecided
        );
        // With no holder known at the force, the wait is a claim's.
        assert_eq!(
            verdict_of(
                Wait::forcing(None),
                [other("bob", false), other("bob", false)]
            ),
            Verdict::Held {
                holder: "bob".into(),
                pushed: false
            }
        );
    }

    fn code(r: Result<Outcome>) -> String {
        output::classify(&r.unwrap_err()).code.into_owned()
    }

    #[test]
    fn a_verdict_becomes_the_answer() {
        assert_eq!(
            outcome_of("s1", Verdict::Hosting, false).unwrap(),
            Outcome::Hosting
        );
        assert_eq!(
            outcome_of("s1", Verdict::Undecided, true).unwrap(),
            Outcome::Pending
        );
        let held = |pushed| Verdict::Held {
            holder: "bob".into(),
            pushed,
        };
        assert_eq!(code(outcome_of("s1", held(false), false)), "held");
        assert_eq!(code(outcome_of("s1", held(false), true)), "held");
        // Pushed is the takeover's refusal; a claim is simply held.
        assert_eq!(code(outcome_of("s1", held(true), true)), "pushed");
        assert_eq!(code(outcome_of("s1", held(true), false)), "held");
        let err = outcome_of("s1", held(false), false).unwrap_err();
        assert!(format!("{err:#}").contains("bob"), "{err:#}");
        assert_eq!(output::classify(&err).exit, 1);
    }

    #[test]
    fn the_outcome_is_one_line_and_one_word() {
        assert_eq!(human_line("s1", Outcome::Hosting, true), "hosting s1");
        assert!(human_line("s1", Outcome::Pending, false).contains("hoard world lease s1"));
        assert!(human_line("s1", Outcome::Pending, true).starts_with("taking the lease"));
        let v = serde_json::to_value(WorldOut {
            save_id: "s1".into(),
            outcome: Outcome::Pending,
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({"save_id": "s1", "outcome": "pending"})
        );
    }

    /// A look takes the engine's word on whose lease it is, and the
    /// fingerprint only while the engine reads unknown or free.
    #[test]
    fn a_look_takes_the_engines_word_on_whose_lease_it_is() {
        let theirs_by_fp = lease(Some("fp-them"), true);
        assert!(matches!(
            seen(
                Some(&theirs_by_fp),
                Some(WorldLease::Mine),
                "fp-me",
                Some("u2")
            ),
            Seen::Mine
        ));
        let mine_by_fp = lease(Some("fp-me"), true);
        assert!(matches!(
            seen(
                Some(&mine_by_fp),
                Some(WorldLease::Other),
                "fp-me",
                Some("u2")
            ),
            Seen::Other { .. }
        ));
        assert!(matches!(
            seen(
                Some(&mine_by_fp),
                Some(WorldLease::Unknown),
                "fp-me",
                Some("u2")
            ),
            Seen::Mine
        ));
    }

    /// A claim that runs out of time while the engine pulls the head answers
    /// `stale`, not a success-shaped pending.
    #[test]
    fn a_world_behind_the_head_answers_stale() {
        let err = outcome_of("s-1", Verdict::Behind, false).unwrap_err();
        let c = output::classify(&err);
        assert_eq!((c.code.as_ref(), c.exit), ("stale", 1));
        assert!(err.to_string().contains("behind"), "{err}");
    }
}
