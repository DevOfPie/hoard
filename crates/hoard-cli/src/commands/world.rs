//! `hoard world`: who hosts a shared world. The verbs are engine commands: the
//! service refuses one for a save that is not a shared world here, and
//! otherwise answers once the engine has taken it. What the server makes of the
//! lease arrives as events, which `hoard sync logs` shows and `hoard world
//! lease` reads back from the server.
//! The tables (`hoard saves`, `hoard status`) read who hosts from the engine
//! instead, see [`Hosts`].

use std::collections::HashMap;

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
    /// never push)
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
    /// pushed under it: only an idle lease can be taken.
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
    /// otherwise this machine's fingerprint against the one on the lease.
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
            let verb = match role {
                WorldRole::Host => "host",
                WorldRole::View => "view",
            };
            println!("asked to {verb} {save_id}; the outcome shows in `hoard sync logs` and `hoard world lease {save_id}`");
            Ok(())
        }
        WorldCommand::Release { save_id } => {
            link::ask(
                &mut client,
                Request::ReleaseWorld {
                    save_id: save_id.clone(),
                },
            )
            .await?;
            println!("releasing the lease on {save_id}");
            Ok(())
        }
        WorldCommand::Force { save_id } => {
            link::ask(
                &mut client,
                Request::ForceWorld {
                    save_id: save_id.clone(),
                },
            )
            .await?;
            println!("taking the lease on {save_id}; the outcome shows in `hoard sync logs`");
            Ok(())
        }
        WorldCommand::Dismiss { save_id } => {
            link::ask(
                &mut client,
                Request::DismissWorld {
                    save_id: save_id.clone(),
                },
            )
            .await?;
            println!("dismissed {save_id}: no role this session");
            Ok(())
        }
        WorldCommand::Lease { save_id } => {
            let lease = lease(&mut client, &save_id).await?;
            // Asked on the same connection; a service that cannot say leaves
            // `here` to the fingerprint.
            let engine = match link::ask(&mut client, Request::Status).await {
                Ok(Payload::Status(status)) => Hosts::from_status(&status).lease(&save_id),
                _ => None,
            };
            let out = LeaseOut {
                save_id: save_id.clone(),
                lease: lease.as_ref().map(|l| detail(l, engine, &this_device())),
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
        Payload::Lease(lease) => Ok(lease.map(|l| *l)),
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

/// Whether the lease is held here. The engine decides by account once it knows
/// who holds the lease; while its slot reads unknown or free (just restarted),
/// or it has no slot, the fingerprint on the lease says.
fn held_here(lease: &Lease, engine: Option<WorldLease>, my_fp: &str) -> bool {
    match engine {
        Some(WorldLease::Mine) => true,
        Some(WorldLease::Other) => false,
        _ => lease.holder_device_fp.as_deref() == Some(my_fp),
    }
}

fn detail(lease: &Lease, engine: Option<WorldLease>, my_fp: &str) -> LeaseDetail {
    LeaseDetail {
        holder: lease.holder_username.to_string(),
        here: held_here(lease, engine, my_fp),
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
        assert!(held_here(&l, Some(WorldLease::Mine), "fp-me"));
        let l = lease(Some("fp-me"), true);
        assert!(!held_here(&l, Some(WorldLease::Other), "fp-me"));
        // With no slot the fingerprint decides.
        assert!(held_here(&l, None, "fp-me"));
        // Just restarted, the engine has not heard who holds it: the
        // fingerprint still says, either way.
        assert!(held_here(&l, Some(WorldLease::Unknown), "fp-me"));
        assert!(held_here(&l, Some(WorldLease::Free), "fp-me"));
        assert!(!held_here(&l, Some(WorldLease::Unknown), "fp-other"));
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
        assert!(!held_here(&lease(None, true), None, "fp-me"));
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
            lease: Some(detail(&lease(Some("fp-me"), true), None, "fp-me")),
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
}
