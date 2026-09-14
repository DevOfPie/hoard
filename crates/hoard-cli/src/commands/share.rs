//! `hoard share` / `hoard unshare`: move a save into a group's namespace and
//! back. The service does the sharing (server row, include list, re-seating
//! the watched folder, refusing a game that shares by world with none named);
//! the CLI resolves the group.

use anyhow::Result;
use serde::Serialize;

use hoard_core::ipc::{IpcError, Payload, Request};

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
    let mut client = link::require("share").await?;
    let group_id = group::resolve(&mut client, &group).await?;
    let answer = link::ask(
        &mut client,
        Request::ShareSave {
            save_id: save_id.clone(),
            group_id,
            world,
        },
    )
    .await
    .map_err(name_the_flag)?;
    let save = match answer {
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

/// The engine names the worlds when a template game is shared without one;
/// the flag that picks one is the CLI's to name.
fn name_the_flag(err: anyhow::Error) -> anyhow::Error {
    match err.downcast_ref::<IpcError>() {
        Some(IpcError::Refused { code, .. }) if code == "needs_input" => {
            err.context("pass `--world <name>`")
        }
        _ => err,
    }
}
