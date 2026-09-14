//! `hoard group`: the groups this account shares saves with. Every verb is one
//! request to the service, which holds the session and talks to the server; the
//! CLI only turns a name into an id and prints the answer.

use anyhow::Result;
use clap::Subcommand;
use serde::Serialize;
use time::format_description::well_known::Rfc3339;

use hoard_core::ipc::{Payload, Request};
use hoard_core::wire::Group;
use hoardd::client::Client;

use super::link;
use crate::output::{self, truncate};

#[derive(Subcommand)]
pub enum GroupCommand {
    /// Create a group you own. Members join with an invite token.
    Create {
        /// Group name, shown to members and on `hoard saves`
        name: String,
    },
    /// List the groups you belong to
    List,
    /// Mint an invite token for a group you own. The token is shown once.
    Invite {
        /// Group id or exact name, see `hoard group list`
        group: String,
        /// How long the token stays valid: `12h`, `7d`, `30m`, `3600s`
        #[arg(long, default_value = "7d")]
        expires: String,
    },
    /// Join a group with an invite token
    Join {
        /// The token from `hoard group invite`
        token: String,
    },
    /// Leave a group you are a member of (the owner cannot leave)
    Leave {
        /// Group id or exact name, see `hoard group list`
        group: String,
    },
}

/// One group as agents and scripts see it.
#[derive(Serialize)]
pub struct GroupRow {
    pub id: String,
    pub name: String,
    pub owner: String,
    pub members: usize,
}

#[derive(Serialize)]
pub struct GroupsOut {
    pub groups: Vec<GroupRow>,
}

pub async fn run(cmd: GroupCommand) -> Result<()> {
    let mut client = link::require("group").await?;
    match cmd {
        GroupCommand::Create { name } => {
            let group = group_reply(link::ask(&mut client, Request::CreateGroup { name }).await?)?;
            println!("created group '{}' ({})", group.name, group.id);
            Ok(())
        }
        GroupCommand::List => {
            let groups = list(&mut client).await?;
            let out = GroupsOut {
                groups: groups.iter().map(row).collect(),
            };
            output::emit(&out, |out| {
                if out.groups.is_empty() {
                    println!(
                        "you belong to no group.\n\
                         Create one with `hoard group create <name>` or join one with \
                         `hoard group join <token>`."
                    );
                    return;
                }
                println!("{:<24}  {:<16}  {:>7}  ID", "NAME", "OWNER", "MEMBERS");
                for g in &out.groups {
                    println!(
                        "{:<24}  {:<16}  {:>7}  {}",
                        truncate(&g.name, 24),
                        truncate(&g.owner, 16),
                        g.members,
                        g.id
                    );
                }
            })
        }
        GroupCommand::Invite { group, expires } => {
            let expires_in_secs =
                parse_expiry(&expires).map_err(|m| output::err("bad_request", m))?;
            let group_id = resolve(&mut client, &group).await?;
            let invite = match link::ask(
                &mut client,
                Request::InviteToGroup {
                    group_id,
                    expires_in_secs: Some(expires_in_secs),
                },
            )
            .await?
            {
                Payload::Invite(invite) => invite,
                other => anyhow::bail!("unexpected answer to an invite request: {other:?}"),
            };
            let until = invite
                .expires_at
                .format(&Rfc3339)
                .unwrap_or_else(|_| invite.expires_at.to_string());
            println!(
                "invite token (shown once, valid until {until}):\n\n  {}\n\n\
                 Whoever should join runs `hoard group join {}`.",
                invite.token, invite.token
            );
            Ok(())
        }
        GroupCommand::Join { token } => {
            let group = group_reply(link::ask(&mut client, Request::JoinGroup { token }).await?)?;
            println!("joined group '{}' ({})", group.name, group.id);
            Ok(())
        }
        GroupCommand::Leave { group } => {
            let group_id = resolve(&mut client, &group).await?;
            link::ask(
                &mut client,
                Request::LeaveGroup {
                    group_id: group_id.clone(),
                },
            )
            .await?;
            println!("left group {group_id}");
            Ok(())
        }
    }
}

/// The groups this account belongs to, as the service reports them.
pub async fn list(client: &mut Client) -> Result<Vec<Group>> {
    match link::ask(client, Request::ListGroups).await? {
        Payload::Groups { groups } => Ok(groups),
        other => anyhow::bail!("unexpected answer to a group list: {other:?}"),
    }
}

/// The id of the group `<group>` names: an id as-is, or an exact name looked up
/// in the account's groups. A UUID goes straight through without listing them:
/// the server refuses one this account cannot reach.
pub async fn resolve(client: &mut Client, query: &str) -> Result<String> {
    if let Some(id) = as_id(query) {
        return Ok(id);
    }
    let groups = list(client).await?;
    resolve_in(&groups, query).map_err(|e| output::err(e.code(), e.to_string()))
}

fn group_reply(payload: Payload) -> Result<Group> {
    match payload {
        Payload::Group(group) => Ok(*group),
        other => anyhow::bail!("unexpected answer to a group request: {other:?}"),
    }
}

/// Why a `<group>` argument did not name exactly one group.
#[derive(Debug, PartialEq, Eq)]
pub enum Lookup {
    NotFound(String),
    /// Several groups carry the name; their ids, for the user to pick one.
    Ambiguous {
        name: String,
        ids: Vec<String>,
    },
}

impl Lookup {
    fn code(&self) -> &'static str {
        match self {
            Lookup::NotFound(_) => "not_found",
            Lookup::Ambiguous { .. } => "needs_choice",
        }
    }
}

impl std::fmt::Display for Lookup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Lookup::NotFound(q) => write!(
                f,
                "no group named or identified `{q}`; see `hoard group list`"
            ),
            Lookup::Ambiguous { name, ids } => write!(
                f,
                "`{name}` names {} groups; pass the id instead: {}",
                ids.len(),
                ids.join(", ")
            ),
        }
    }
}

/// `query` as a group id when it is a canonical UUID, the shape the server
/// mints; `None` sends it to the name lookup.
pub fn as_id(query: &str) -> Option<String> {
    hoard_core::ids::SaveId::parse(query)
        .ok()
        .map(|id| id.as_str().to_string())
}

/// An id wins over a name: an id is unique by construction, a name is not.
pub fn resolve_in(groups: &[Group], query: &str) -> Result<String, Lookup> {
    if let Some(g) = groups.iter().find(|g| g.id == query) {
        return Ok(g.id.clone());
    }
    let by_name: Vec<&Group> = groups.iter().filter(|g| g.name == query).collect();
    match by_name.as_slice() {
        [] => Err(Lookup::NotFound(query.to_string())),
        [one] => Ok(one.id.clone()),
        many => Err(Lookup::Ambiguous {
            name: query.to_string(),
            ids: many.iter().map(|g| g.id.clone()).collect(),
        }),
    }
}

/// The server's cap on an invite's lifetime: one year.
const MAX_EXPIRY_SECS: u64 = 365 * 86_400;

/// `7d`, `12h`, `30m` or `3600s` as seconds, at most one year. A bare number is
/// refused: it is not obvious whether `7` means days or seconds, so the unit has
/// to be said.
pub fn parse_expiry(raw: &str) -> Result<u64, String> {
    let raw = raw.trim();
    let (digits, unit) = raw.split_at(
        raw.trim_end_matches(|c: char| c.is_ascii_alphabetic())
            .len(),
    );
    // Digits only: `u64::from_str` would take a leading `+`.
    let n: u64 = Some(digits)
        .filter(|d| d.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|d| d.parse().ok())
        .ok_or_else(|| {
            format!("expected a duration such as `7d`, `12h`, `30m` or `3600s`, got `{raw}`")
        })?;
    let per_unit = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => {
            return Err(format!(
                "expected a duration such as `7d`, `12h`, `30m` or `3600s`, got `{raw}`"
            ))
        }
    };
    let secs = n
        .checked_mul(per_unit)
        .filter(|&s| s > 0)
        .ok_or_else(|| format!("`{raw}` is not a usable expiry"))?;
    if secs > MAX_EXPIRY_SECS {
        return Err(format!(
            "an invite expires in at most one year, got `{raw}`"
        ));
    }
    Ok(secs)
}

fn row(g: &Group) -> GroupRow {
    let owner = g
        .members
        .iter()
        .find(|m| m.user_id == g.owner_user_id)
        .map(|m| m.username.to_string())
        .unwrap_or_else(|| g.owner_user_id.clone());
    GroupRow {
        id: g.id.clone(),
        name: g.name.clone(),
        owner,
        members: g.members.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    fn group(id: &str, name: &str) -> Group {
        Group {
            id: id.into(),
            name: name.into(),
            owner_user_id: "u1".into(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            members: Vec::new(),
        }
    }

    #[test]
    fn an_id_resolves_to_itself() {
        let groups = [group("g1", "friends"), group("g2", "raid")];
        assert_eq!(resolve_in(&groups, "g2").unwrap(), "g2");
    }

    #[test]
    fn a_unique_name_resolves_to_its_id() {
        let groups = [group("g1", "friends"), group("g2", "raid")];
        assert_eq!(resolve_in(&groups, "raid").unwrap(), "g2");
    }

    /// An id is unique; a name that happens to equal another group's id must
    /// not shadow that group.
    #[test]
    fn an_id_wins_over_a_name() {
        let groups = [group("g1", "g2"), group("g2", "raid")];
        assert_eq!(resolve_in(&groups, "g2").unwrap(), "g2");
    }

    #[test]
    fn a_duplicated_name_lists_the_ids() {
        let groups = [group("g1", "raid"), group("g2", "raid"), group("g3", "x")];
        let err = resolve_in(&groups, "raid").unwrap_err();
        assert_eq!(
            err,
            Lookup::Ambiguous {
                name: "raid".into(),
                ids: vec!["g1".into(), "g2".into()]
            }
        );
        let text = err.to_string();
        assert!(text.contains("g1, g2"), "{text}");
    }

    #[test]
    fn an_unknown_group_is_not_found() {
        let groups = [group("g1", "raid")];
        assert_eq!(
            resolve_in(&groups, "nope").unwrap_err(),
            Lookup::NotFound("nope".into())
        );
        assert!(resolve_in(&[], "g1").is_err());
    }

    #[test]
    fn expiry_takes_a_unit() {
        assert_eq!(parse_expiry("7d").unwrap(), 7 * 86_400);
        assert_eq!(parse_expiry("12h").unwrap(), 12 * 3600);
        assert_eq!(parse_expiry("30m").unwrap(), 1800);
        assert_eq!(parse_expiry("3600s").unwrap(), 3600);
        assert!(parse_expiry("7").is_err());
        assert!(parse_expiry("0d").is_err());
        assert!(parse_expiry("d").is_err());
        assert!(parse_expiry("7w").is_err());
    }

    #[test]
    fn expiry_refuses_a_sign() {
        assert!(parse_expiry("+7d").is_err());
        assert!(parse_expiry("-7d").is_err());
    }

    /// The server refuses past a year; saying so here costs no round trip.
    #[test]
    fn expiry_is_capped_at_a_year() {
        assert_eq!(parse_expiry("365d").unwrap(), 365 * 86_400);
        assert_eq!(parse_expiry("8760h").unwrap(), 365 * 86_400);
        let err = parse_expiry("366d").unwrap_err();
        assert!(err.contains("at most one year"), "{err}");
        assert!(parse_expiry("8761h").is_err());
        // Overflow stays its own refusal, not a panic.
        assert!(parse_expiry("99999999999999999999d").is_err());
    }

    #[test]
    fn a_uuid_skips_the_lookup_and_a_name_does_not() {
        let id = "0f8a5c2e-3b1d-4e6f-9a7b-1c2d3e4f5a6b";
        assert_eq!(as_id(id).as_deref(), Some(id));
        assert_eq!(as_id("raid"), None);
        assert_eq!(as_id("g1"), None);
        // Not the canonical shape: looked up by name, where it can still match.
        assert_eq!(as_id(&id.to_uppercase()), None);
    }
}
