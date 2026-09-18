//! What a shared save consists of, per game.
//!
//! A shared save is one world, not the whole folder: Valheim keeps every world
//! and every character of the account under one `IronGate/Valheim` root, and a
//! group shares a world while each member keeps their own character. The
//! template below names that world's files and folder as `/`-separated
//! patterns relative to the save root (a pattern naming a folder covers
//! everything in it), and those patterns travel with the share so every member
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
/// Valheim: the world's folder, `worlds_local/<W>/`, where 1.0 keeps a world
/// as `_main.<N>.*` generation files and `*.chunk` files whose names change on
/// every save, so the folder is named rather than its files. Beside it the
/// legacy flat layout: the `.db` and `.fwl`, the `.old` twins (the game's
/// rotation before 1.0, and what 1.0 leaves behind when it converts a world),
/// and the `_backup_*` copies, files before 1.0 and folders since. Both
/// layouts stay in one list so a share made before the conversion still covers
/// the world after it. Nothing under `characters_local/`: a character is the
/// player's, not the world's.
pub fn template(game_slug: &str, world: &str) -> Result<Option<Vec<String>>, BadWorld> {
    if world.is_empty() || world.contains(['/', '\\', '*', '?']) || world.contains("..") {
        return Err(BadWorld(world.to_string()));
    }
    Ok(match game_slug {
        VALHEIM => Some(
            ["", ".db", ".fwl", ".db.old", ".fwl.old", "_backup_*"]
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

/// The worlds found under `root`, sorted. Valheim: the stems of the legacy
/// `worlds_local/*.fwl`, the file that names a world (a `.db` on its own is a
/// leftover, not a world), and the 1.0 folders `worlds_local/<W>/` holding a
/// `_main.<N>.fwl2`, the same file in the new layout. The game's own
/// `<W>_backup_*` copies, files or folders, carry one too and belong to the
/// world they are named after. A world found in both layouts, mid-conversion,
/// is listed once. Empty for a game with no template or a root with no
/// worlds.
pub fn worlds(game_slug: &str, root: &Path) -> Vec<String> {
    if game_slug != VALHEIM {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(root.join("worlds_local")) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let kind = e.file_type().ok()?;
            let name = e.file_name().into_string().ok()?;
            if kind.is_file() {
                name.strip_suffix(".fwl").map(str::to_string)
            } else if kind.is_dir() && holds_a_generation(&e.path()) {
                Some(name)
            } else {
                None
            }
        })
        .filter(|stem| !stem.is_empty() && !stem.contains("_backup_"))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Does this folder hold a Valheim 1.0 world, a `_main.<N>.fwl2` file?
fn holds_a_generation(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(|e| e.ok()).any(|e| {
        e.file_type().map(|t| t.is_file()).unwrap_or(false)
            && e.file_name().to_str().is_some_and(|n| {
                n.strip_prefix("_main.")
                    .and_then(|n| n.strip_suffix(".fwl2"))
                    .is_some_and(|generation| !generation.is_empty())
            })
    })
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
                "worlds_local/Alpha",
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

    /// A Valheim 1.0 root: `Alpha` converted at first load (its folder of
    /// generation and chunk files, the `.old` twins of its legacy files and a
    /// `_backup_` folder), `Gamma` made in 1.0, `Beta` a legacy flat world
    /// still waiting to be opened, and a folder with no generation in it.
    fn valheim_1_0_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for f in [
            "worlds_local/Alpha/_main.4.fwl2",
            "worlds_local/Alpha/_main.4.db2",
            "worlds_local/Alpha/_main.4.chunks",
            "worlds_local/Alpha/_main.4.ok",
            "worlds_local/Alpha/0_0.chunk",
            "worlds_local/Alpha/-1_2.chunk",
            "worlds_local/Alpha.db.old",
            "worlds_local/Alpha.fwl.old",
            "worlds_local/Alpha_backup_auto-20260915/_main.2.fwl2",
            "worlds_local/Alpha_backup_auto-20260915/_main.2.db2",
            "worlds_local/Alpha_backup_auto-20260915/0_0.chunk",
            "worlds_local/Beta.db",
            "worlds_local/Beta.fwl",
            "worlds_local/Gamma/_main.1.fwl2",
            "worlds_local/Gamma/_main.1.db2",
            "worlds_local/Gamma/0_0.chunk",
            // A folder with no generation: not a world.
            "worlds_local/Scratch/notes.txt",
            "characters_local/Me.fch",
            "characters_local/Alpha.fch",
        ] {
            let path = root.join(f);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, f).unwrap();
        }
        dir
    }

    #[test]
    fn valheim_1_0_worlds_are_listed_beside_the_legacy_ones() {
        let dir = valheim_1_0_root();
        assert_eq!(
            worlds("valheim", dir.path()),
            vec!["Alpha", "Beta", "Gamma"]
        );
        // Mid-conversion both layouts name `Alpha`: it is listed once.
        std::fs::write(dir.path().join("worlds_local/Alpha.fwl"), b"fwl").unwrap();
        assert_eq!(
            worlds("valheim", dir.path()),
            vec!["Alpha", "Beta", "Gamma"]
        );
    }

    /// The same template covers a converted world: its folder, nested files
    /// and all, its `_backup_` folder and its `.old` twins, and nothing of
    /// another world's folder or of any character. Checked with the walk the
    /// backup uses, which is what descends into the folders.
    #[test]
    fn the_valheim_template_covers_a_1_0_world_folder() {
        use hoard_core::kernel::fileclass::{reaches_beneath, Scope};
        let dir = valheim_1_0_root();
        let list = template("valheim", "Alpha").unwrap().unwrap();
        let kept: Vec<String> = crate::backup::walk_source(
            dir.path(),
            Scope {
                shields: &[],
                include: &list,
            },
        )
        .unwrap()
        .into_iter()
        .map(|f| f.relative_path)
        .collect();
        assert_eq!(
            kept,
            vec![
                "worlds_local/Alpha.db.old",
                "worlds_local/Alpha.fwl.old",
                "worlds_local/Alpha/-1_2.chunk",
                "worlds_local/Alpha/0_0.chunk",
                "worlds_local/Alpha/_main.4.chunks",
                "worlds_local/Alpha/_main.4.db2",
                "worlds_local/Alpha/_main.4.fwl2",
                "worlds_local/Alpha/_main.4.ok",
                "worlds_local/Alpha_backup_auto-20260915/0_0.chunk",
                "worlds_local/Alpha_backup_auto-20260915/_main.2.db2",
                "worlds_local/Alpha_backup_auto-20260915/_main.2.fwl2",
            ]
        );
        // The next generation, under a name no one has seen yet, is covered.
        assert!(included(&list, "worlds_local/Alpha/_main.5.db2"));
        assert!(!included(&list, "worlds_local/Gamma/_main.1.db2"));
        assert!(!included(&list, "worlds_local/Alpha2/_main.1.db2"));
        assert!(!reaches_beneath(&list, "worlds_local/Gamma"));
        assert!(!reaches_beneath(&list, "characters_local"));
        assert!(reaches_beneath(&list, "worlds_local/Alpha"));
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
