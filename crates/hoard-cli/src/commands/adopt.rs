//! `hoard adopt <SAVE_ID> --path <FOLDER>`: gives a save that exists on the
//! server but not on this machine (a world shared with you, or your own save
//! from another machine) a folder here. The headless twin of the desktop's
//! `adopt_save`: the same `library::adopt`, fed the server row's slug and
//! label, then the same `Reload` so the service starts watching the folder.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use hoard_agent::api::{ApiClient, ApiError};
use hoard_agent::library::{self, AdoptArgs};
use hoard_agent::session;
use hoard_agent::state::CliState;
use hoard_core::ids::SaveId;

use super::link;
use super::tracked::SaveRow;
use crate::output;

pub struct Args {
    /// Save id (UUID) of the server's row.
    pub save_id: String,
    /// The folder on this machine. Relative paths are taken from here.
    pub path: PathBuf,
}

/// An adopt as agents and scripts see it.
#[derive(Serialize)]
pub struct AdoptOut {
    /// The row `hoard saves` now lists for it.
    pub save: SaveRow,
    /// A running service picked it up. `false` means there is none, and the
    /// save is watched when it starts.
    pub watching: bool,
}

pub async fn run(args: Args) -> Result<()> {
    // What needs no server is refused first, against this account's state as
    // `hoard saves` reads it.
    session::set_context_offline();
    let (state, _) = CliState::load_default()?;
    let folder = check(&state, &args.save_id, &args.path)?;

    // The Cloud token comes on loan from the service, as in `hoard track`.
    let active = link::resolve_session().await?;
    let (game_slug, label) = server_row(&active.client, &args.save_id).await?;
    let outcome = library::adopt(
        &active.client,
        AdoptArgs {
            save_id: args.save_id.clone(),
            game_slug,
            label,
            local_path: folder.to_string_lossy().into_owned(),
        },
    )
    .await?;
    // The service owns the watched set: it is told to re-read it, as the
    // desktop does after the same call.
    let watching = link::reload().await;

    let t = outcome.tracked;
    let out = AdoptOut {
        save: SaveRow {
            save_id: t.save_id,
            game_slug: t.game_slug,
            label: t.label,
            local_path: t.local_path,
            paused: t.paused,
            last_version_num: t.local_version_num,
            last_backup_at: t.last_backup_at,
            preset: t.preset,
            group: t.shared.map(|s| s.group_name),
            hosted: None,
            lease: None,
        },
        watching,
    };
    output::emit(&out, |o| {
        println!(
            "adopted {}/{} ({}) into {}; {}",
            o.save.game_slug,
            o.save.label,
            o.save.save_id,
            o.save.local_path,
            if o.watching {
                "the sync service picked it up"
            } else {
                "the sync service picks it up when it starts"
            }
        );
    })
}

/// The folder `path` names, made absolute, or the refusal when `save_id`
/// cannot be adopted there. Local only: the server is asked afterwards.
fn check(state: &CliState, save_id: &str, path: &Path) -> Result<PathBuf> {
    SaveId::parse(save_id).map_err(|e| {
        output::err(
            "bad_request",
            format!("{e}; the save id is the UUID `hoard save list` shows"),
        )
    })?;
    if let Some(row) = state.saves.get(save_id) {
        return Err(output::err(
            "already_tracked",
            format!(
                "{save_id} is already tracked on this machine, in {}; \
                 `hoard save path {save_id} <folder>` moves it",
                row.local_path.display()
            ),
        ));
    }
    let folder = std::path::absolute(path).map_err(|e| {
        output::err(
            "bad_request",
            format!("`{}` is not a usable folder: {e}", path.display()),
        )
    })?;
    if folder.exists() && !folder.is_dir() {
        return Err(output::err(
            "bad_request",
            format!("{} is not a folder", folder.display()),
        ));
    }
    library::validate_path_shape(&folder)
        .map_err(|e| output::err("bad_request", format!("{e:#}")))?;
    Ok(folder)
}

/// The server's slug and label for `save_id`, which the adopt records as they
/// are. Cloud reads them from the sync manifest, where the desktop's library
/// finds a save with no folder here; a self-hosted server answers for the row.
async fn server_row(client: &ApiClient, save_id: &str) -> Result<(String, String)> {
    let unknown = || format!("the server has no save {save_id} this account can reach");
    if client.is_cloud().await {
        let manifest = client.cloud_sync().await?;
        return manifest
            .saves
            .into_iter()
            .find(|e| e.save_id == save_id)
            .map(|e| (e.game_slug, e.label))
            .ok_or_else(|| anyhow::Error::new(ApiError::NotFound).context(unknown()));
    }
    match client.get_save(save_id).await {
        Ok(save) => Ok((save.game_slug.to_string(), save.label)),
        Err(e) if matches!(e.downcast_ref::<ApiError>(), Some(ApiError::NotFound)) => {
            Err(e.context(unknown()))
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hoard_agent::state::SaveState;

    const ID: &str = "0f8a5c2e-3b1d-4e6f-9a7b-1c2d3e4f5a6b";

    fn code(e: &anyhow::Error) -> String {
        output::classify(e).code.into_owned()
    }

    fn tracking(id: &str, path: &Path) -> CliState {
        let mut state = CliState::default();
        state.saves.insert(
            id.to_string(),
            SaveState {
                local_path: path.to_path_buf(),
                game_slug: "valheim".into(),
                label: "main".into(),
                last_backup_at: None,
                last_version_num: None,
                paused: false,
                preset: None,
                set_hash: None,
                processes: Vec::new(),
                shared_processes: false,
                allow_device_local: None,
                shared: None,
                include: Vec::new(),
            },
        );
        state
    }

    #[test]
    fn a_folder_is_taken_as_an_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("valheim");
        let got = check(&CliState::default(), ID, &folder).unwrap();
        assert_eq!(got, folder);
        let relative = check(&CliState::default(), ID, Path::new("worlds")).unwrap();
        assert!(relative.is_absolute(), "{}", relative.display());
        assert!(relative.ends_with("worlds"));
    }

    #[test]
    fn a_save_tracked_here_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let state = tracking(ID, dir.path());
        let err = check(&state, ID, &dir.path().join("elsewhere")).unwrap_err();
        assert_eq!(code(&err), "already_tracked");
        assert!(format!("{err:#}").contains("hoard save path"), "{err:#}");
    }

    #[test]
    fn an_id_that_is_not_a_uuid_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let err = check(&CliState::default(), "valheim", dir.path()).unwrap_err();
        assert_eq!(code(&err), "bad_request");
    }

    #[test]
    fn a_file_or_an_empty_path_is_not_a_folder() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("world.fwl");
        std::fs::write(&file, b"").unwrap();
        let err = check(&CliState::default(), ID, &file).unwrap_err();
        assert_eq!(code(&err), "bad_request");
        assert!(format!("{err:#}").contains("not a folder"), "{err:#}");
        let err = check(&CliState::default(), ID, Path::new("")).unwrap_err();
        assert_eq!(code(&err), "bad_request");
    }

    /// The engine's own refusals (a filesystem root, Hoard's data folder) come
    /// through as `bad_request` too.
    #[test]
    fn a_dangerous_folder_is_refused() {
        let err = check(&CliState::default(), ID, Path::new("/")).unwrap_err();
        assert_eq!(code(&err), "bad_request");
    }
}
