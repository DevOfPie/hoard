//! `hoard share` / `hoard unshare`: move a save into a group's namespace and
//! back. The service does the sharing (server row, include list, re-seating
//! the watched folder); the CLI resolves the group and, for a game that shares
//! by world, insists the world is named before asking.

use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use hoard_agent::session;
use hoard_agent::state::CliState;
use hoard_agent::worldfiles;
use hoard_core::ipc::{Payload, Request};

use super::{group, link};
use crate::output;

/// A share as agents and scripts see it.
#[derive(Serialize)]
pub struct ShareOut {
    pub save_id: String,
    pub game_slug: String,
    pub label: String,
    pub group_id: String,
    pub group_name: String,
    pub owner: String,
    /// The world's files, `/`-separated patterns relative to the save folder.
    /// Empty when the whole folder is shared.
    pub include: Vec<String>,
}

/// `<save>` is the save id (UUID) from `hoard saves`; there is no `game/label`
/// form in this CLI yet.
pub async fn share(save_id: String, group: String, world: Option<String>) -> Result<()> {
    if let Some(hint) = needs_world(&save_id, world.as_deref())? {
        return Err(output::err("needs_input", hint));
    }
    let mut client = link::require("share").await?;
    let group_id = group::resolve(&mut client, &group).await?;
    let save = match link::ask(
        &mut client,
        Request::ShareSave {
            save_id: save_id.clone(),
            group_id,
            world,
        },
    )
    .await?
    {
        Payload::Save(save) => save,
        other => anyhow::bail!("unexpected answer to a share request: {other:?}"),
    };
    let Some(shared) = save.shared else {
        anyhow::bail!("the service shared {save_id} but the row came back unshared");
    };
    let out = ShareOut {
        save_id: save.id.to_string(),
        game_slug: save.game_slug.to_string(),
        label: save.label,
        group_id: shared.group_id,
        group_name: shared.group_name,
        owner: shared.owner_username.to_string(),
        include: shared.include,
    };
    output::emit(&out, |o| {
        println!(
            "shared {}/{} into '{}' (owner {})",
            o.game_slug, o.label, o.group_name, o.owner
        );
        if o.include.is_empty() {
            println!("members pull the whole folder");
        } else {
            println!("members pull: {}", o.include.join(", "));
        }
    })
}

pub async fn unshare(save_id: String) -> Result<()> {
    let mut client = link::require("share").await?;
    link::ask(
        &mut client,
        Request::UnshareSave {
            save_id: save_id.clone(),
        },
    )
    .await?;
    println!("unshared {save_id}: back in your own namespace");
    Ok(())
}

/// The refusal when a game that shares by world is asked to share without one.
/// Only a save tracked on this machine can be checked: the world list comes
/// from its folder. `None` means go ahead.
fn needs_world(save_id: &str, world: Option<&str>) -> Result<Option<String>> {
    if world.is_some() {
        return Ok(None);
    }
    session::set_context_offline();
    let (state, _) = CliState::load_default()?;
    Ok(state
        .saves
        .get(save_id)
        .and_then(|s| world_hint(&s.game_slug, &s.local_path)))
}

/// For a game with a world template: the worlds found under `root`, as the
/// `--world` hint. `None` for a game that shares whole.
pub fn world_hint(game_slug: &str, root: &Path) -> Option<String> {
    if !worldfiles::has_template(game_slug) {
        return None;
    }
    let worlds = worldfiles::worlds(game_slug, root);
    Some(if worlds.is_empty() {
        format!(
            "{game_slug} shares one world at a time, and none was found under {}; \
             pass `--world <name>` once the game has created one",
            root.display()
        )
    } else {
        format!(
            "{game_slug} shares one world at a time; pass `--world <name>`, one of: {}",
            worlds.join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::world_hint;

    #[test]
    fn a_game_without_a_template_needs_no_world() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(world_hint("stardew-valley", dir.path()), None);
    }

    #[test]
    fn a_template_game_lists_its_worlds() {
        let dir = tempfile::tempdir().unwrap();
        let worlds = dir.path().join("worlds_local");
        std::fs::create_dir_all(&worlds).unwrap();
        for f in [
            "Beta.fwl",
            "Beta.db",
            "Alpha.fwl",
            "Alpha_backup_auto-1.fwl",
        ] {
            std::fs::write(worlds.join(f), b"").unwrap();
        }
        let hint = world_hint("valheim", dir.path()).unwrap();
        assert!(hint.ends_with("one of: Alpha, Beta"), "{hint}");
        assert!(hint.contains("--world"), "{hint}");
    }

    #[test]
    fn a_template_game_with_no_worlds_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let hint = world_hint("valheim", dir.path()).unwrap();
        assert!(hint.contains("none was found"), "{hint}");
        assert!(hint.contains("--world"), "{hint}");
    }
}
