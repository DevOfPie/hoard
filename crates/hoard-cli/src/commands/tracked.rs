//! `hoard saves`: the saves this machine tracks, meaning what `daemon` and `sync`
//! watch. Local: it reads `contexts/<id>.json` of the active context (Cloud or
//! self-host) and never touches the network itself, so it works offline and is
//! instant. The one exception is the holder of a shared save, which it asks the
//! resident service for and leaves out when there is none.

use std::collections::HashMap;

use anyhow::Result;
use serde::Serialize;
use time::format_description::well_known::Rfc3339;

use hoard_agent::session;
use hoard_agent::state::CliState;

use super::{link, world};
use crate::output::{self, truncate};

/// One tracked save as agents and scripts see it. Declared here on purpose:
/// `SaveState` is the engine's own struct and must stay free to change.
#[derive(Serialize)]
pub struct SaveRow {
    pub save_id: String,
    pub game_slug: String,
    pub label: String,
    /// Absolute and untruncated. The table clips this, JSON must not.
    pub local_path: String,
    pub paused: bool,
    pub last_version_num: Option<i64>,
    /// RFC3339, or null when this save has never been backed up.
    pub last_backup_at: Option<String>,
    pub preset: Option<String>,
    /// The group this save is shared into, or null.
    pub group: Option<String>,
    /// "hosted here" or "hosted by <name>" while somebody holds a live lease on
    /// a shared save. Absent when nobody does, or with no service to ask.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hosted: Option<String>,
}

#[derive(Serialize)]
pub struct SavesOut {
    pub saves: Vec<SaveRow>,
    pub state_file: String,
}

pub async fn run() -> Result<()> {
    // No network: pin the active account's context via the stored JWT/URL.
    session::set_context_offline();
    let (state, path) = CliState::load_default()?;

    // Stable order by game name + label for deterministic output.
    let mut rows: Vec<_> = state.saves.iter().collect();
    rows.sort_by(|(_, a), (_, b)| {
        a.game_slug
            .cmp(&b.game_slug)
            .then_with(|| a.label.cmp(&b.label))
    });

    // The holder is the service's to know (it holds the session). No service
    // means no column, not a column of dashes claiming nobody hosts.
    let mut service = if rows.iter().any(|(_, s)| s.shared.is_some()) {
        link::attached("saves").await
    } else {
        None
    };
    let my_fp = world::this_device();
    let mut hosted = HashMap::new();
    if let Some(client) = service.as_mut() {
        for (id, s) in &rows {
            if s.shared.is_some() {
                if let Some(label) = world::hosted(client, id, &my_fp).await {
                    hosted.insert((*id).clone(), label);
                }
            }
        }
    }
    let host_column = service.is_some();

    let out = SavesOut {
        saves: rows
            .into_iter()
            .map(|(id, s)| SaveRow {
                save_id: id.clone(),
                game_slug: s.game_slug.clone(),
                label: s.label.clone(),
                local_path: s.local_path.display().to_string(),
                paused: s.paused,
                last_version_num: s.last_version_num,
                last_backup_at: s.last_backup_at.and_then(|t| t.format(&Rfc3339).ok()),
                preset: s.preset.clone(),
                group: s.shared.as_ref().map(|g| g.group_name.clone()),
                hosted: hosted.remove(id),
            })
            .collect(),
        state_file: path.display().to_string(),
    };

    output::emit(&out, |out| {
        if out.saves.is_empty() {
            println!(
                "you don't track any save on this machine.\n\
                 Add one with `hoard track \"<game>\"`."
            );
            return;
        }
        let host = |cell: &str| {
            if host_column {
                format!("{cell:<16}  ")
            } else {
                String::new()
            }
        };
        println!(
            "{:<24}  {:<10}  {:>6}  {:<20}  {:<8}  {:<12}  {}PATH",
            "GAME",
            "LABEL",
            "VER",
            "LAST",
            "STATE",
            "GROUP",
            host("HOST")
        );
        for s in &out.saves {
            let ver = s
                .last_version_num
                .map(|v| format!("v{v}"))
                .unwrap_or_else(|| "—".to_string());
            let last = s
                .last_backup_at
                .as_deref()
                .map(|t| t.chars().take(19).collect::<String>().replace('T', " "))
                .unwrap_or_else(|| "—".to_string());
            let state_label = if s.paused { "paused" } else { "active" };
            let group = s
                .group
                .as_deref()
                .map(|g| truncate(g, 12))
                .unwrap_or_else(|| "—".to_string());
            let hosted = s
                .hosted
                .as_deref()
                .map(|h| truncate(h, 16))
                .unwrap_or_else(|| "—".to_string());
            println!(
                "{:<24}  {:<10}  {:>6}  {:<20}  {:<8}  {:<12}  {}{}",
                truncate(&s.game_slug, 24),
                truncate(&s.label, 10),
                ver,
                last,
                state_label,
                group,
                host(&hosted),
                s.local_path
            );
        }
        println!("\n{} save(s) · {}", out.saves.len(), out.state_file);
    })
}
