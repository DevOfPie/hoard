//! What a shared save consists of, per game.
//!
//! A shared save is one world, not the whole folder: Valheim keeps every world
//! and every character of the account under one `IronGate/Valheim` root, and a
//! group shares a world while each member keeps their own character. The
//! template below names that world's files as `/`-separated patterns relative
//! to the save root, and those patterns travel with the share so every member
//! walks the same files (`fileclass::included`).
//!
//! The world name is inserted literally, so it has to be a bare file stem:
//! anything that could leave the folder or widen the match is refused with
//! [`BadWorld`] before it reaches the server.

use std::path::Path;

/// The game whose world layout is known. Others share the whole folder.
const VALHEIM: &str = "valheim";

/// A world name that is not a bare file stem.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a world name: no `/`, `\\`, `*`, `?` or `..`, and not empty")]
pub struct BadWorld(pub String);

/// The include list for `world` of `game_slug`, or `None` when the game has no
/// known world layout (the whole folder is what gets shared then).
///
/// Valheim: the world's `.db` and `.fwl`, the `.old` twins the game rotates on
/// every save, and the `_backup_*` copies it keeps beside them. Nothing under
/// `characters_local/`: a character is the player's, not the world's.
pub fn template(game_slug: &str, world: &str) -> Result<Option<Vec<String>>, BadWorld> {
    if world.is_empty() || world.contains(['/', '\\', '*', '?']) || world.contains("..") {
        return Err(BadWorld(world.to_string()));
    }
    Ok(match game_slug {
        VALHEIM => Some(
            [".db", ".fwl", ".db.old", ".fwl.old", "_backup_*"]
                .iter()
                .map(|suffix| format!("worlds_local/{world}{suffix}"))
                .collect(),
        ),
        _ => None,
    })
}

/// Does this game share by world at all? `template` answers `None` for the
/// rest; this is the question to ask before offering a picker.
pub fn has_template(game_slug: &str) -> bool {
    game_slug == VALHEIM
}

/// The worlds found under `root`, sorted. Valheim: the stems of
/// `worlds_local/*.fwl`, the file that names a world (a `.db` on its own is a
/// leftover, not a world), minus the game's own `<W>_backup_*` copies, which
/// carry a `.fwl` too and belong to the world they are named after. Empty for
/// a game with no template or a root with no worlds.
pub fn worlds(game_slug: &str, root: &Path) -> Vec<String> {
    if game_slug != VALHEIM {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(root.join("worlds_local")) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| {
            let name = e.file_name();
            let name = name.to_str()?;
            name.strip_suffix(".fwl").map(str::to_string)
        })
        .filter(|stem| !stem.is_empty() && !stem.contains("_backup_"))
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use hoard_core::kernel::fileclass::included;

    fn valheim_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let worlds = dir.path().join("worlds_local");
        let chars = dir.path().join("characters_local");
        std::fs::create_dir_all(&worlds).unwrap();
        std::fs::create_dir_all(&chars).unwrap();
        for f in [
            "Alpha.db",
            "Alpha.fwl",
            "Alpha.db.old",
            "Alpha.fwl.old",
            "Alpha_backup_auto-20260913.db",
            "Alpha_backup_auto-20260913.fwl",
            "Beta.db",
            "Beta.fwl",
            // A `.db` with no `.fwl`: a leftover, not a world.
            "Gamma.db",
        ] {
            std::fs::write(worlds.join(f), f).unwrap();
        }
        for f in ["Me.fch", "Me.fch.old", "Alpha.fch"] {
            std::fs::write(chars.join(f), f).unwrap();
        }
        dir
    }

    #[test]
    fn valheim_worlds_are_the_fwl_stems_sorted() {
        let dir = valheim_root();
        assert_eq!(worlds("valheim", dir.path()), vec!["Alpha", "Beta"]);
        assert!(worlds("stardew-valley", dir.path()).is_empty());
        assert!(worlds("valheim", &dir.path().join("nowhere")).is_empty());
    }

    /// The template names one world's files and nothing of the other world or
    /// of any character, checked with the matcher the walk uses.
    #[test]
    fn the_valheim_template_selects_one_world_and_no_character() {
        let dir = valheim_root();
        let list = template("valheim", "Alpha").unwrap().unwrap();
        assert_eq!(
            list,
            vec![
                "worlds_local/Alpha.db",
                "worlds_local/Alpha.fwl",
                "worlds_local/Alpha.db.old",
                "worlds_local/Alpha.fwl.old",
                "worlds_local/Alpha_backup_*",
            ]
        );
        let mut kept: Vec<String> = Vec::new();
        for sub in ["worlds_local", "characters_local"] {
            for e in std::fs::read_dir(dir.path().join(sub)).unwrap() {
                let rel = format!("{sub}/{}", e.unwrap().file_name().to_str().unwrap());
                if included(&list, &rel) {
                    kept.push(rel);
                }
            }
        }
        kept.sort();
        assert_eq!(
            kept,
            vec![
                "worlds_local/Alpha.db",
                "worlds_local/Alpha.db.old",
                "worlds_local/Alpha.fwl",
                "worlds_local/Alpha.fwl.old",
                "worlds_local/Alpha_backup_auto-20260913.db",
                "worlds_local/Alpha_backup_auto-20260913.fwl",
            ]
        );
    }

    #[test]
    fn other_games_have_no_template() {
        assert_eq!(template("stardew-valley", "Alpha"), Ok(None));
        assert!(!has_template("stardew-valley"));
        assert!(has_template("valheim"));
    }

    /// A name that is not a file stem is refused for every game, template or
    /// not: the check is on the name, so a caller can 400 before looking up
    /// the game.
    #[test]
    fn a_world_name_that_is_not_a_stem_is_refused() {
        for bad in ["", "a/b", "a\\b", "*", "Alpha?", "..", "x..y", "../Alpha"] {
            assert_eq!(
                template("valheim", bad),
                Err(BadWorld(bad.to_string())),
                "{bad:?}"
            );
            assert!(template("stardew-valley", bad).is_err(), "{bad:?}");
        }
        assert!(template("valheim", "Alpha World 2").is_ok());
    }
}
