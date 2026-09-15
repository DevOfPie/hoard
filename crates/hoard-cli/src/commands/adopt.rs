//! `hoard adopt <SAVE_ID> --path <FOLDER>`: gives a save that exists on the
//! server but not on this machine (a world shared with you, or your own save
//! from another machine) a folder here. The headless twin of the desktop's
//! `adopt_save`: the same `library::adopt`, fed the server row's slug and
//! label, then the same `Reload` so the service starts watching the folder.

use std::io;
use std::path::{Component, Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use hoard_agent::api::{ApiClient, ApiError};
use hoard_agent::library::{self, AdoptArgs, AdoptRefusal};
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
    // What needs neither a session nor the server is refused first.
    let folder = check(&args.save_id, &args.path)?;

    // The Cloud token comes on loan from the service, as in `hoard track`. The
    // session also picks the account whose state the adopt writes, so a save
    // already tracked is looked for there, before the server is asked.
    let active = link::resolve_session().await?;
    let (state, _) = CliState::load_default()?;
    if let Some(row) = state.saves.get(&args.save_id) {
        return Err(refused(AdoptRefusal::AlreadyTracked {
            save_id: args.save_id.clone(),
            local_path: row.local_path.clone(),
        }));
    }
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
    .await
    .map_err(coded)?;
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

/// The folder `path` names, resolved as the adopt stores it, or the refusal
/// when `save_id` cannot be adopted there. Local only: the session and the
/// server come afterwards.
fn check(save_id: &str, path: &Path) -> Result<PathBuf> {
    SaveId::parse(save_id).map_err(|e| {
        output::err(
            "bad_request",
            format!("{e}; the save id is the UUID `hoard save list` shows"),
        )
    })?;
    let folder = resolve(path).map_err(|e| {
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

/// `path` as the folder it really is: absolute, without `.` and `..`, and
/// through any link on the part of it that exists. Every check and the stored
/// row see this form only. Compared as text, `/home/u/games/..` is neither a
/// home folder nor a parent of `/home/u/games/valheim`, and a link into a
/// tracked save is not inside it.
fn resolve(path: &Path) -> io::Result<PathBuf> {
    // `absolute` keeps `..` on Unix. It is taken out as text: it means the
    // folder above the one the user typed.
    let mut folder = PathBuf::new();
    for part in std::path::absolute(path)?.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                folder.pop();
            }
            part => folder.push(part),
        }
    }
    // The longest part that exists, and whether a link is on the way to it.
    let parts: Vec<Component> = folder.components().collect();
    let mut prefix = PathBuf::new();
    let mut existing = 0;
    let mut linked = false;
    for part in &parts {
        prefix.push(part);
        let Ok(meta) = std::fs::symlink_metadata(&prefix) else {
            break;
        };
        if !prefix.exists() {
            // A link to nothing: what is beyond it does not exist either.
            break;
        }
        linked |= meta.file_type().is_symlink();
        existing += 1;
    }
    if !linked {
        return Ok(folder);
    }
    let mut resolved = std::fs::canonicalize(parts[..existing].iter().collect::<PathBuf>())?;
    resolved.extend(&parts[existing..]);
    Ok(without_verbatim(resolved))
}

/// `canonicalize` spells a Windows path `\\?\C:\…`, where no check here finds
/// a drive. The plain spelling names the same folder.
fn without_verbatim(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(rest) = path.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
            if !rest.starts_with(r"UNC\") {
                return PathBuf::from(rest);
            }
        }
    }
    path
}

/// The engine's refusals as the codes SKILL.md gives them. Anything else, a
/// server or a disk failure, keeps its own.
fn coded(e: anyhow::Error) -> anyhow::Error {
    match e.downcast::<AdoptRefusal>() {
        Ok(refusal) => refused(refusal),
        Err(e) => e,
    }
}

fn refused(refusal: AdoptRefusal) -> anyhow::Error {
    match refusal {
        AdoptRefusal::AlreadyTracked {
            save_id,
            local_path,
        } => output::err(
            "already_tracked",
            format!(
                "{save_id} is already tracked on this machine, in {}; \
                 `hoard save path {save_id} <folder>` moves it",
                local_path.display()
            ),
        ),
        AdoptRefusal::Invalid(message) => output::err("bad_request", message),
    }
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
    const OTHER: &str = "7d1e2f3a-4b5c-4d6e-8f9a-0b1c2d3e4f5a";

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

    /// What `library::adopt` answers for `folder` against `state` before the
    /// server is asked, coded as the command codes it.
    fn adopt_refusal(state: &CliState, save_id: &str, folder: &Path) -> Option<anyhow::Error> {
        let args = AdoptArgs {
            save_id: save_id.into(),
            game_slug: "factorio".into(),
            label: "main".into(),
            local_path: folder.to_string_lossy().into_owned(),
        };
        library::plan_adopt(state, &args)
            .err()
            .map(|refusal| coded(refusal.into()))
    }

    /// A temporary folder as `resolve` spells it (on macOS `/var` is a link).
    fn base(dir: &tempfile::TempDir) -> PathBuf {
        resolve(dir.path()).unwrap()
    }

    #[test]
    fn a_folder_is_taken_as_an_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let folder = base(&dir).join("valheim");
        let got = check(ID, &folder).unwrap();
        assert_eq!(got, folder);
        let relative = check(ID, Path::new("worlds")).unwrap();
        assert!(relative.is_absolute(), "{}", relative.display());
        assert!(relative.ends_with("worlds"));
    }

    /// Refused on the state `library::adopt` writes, which is the one the
    /// session selected.
    #[test]
    fn a_save_tracked_here_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let state = tracking(ID, dir.path());
        let err = adopt_refusal(&state, ID, &dir.path().join("elsewhere")).unwrap();
        assert_eq!(code(&err), "already_tracked");
        assert!(format!("{err:#}").contains("hoard save path"), "{err:#}");
    }

    #[test]
    fn an_id_that_is_not_a_uuid_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let err = check("valheim", dir.path()).unwrap_err();
        assert_eq!(code(&err), "bad_request");
    }

    #[test]
    fn a_file_or_an_empty_path_is_not_a_folder() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("world.fwl");
        std::fs::write(&file, b"").unwrap();
        let err = check(ID, &file).unwrap_err();
        assert_eq!(code(&err), "bad_request");
        assert!(format!("{err:#}").contains("not a folder"), "{err:#}");
        let err = check(ID, Path::new("")).unwrap_err();
        assert_eq!(code(&err), "bad_request");
    }

    /// The engine's own refusals (a filesystem root, Hoard's data folder) come
    /// through as `bad_request` too.
    #[test]
    fn a_dangerous_folder_is_refused() {
        let err = check(ID, Path::new("/")).unwrap_err();
        assert_eq!(code(&err), "bad_request");
    }

    #[test]
    fn dot_dot_is_taken_out_before_anything_is_stored() {
        let dir = tempfile::tempdir().unwrap();
        let base = base(&dir);
        assert_eq!(check(ID, &base.join("a/../b")).unwrap(), base.join("b"));
        assert_eq!(check(ID, &base.join("./a/./b/..")).unwrap(), base.join("a"));
        let relative = check(ID, Path::new("a/../b")).unwrap();
        assert_eq!(relative, std::env::current_dir().unwrap().join("b"));
    }

    /// `..` from a folder next to a tracked save names the folder holding it,
    /// and that is refused.
    #[test]
    fn dot_dot_from_beside_a_tracked_save_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let games = base(&dir).join("games");
        std::fs::create_dir_all(games.join("valheim")).unwrap();
        std::fs::create_dir_all(games.join("factorio")).unwrap();
        let state = tracking(OTHER, &games.join("valheim"));

        let folder = check(ID, &games.join("factorio/..")).unwrap();
        assert_eq!(folder, games);
        let err = adopt_refusal(&state, ID, &folder).unwrap();
        assert_eq!(code(&err), "bad_request");
        assert!(format!("{err:#}").contains("inside this folder"), "{err:#}");
    }

    /// The home folder spelled with `..` is still the home folder.
    #[cfg(unix)]
    #[test]
    fn dot_dot_up_to_a_home_folder_is_refused() {
        let err = check(ID, Path::new("/home/u/games/..")).unwrap_err();
        assert_eq!(code(&err), "bad_request");
        assert!(format!("{err:#}").contains("/home/u"), "{err:#}");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_into_a_tracked_save_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let valheim = base(&dir).join("games/valheim");
        std::fs::create_dir_all(&valheim).unwrap();
        let state = tracking(OTHER, &valheim);
        let link = base(&dir).join("shortcut");
        std::os::unix::fs::symlink(&valheim, &link).unwrap();

        let folder = check(ID, &link.join("worlds")).unwrap();
        assert_eq!(folder, valheim.join("worlds"));
        let err = adopt_refusal(&state, ID, &folder).unwrap();
        assert_eq!(code(&err), "bad_request");
    }

    /// The engine's folder and slug refusals are the request's fault.
    #[test]
    fn a_folder_the_engine_refuses_is_a_bad_request() {
        let err = coded(AdoptRefusal::Invalid("'valheim' already tracks it".into()).into());
        assert_eq!(code(&err), "bad_request");
        assert_eq!(format!("{err:#}"), "'valheim' already tracks it");
        // Anything else keeps its own code.
        let err = coded(anyhow::anyhow!("disk full"));
        assert_eq!(code(&err), "error");
    }
}
