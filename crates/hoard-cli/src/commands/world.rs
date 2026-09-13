//! `hoard world`: who hosts a shared world. The verbs are engine commands the
//! service acknowledges at once; what came of them arrives as events, which
//! `hoard sync logs` shows and `hoard world lease` reads back from the server.

use anyhow::Result;
use clap::Subcommand;
use serde::Serialize;
use time::format_description::well_known::Rfc3339;

use hoard_core::ipc::{Payload, Request, WorldRole};
use hoard_core::wire::Lease;
use hoardd::client::Client;

use super::link;
use crate::output;

#[derive(Subcommand)]
pub enum WorldCommand {
    /// Take a role on a shared world: host it (acquire the lease, your changes
    /// upload) or only view it (pull, never push)
    Claim {
        /// Save id (UUID), see `hoard saves`
        save_id: String,
        /// Host the world: the default
        #[arg(long, conflicts_with = "view")]
        host: bool,
        /// View the world only: your writes to it stay on this machine
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
    pub holder: Option<String>,
    /// The lease is held by this machine.
    pub here: bool,
    pub acquired_at: Option<String>,
    pub renewed_at: Option<String>,
    pub base_version: Option<i64>,
    pub live: bool,
    pub pushed_since: bool,
}

pub async fn run(cmd: WorldCommand) -> Result<()> {
    let mut client = link::require("world").await?;
    match cmd {
        WorldCommand::Claim { save_id, view, .. } => {
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
            let my_fp = this_device();
            let out = LeaseOut {
                save_id: save_id.clone(),
                holder: lease.as_ref().map(|l| l.holder_username.to_string()),
                here: lease.as_ref().is_some_and(|l| held_here(l, &my_fp)),
                acquired_at: lease.as_ref().map(|l| rfc3339(l.acquired_at)),
                renewed_at: lease.as_ref().map(|l| rfc3339(l.renewed_at)),
                base_version: lease.as_ref().map(|l| l.base_version),
                live: lease.as_ref().is_some_and(|l| l.live),
                pushed_since: lease.as_ref().is_some_and(|l| l.pushed_since),
            };
            output::emit(&out, |o| {
                let Some(holder) = &o.holder else {
                    println!("{}: nobody is hosting", o.save_id);
                    return;
                };
                println!(
                    "holder:  {}{}\nsince:   {}\nrenewed: {}\nbase:    v{}\nlive:    {}\npushed:  {}",
                    holder,
                    if o.here { " (this machine)" } else { "" },
                    o.acquired_at.as_deref().unwrap_or("—"),
                    o.renewed_at.as_deref().unwrap_or("—"),
                    o.base_version.unwrap_or(0),
                    yes_no(o.live),
                    yes_no(o.pushed_since),
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

/// "hosted here" or "hosted by alice" for a live lease; `None` when nobody is
/// hosting, the lease has gone quiet, or the service could not say. For the
/// tables: a column that is blank rather than wrong when the answer is not
/// there.
pub async fn hosted(client: &mut Client, save_id: &str, my_fp: &str) -> Option<String> {
    match lease(client, save_id).await {
        Ok(lease) => lease.as_ref().and_then(|l| hosted_label(l, my_fp)),
        Err(err) => {
            tracing::debug!(error = %format!("{err:#}"), "cli: couldn't read the lease on {save_id}");
            None
        }
    }
}

/// This machine's fingerprint, the one the service stamps on a lease it takes.
pub fn this_device() -> String {
    hoard_agent::logship::device_identity().fingerprint
}

fn held_here(lease: &Lease, my_fp: &str) -> bool {
    lease.holder_device_fp.as_deref() == Some(my_fp)
}

/// The label a table shows for a lease: only a live one names a holder.
pub fn hosted_label(lease: &Lease, my_fp: &str) -> Option<String> {
    if !lease.live {
        return None;
    }
    Some(if held_here(lease, my_fp) {
        "hosted here".to_string()
    } else {
        format!("hosted by {}", lease.holder_username)
    })
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
            hosted_label(&lease(Some("fp-me"), true), "fp-me").as_deref(),
            Some("hosted here")
        );
    }

    #[test]
    fn a_live_lease_elsewhere_names_the_holder() {
        assert_eq!(
            hosted_label(&lease(Some("fp-them"), true), "fp-me").as_deref(),
            Some("hosted by alice")
        );
        // A server that kept no fingerprint still names the holder.
        assert_eq!(
            hosted_label(&lease(None, true), "fp-me").as_deref(),
            Some("hosted by alice")
        );
    }

    #[test]
    fn a_quiet_lease_shows_nothing() {
        assert_eq!(hosted_label(&lease(Some("fp-me"), false), "fp-me"), None);
    }
}
