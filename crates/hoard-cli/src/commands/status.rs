use anyhow::Result;
use serde::Serialize;

use hoard_agent::api::ApiClient;
use hoard_agent::config::CliConfig;
use hoard_agent::session;
use hoard_agent::state::CliState;

use super::{link, world};
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
    /// "hosted here" or "hosted by <name>" while a live lease is held. Absent
    /// when nobody hosts, or with no service to ask.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hosted: Option<String>,
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
        shared: shared_saves().await?,
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

/// This machine's shared saves, with the holder from the service when there is
/// one to ask. Local state otherwise: the list is still right, only the holder
/// is unknown.
async fn shared_saves() -> Result<Vec<SharedRow>> {
    session::set_context_offline();
    let (state, _) = CliState::load_default()?;
    let mut rows: Vec<_> = state
        .saves
        .iter()
        .filter_map(|(id, s)| s.shared.as_ref().map(|g| (id, s, g)))
        .collect();
    rows.sort_by(|(_, a, _), (_, b, _)| {
        a.game_slug
            .cmp(&b.game_slug)
            .then_with(|| a.label.cmp(&b.label))
    });
    let mut service = if rows.is_empty() {
        None
    } else {
        link::attached("status").await
    };
    let my_fp = world::this_device();
    let mut out = Vec::with_capacity(rows.len());
    for (id, s, g) in rows {
        let hosted = match service.as_mut() {
            Some(client) => world::hosted(client, id, &my_fp).await,
            None => None,
        };
        out.push(SharedRow {
            save_id: id.clone(),
            game_slug: s.game_slug.clone(),
            label: s.label.clone(),
            group: g.group_name.clone(),
            hosted,
        });
    }
    Ok(out)
}
