use anyhow::Result;
use serde::Serialize;

use hoard_agent::api::ApiClient;
use hoard_agent::config::CliConfig;
use hoard_agent::session;
use hoard_agent::state::CliState;

use super::world;
use crate::output;

#[derive(Serialize)]
pub struct StatusOut {
    pub server: String,
    pub status: String,
    pub version: String,
    pub uptime_secs: u64,
    /// The saves this machine shares, with who hosts each. Appended: an older
    /// reader ignores it.
    pub shared: Vec<SharedRow>,
}

/// One shared save on this machine.
#[derive(Serialize)]
pub struct SharedRow {
    pub save_id: String,
    pub game_slug: String,
    pub label: String,
    pub group: String,
    /// Who hosts it, as in `hoard saves`. Absent with no service to ask.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hosted: Option<String>,
    /// "mine", "other", "free" or "unknown"; present exactly when `hosted` is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease: Option<&'static str>,
}

pub async fn run() -> Result<()> {
    let (cfg, _) = CliConfig::load_default()?;
    // Use whatever token we have, or none: /v1/health is unauthenticated.
    let token = cfg.auth.token.clone().unwrap_or_default();
    let client = ApiClient::new(cfg.server.url.clone(), token)?;
    let h = client.health().await?;
    let out = StatusOut {
        server: cfg.server.url.clone(),
        status: h.status,
        version: h.version,
        uptime_secs: h.uptime_secs as u64,
        shared: shared_saves().await,
    };
    output::emit(&out, |o| {
        println!(
            "server:  {}\nstatus:  {}\nversion: {}\nuptime:  {}s",
            o.server, o.status, o.version, o.uptime_secs
        );
        for s in &o.shared {
            println!(
                "shared:  {}/{} in '{}'{}",
                s.game_slug,
                s.label,
                s.group,
                s.hosted
                    .as_deref()
                    .map(|h| format!(", {h}"))
                    .unwrap_or_default()
            );
        }
    })
}

/// This machine's shared saves, with who hosts each from one status read of
/// the service when there is one. Local state otherwise: the list is still
/// right, only the host is left out.
async fn shared_saves() -> Vec<SharedRow> {
    session::set_context_offline();
    let mut rows = shared_in(CliState::load_default().map(|(state, _)| state));
    if rows.is_empty() {
        return rows;
    }
    if let Some(hosts) = world::Hosts::read().await {
        for row in &mut rows {
            let host = hosts.of(&row.save_id);
            row.hosted = Some(host.cell);
            row.lease = Some(host.lease);
        }
    }
    rows
}

/// The shared rows of a state load, sorted, holders unknown. A load that
/// failed is no shared saves: health needs no local state, so `hoard status`
/// still answers.
fn shared_in(loaded: Result<CliState>) -> Vec<SharedRow> {
    let state = match loaded {
        Ok(state) => state,
        Err(err) => {
            tracing::debug!(error = %format!("{err:#}"), "cli: couldn't load local state for the shared saves");
            return Vec::new();
        }
    };
    let mut rows: Vec<SharedRow> = state
        .saves
        .iter()
        .filter_map(|(id, s)| {
            s.shared.as_ref().map(|g| SharedRow {
                save_id: id.clone(),
                game_slug: s.game_slug.clone(),
                label: s.label.clone(),
                group: g.group_name.clone(),
                hosted: None,
                lease: None,
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        a.game_slug
            .cmp(&b.game_slug)
            .then_with(|| a.label.cmp(&b.label))
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn save(game: &str, label: &str, group: Option<&str>) -> serde_json::Value {
        let mut v = serde_json::json!({
            "local_path": "/saves",
            "game_slug": game,
            "label": label,
            "last_version_num": null,
        });
        if let Some(group) = group {
            v["shared"] = serde_json::json!({
                "group_id": "g1",
                "group_name": group,
                "owner_user_id": "u1",
                "owner_username": "alice",
            });
        }
        v
    }

    #[test]
    fn a_state_that_fails_to_load_shares_nothing() {
        assert!(shared_in(Err(anyhow::anyhow!("state.json is corrupt"))).is_empty());
    }

    #[test]
    fn only_shared_saves_are_listed_in_order() {
        let state: CliState = serde_json::from_value(serde_json::json!({
            "saves": {
                "s1": save("valheim", "main", Some("raid")),
                "s2": save("stardew", "farm", None),
                "s3": save("terraria", "a", Some("friends")),
            }
        }))
        .unwrap();
        let rows = shared_in(Ok(state));
        let ids: Vec<_> = rows.iter().map(|r| r.save_id.as_str()).collect();
        assert_eq!(ids, ["s3", "s1"]);
        assert_eq!(rows[1].group, "raid");
        assert!(rows.iter().all(|r| r.hosted.is_none()));
    }
}
