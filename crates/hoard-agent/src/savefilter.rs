//! Which game this folder belongs to, and therefore which files inside it are
//! save data.
//!
//! The *what* is decided by [`hoard_core::kernel::fileclass`], which is pure.
//! What lives here is the part that needs IO and the catalogue: pulling the
//! game's file patterns out of the Ludusavi manifest to hand them over as
//! shields.
//!
//! ## Where the patterns come from
//!
//! 20,499 of the catalogue's 47,404 templates end in a file pattern
//! (`<base>/Saves/*.sav`, `<base>/SavesDir/*.sav`). Hoard already had them in
//! front of it and threw them away: `pathexpand::expand_path_globbed` collapses
//! the pattern to its parent folder and returns only the directory, because what
//! Hoard tracks is the folder. The pattern was lost there, and with it the only
//! reliable source for "what counts as save data in this particular folder".
//!
//! It is recovered here, by slug and against the templates of all three systems:
//! a Windows game running under Proton lives in a Windows-shaped folder, so
//! looking only at the host OS would drop half of them. Since the patterns only
//! ever rescue and never exclude, the superset is the safe choice.
//!
//! A hand-added save, or one for a game outside the catalogue, gets no shields:
//! the kernel's name rules decide alone, which is why they are conservative.

use hoard_core::kernel::fileclass::{is_useful_shield, RestoreGate};

use crate::agent::WatchedSave;
use crate::state::{SaveState, SharedRef};

/// The include list an explicit restore narrows to: a shared save's list for a
/// member, nothing for its owner or a save that is not shared. The owner's own
/// history holds the whole folder (files from before the share, other worlds),
/// and restoring it is not the share's to narrow. Whose save it is is the
/// server's word ([`SharedRef::caller_owns`]), not the account this machine
/// cached, which a login made with no service running leaves blank. Pulls the
/// engine drives keep [`SaveState::gate`].
pub fn restore_include(shared: Option<&SharedRef>) -> &[String] {
    shared.map_or(&[], SharedRef::walk_include)
}

/// The share's list when this account owns the shared save
/// ([`SharedRef::caller_owns`]), nothing otherwise. The owner's restore writes
/// the whole folder: a file outside the list that the restore overwrites is
/// copied aside first ([`crate::restore::keep_outside_share`]), so its current
/// bytes survive whatever version the restore brings back. The other half of
/// [`restore_include`].
pub fn owner_share_include(shared: Option<&SharedRef>) -> &[String] {
    match shared {
        Some(s) if s.caller_owns => &s.include,
        _ => &[],
    }
}

/// The filename patterns the manifest declares as save data for `slug`, in
/// lowercase and deduplicated.
///
/// Only the template's last segment counts, and only when it is a wildcard: a
/// literal segment (`.../Fallout4/Saves`) is the name of the tracked folder
/// rather than a file pattern, and taking it for one would shield a name that
/// does not exist inside.
pub fn shields_for_slug(slug: &str) -> Vec<String> {
    let Some(entry) = hoard_manifest::ludusavi::find_by_slug(slug) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    let all = entry
        .paths
        .windows
        .iter()
        .chain(entry.paths.linux.iter())
        .chain(entry.paths.mac.iter());
    for p in all {
        let Some(last) = p.path.rsplit('/').next() else {
            continue;
        };
        // A literal is not a file pattern: it is the folder being tracked.
        if !last.contains('*') && !last.contains('?') {
            continue;
        }
        if !is_useful_shield(last) {
            continue;
        }
        let lower = last.to_ascii_lowercase();
        if !out.contains(&lower) {
            out.push(lower);
        }
    }
    out
}

/// The gate a restore of one save runs through: the game's shields, what the
/// save consists of when shared, and the user's answer on writing its config.
/// Every restore of a row builds it here, so what the preview promises, what
/// the dialog writes and what auto-restore merges come out of one decision.
pub fn gate_for_save(game_slug: &str, include: &[String], allow_device_local: bool) -> RestoreGate {
    RestoreGate {
        shields: shields_for_slug(game_slug),
        include: include.to_vec(),
        allow_device_local,
    }
}

impl SharedRef {
    /// The shared world: the files members read and push, and the only ones
    /// the hosting lease guards (HRD-D-0019). Empty is the whole folder.
    pub fn world(&self) -> &[String] {
        &self.include
    }
}

impl SaveState {
    /// See [`gate_for_save`].
    pub fn gate(&self, allow_device_local: bool) -> RestoreGate {
        gate_for_save(&self.game_slug, &self.include, allow_device_local)
    }

    /// See [`SharedRef::world`]; nothing for an unshared save. Unlike
    /// [`Self::include`], which is empty for the owner, this is the share's
    /// list for owner and member alike.
    pub fn world(&self) -> &[String] {
        self.shared.as_ref().map_or(&[], SharedRef::world)
    }
}

impl WatchedSave {
    /// See [`gate_for_save`].
    pub fn gate(&self, allow_device_local: bool) -> RestoreGate {
        gate_for_save(&self.game_slug, &self.include, allow_device_local)
    }

    /// See [`SaveState::world`].
    pub fn world(&self) -> &[String] {
        self.shared.as_ref().map_or(&[], SharedRef::world)
    }

    /// The caller owns a share of part of this folder and walks all of it:
    /// its uploads carry the whole folder, and one that leaves the world as
    /// last synced needs no lease. False once the walk was narrowed to the
    /// world for an older server, where the owner pushes like a member.
    pub fn owns_whole_folder(&self) -> bool {
        self.shared.as_ref().is_some_and(|s| s.caller_owns)
            && self.include.is_empty()
            && !self.world().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the owner has files outside the share to keep: a member's restore
    /// never writes there, and an unshared save has no share to be outside of.
    /// The owner is the server's mark, not an account id.
    #[test]
    fn only_the_owner_keeps_files_outside_the_share() {
        let owner = SharedRef {
            group_id: "g1".into(),
            group_name: "friends".into(),
            owner_user_id: "u1".into(),
            owner_username: "alice".into(),
            include: vec!["worlds/One.wld".into()],
            caller_owns: true,
        };
        let member = SharedRef {
            caller_owns: false,
            ..owner.clone()
        };
        assert_eq!(
            owner_share_include(Some(&owner)),
            ["worlds/One.wld".to_string()]
        );
        assert!(owner_share_include(Some(&member)).is_empty());
        assert!(owner_share_include(None).is_empty());
    }

    #[test]
    fn a_restore_narrows_to_the_share_only_for_a_member() {
        let member = SharedRef {
            group_id: "g1".into(),
            group_name: "friends".into(),
            owner_user_id: "u1".into(),
            owner_username: "alice".into(),
            include: vec!["worlds/One.wld".into()],
            caller_owns: false,
        };
        let owner = SharedRef {
            caller_owns: true,
            ..member.clone()
        };
        // The owner restores the whole of their own history, with the share's
        // list still on the row as its world.
        assert!(restore_include(Some(&owner)).is_empty());
        // A member restores the share's files.
        assert_eq!(
            restore_include(Some(&member)),
            ["worlds/One.wld".to_string()]
        );
        // An unshared save has no list for anybody.
        assert!(restore_include(None).is_empty());
    }

    #[test]
    fn a_game_with_file_patterns_yields_them() {
        // Terraria: `<root>/userdata/<storeUserId>/105600/remote/players/*.plr`
        // y `.../worlds/*.wld`.
        let shields = shields_for_slug("terraria");
        assert!(shields.contains(&"*.plr".to_string()), "{shields:?}");
        assert!(shields.contains(&"*.wld".to_string()), "{shields:?}");
    }

    #[test]
    fn a_bare_directory_template_yields_no_shield() {
        // Fallout 4 is `<winDocuments>/My Games/Fallout4/Saves`: the last
        // segment is the folder, not a pattern.
        assert!(shields_for_slug("fallout-4").is_empty());
        // And the game that motivated all of this has none either.
        assert!(shields_for_slug("cell-to-singularity-evolution-never-ends").is_empty());
    }

    #[test]
    fn an_unknown_slug_is_not_an_error() {
        assert!(shields_for_slug("no-existe-este-juego-12345").is_empty());
    }

    /// The Windows patterns count even when we run on Linux: under Proton the
    /// folder is Windows-shaped.
    #[test]
    fn windows_patterns_count_on_every_host() {
        // `<base>/SavesDir/*.sav`, declared only in `paths.windows`.
        let shields = shields_for_slug("singularity-tactics-arena");
        assert!(shields.contains(&"*.sav".to_string()), "{shields:?}");
    }

    /// `*.*` matches everything: it would shield the whole folder and void the
    /// filter.
    #[test]
    fn degenerate_patterns_never_become_shields() {
        for e in hoard_manifest::ludusavi::catalog().iter().take(4000) {
            for s in shields_for_slug(&e.slug) {
                assert!(is_useful_shield(&s), "{} shielded with {s}", e.slug);
            }
        }
    }
}
