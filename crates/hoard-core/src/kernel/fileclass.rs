//! What each file inside a save folder actually is.
//!
//! A save folder almost never holds only saves. `walk_source` used to take every
//! regular file it found, and that drags into the snapshot things that are not
//! the player's data but *this machine's*: a Unity `Player.log`, the analytics
//! queue carrying the install GUID, this GPU's shader info,
//! `steam_autocloud.vdf`, a `graphics.ini` with this monitor's resolution.
//!
//! Two separate kinds of damage:
//!
//! * Noise. The log is rewritten on every launch, so the cheap signature moves,
//!   the content signature confirms the bytes really did change (they did, it is
//!   a log), and a new cloud version gets cut every single time the game opens
//!   without the save being touched.
//! * Crashes. Restoring PC A's `graphics.ini` onto PC B hands the game a
//!   resolution, a GPU or a path that does not exist on that machine.
//!
//! ## The ladder, least to most destructive
//!
//! [`FileClass::Junk`] is the only thing that stops being uploaded, which is why
//! the list is short and matches exact names wherever it can: a file that is not
//! uploaded cannot be recovered, so doubt never lands here.
//!
//! [`FileClass::DeviceLocal`] is where doubt lands. It does get uploaded (if the
//! disk burns, it is there), but a restore will not write it unless the user
//! asks by hand (`--allow-ini` on the CLI, a switch that is off by default in
//! the desktop dialog). That way the most expensive misclassification possible,
//! calling config something that was the save, costs a click rather than the
//! save.
//!
//! ## Shields from the manifest
//!
//! The catalogue carries a file pattern in 20,499 of its 47,404 templates
//! (`<base>/Saves/*.sav`), and there the community does know what save data is.
//! That pattern arrives here as `shields`: whatever matches one is save data and
//! no rule below touches it. It is genuinely needed, because `.ini` is the save
//! pattern of 582 templates, `.cfg` of 98 and `.log` of 64, so without shields
//! the extension rules would mow down real saves.
//!
//! The reverse is not used: a file the manifest does not list is not thereby
//! condemned. The catalogue has enormous holes (one game is a bare directory
//! with not a single pattern), and trusting it to exclude would mean trusting a
//! hole to delete.

/// What a file inside the save folder is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileClass {
    /// The player's data. Uploaded and restored, as always.
    SaveData,
    /// This machine's data rather than the player's: config, settings, generic
    /// logs. Uploaded so it is never lost, but a restore will not write it
    /// without an explicit request.
    DeviceLocal,
    /// Neither the player's data nor config anyone wants back: OS litter,
    /// temporaries, crash dumps, engine telemetry. Never uploaded, never
    /// restored.
    Junk,
}

impl FileClass {
    /// Does it go into a new snapshot?
    pub fn is_backed_up(self) -> bool {
        !matches!(self, FileClass::Junk)
    }

    /// Does a restore write it to disk? `allow_device_local` is the switch the
    /// user turns on by hand.
    pub fn is_restored(self, allow_device_local: bool) -> bool {
        match self {
            FileClass::SaveData => true,
            FileClass::DeviceLocal => allow_device_local,
            FileClass::Junk => false,
        }
    }
}

/// OS and file-manager litter, by exact name.
const JUNK_NAMES: &[&str] = &[
    ".ds_store",
    "thumbs.db",
    "ehthumbs.db",
    "desktop.ini",
    ".directory",
    // Steam's own bookkeeping, not the game's: which files it had to sync and
    // when. Restoring it on another machine lies to the Steam client.
    "steam_autocloud.vdf",
    "remotecache.vdf",
    // Engine logs by exact name. Generic `*.log` does not land here but in
    // `DeviceLocal`, because `.log` is the save pattern of 64 catalogue
    // templates and not all of them are shielded.
    "player.log",
    "player-prev.log",
    "output_log.txt",
    "output_log_prev.txt",
    // A lock the game holds open exclusively while it runs. It carries no data,
    // and on Windows it cannot even be opened for reading with the game alive
    // (sharing violation, os error 32), which used to abort the whole backup
    // halfway through the walk.
    "session.lock",
];

/// Extensions that are never save data.
const JUNK_EXTS: &[&str] = &[
    // Crash dumps.
    "dmp",
    "mdmp",
    "stackdump", // Escrituras a medias y temporales de editores/descargas.
    "tmp",
    "temp",
    "part",
    "crdownload",
    "swp",
];

/// A path segment with this name hangs off engine telemetry, not off the save.
const JUNK_SEGMENTS: &[&str] = &[
    // Unity: shader and GPU info for *this* machine.
    "shadervariantanalytics",
    // Unreal.
    "crashreportclient",
];

/// Config extensions. Unshielded, a file with one of these uploads but never
/// gets restored over a live machine.
const CONFIG_EXTS: &[&str] = &[
    "ini",
    "cfg",
    "conf",
    "config",
    "toml",
    "yaml",
    "yml",
    "vdf",
    "properties",
    // Generic log: kept just in case, never restored.
    "log",
];

/// A stem ending in one of these is config whatever its extension. Catches
/// `GraphicsSettings.json`, `Fallout4Prefs.ini`, `UserOptions.dat`.
const CONFIG_STEM_SUFFIXES: &[&str] = &[
    "settings",
    "config",
    "configuration",
    "prefs",
    "preferences",
    "options",
];

/// A stem that is exactly one of these is config. Exact rather than
/// "contains", deliberately: `input` is config, `input_puzzle_solved` would be
/// the save.
const CONFIG_STEMS: &[&str] = &[
    "graphics",
    "graphic",
    "video",
    "audio",
    "sound",
    "display",
    "resolution",
    "input",
    "controls",
    "keybinds",
    "keybindings",
    "keyboard",
    "gamepad",
    "launcher",
    "hardware",
];

/// The two lists every walk of a save folder has to agree on: what the manifest
/// shields as save data, and what a shared save admits at all.
///
/// Borrowed rather than owned so the five places that classify (the backup
/// walk, the fingerprint sample, the restore gate, the merge count, the preview)
/// pass exactly the same pair and nobody can hand one without the other. Two
/// walks with different lists give two different signatures for the same quiet
/// folder, and the reducer sees a change that never settles.
#[derive(Debug, Clone, Copy, Default)]
pub struct Scope<'a> {
    /// Manifest patterns that shield a file as save data.
    pub shields: &'a [String],
    /// Patterns naming what a shared save consists of, relative to its root
    /// (`worlds_local/Alpha.db`). Empty means everything. See [`included`].
    pub include: &'a [String],
}

impl<'a> Scope<'a> {
    /// A scope over the whole folder: the game's shields and no include list.
    pub fn shields_only(shields: &'a [String]) -> Self {
        Scope {
            shields,
            include: &[],
        }
    }
}

/// Is `rel_path` one of the files a shared save names?
///
/// An empty list is no filter. Otherwise the path, `/`-separated as
/// `walk_source` hands it out, matches when one pattern matches it segment by
/// segment with [`glob_match`]: `*` and `?` stay inside a segment and there is
/// no `**`. A pattern with fewer segments than the path names a directory and
/// covers everything beneath it (`saves/Alpha` takes `saves/Alpha/region/r.mca`);
/// a pattern with more segments than the path matches nothing. The match is
/// exact in case: the patterns are made from names read off the disk. This is
/// a stored format (`shared_saves.include_json`), evaluated by every member's
/// build, so the rule does not move.
pub fn included(include: &[String], rel_path: &str) -> bool {
    include.is_empty()
        || include
            .iter()
            .any(|pattern| pattern_covers(pattern, rel_path))
}

/// The entries of a shared save's list that could name a folder outright: no
/// `*` or `?` anywhere in them. Whether one is a folder or a file is the
/// disk's answer, not the list's; [`in_mirrored_folder`] only takes paths
/// beneath one.
pub fn mirrored_folders(include: &[String]) -> impl Iterator<Item = &str> {
    include
        .iter()
        .map(String::as_str)
        .filter(|pattern| !pattern.is_empty() && !pattern.contains(['*', '?']))
}

/// Is `rel_path` inside a folder the share names whole, so that a version
/// written into the save replaces the folder's contents instead of merging
/// into them (HRD-Q-0027)?
///
/// A game that renames its files on every save (Valheim 1.0's
/// `worlds_local/<W>/_main.<N>.*` generations) leaves the older generation
/// beside a pulled one, and the game loads the newest; files there that the
/// version does not have are moved into the conflicts folder, kept there for
/// the retention period. True when an entry of
/// [`mirrored_folders`] equals the path's leading segments and the path has
/// more segments than the entry: `worlds_local/Alpha` covers
/// `worlds_local/Alpha/_main.8.db2` but neither `worlds_local/Alpha.db` nor
/// `worlds_local/Alpha2/x`. A wildcard entry (`worlds_local/Alpha_backup_*`)
/// and a flat file keep the merge. An empty list is no share: nothing is.
pub fn in_mirrored_folder(include: &[String], rel_path: &str) -> bool {
    mirrored_folders(include).any(|folder| {
        rel_path
            .strip_prefix(folder.trim_end_matches('/'))
            .and_then(|rest| rest.strip_prefix('/'))
            .is_some_and(|rest| !rest.is_empty())
    })
}

/// Does a version with these files hold the share's world as a folder, a file
/// beneath one of its [`mirrored_folders`]? A Valheim world converted to 1.0
/// does; a legacy flat world's version does not.
pub fn holds_mirrored_folder<'a>(
    include: &[String],
    mut version_files: impl Iterator<Item = &'a str>,
) -> bool {
    version_files.any(|rel| in_mirrored_folder(include, rel))
}

/// Is `rel_path`, a local file the version being written does not carry,
/// replaced by that version, and so moved aside rather than kept
/// (HRD-Q-0027)? Anything beneath a folder the share names whole
/// ([`in_mirrored_folder`]). And once the version holds the world as a folder
/// (`version_holds_folder`, [`holds_mirrored_folder`]), the files the list
/// names outright too: a converted world's legacy `<W>.db` and `<W>.fwl`, and
/// their `.old` twins, would otherwise stay beside the folder and go up again
/// with the next push. Wildcard entries (`<W>_backup_*`, the game's own
/// backups) never are.
pub fn replaced_by_version(include: &[String], rel_path: &str, version_holds_folder: bool) -> bool {
    in_mirrored_folder(include, rel_path)
        || (version_holds_folder
            && mirrored_folders(include).any(|entry| entry.trim_end_matches('/') == rel_path))
}

/// Can any file under the directory `rel_dir` be [`included`]? The walk asks
/// before descending, so a shared save's fingerprint never reads the folders
/// its list cannot name. A pattern reaches beneath when its leading segments
/// match the directory's, segment by segment, whichever of the two runs out
/// first: a shorter pattern covers the directory whole, a longer one may name
/// something inside it. An empty list is no filter.
pub fn reaches_beneath(include: &[String], rel_dir: &str) -> bool {
    include.is_empty()
        || include.iter().any(|pattern| {
            pattern
                .split('/')
                .zip(rel_dir.split('/'))
                .all(|(p, s)| glob_match(p, s))
        })
}

/// One pattern against one path, without collecting either: this runs once per
/// file per pattern on the engine's tick.
fn pattern_covers(pattern: &str, rel_path: &str) -> bool {
    let mut pat = pattern.split('/');
    let mut path = rel_path.split('/');
    loop {
        match (pat.next(), path.next()) {
            (None, _) => return true,
            (Some(_), None) => return false,
            (Some(p), Some(s)) => {
                if !glob_match(p, s) {
                    return false;
                }
            }
        }
    }
}

/// What a restore is allowed to write to disk.
///
/// Travels inside `RestoreOptions` and rides along with the preview, so that
/// what `--dry-run` promises and what the restore does come out of one decision
/// rather than two copies drifting apart.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreGate {
    /// Manifest patterns that shield a file as save data.
    pub shields: Vec<String>,
    /// What a shared save consists of; empty for everything. A file outside it
    /// is never written, whatever the switch below says.
    pub include: Vec<String>,
    /// The user asked by hand for the snapshot's config to be written over this
    /// machine's (`--allow-ini`, the switch in the dialog). Off by default, and
    /// always off in auto-restore: writing PC A's config onto PC B is precisely
    /// the crash this module exists to prevent.
    pub allow_device_local: bool,
}

impl RestoreGate {
    /// Wide open, the way things were before any of this existed. For tests and
    /// for callers that already did their own filtering.
    pub fn permissive() -> Self {
        Self {
            shields: Vec::new(),
            include: Vec::new(),
            allow_device_local: true,
        }
    }

    /// The lists the walk on the other side of this gate has to use.
    pub fn scope(&self) -> Scope<'_> {
        Scope {
            shields: &self.shields,
            include: &self.include,
        }
    }

    /// Does this snapshot file get written to disk?
    pub fn allows(&self, rel_path: &str) -> bool {
        classify(rel_path, self.scope()).is_restored(self.allow_device_local)
    }
}

/// Classifies a file by its path relative to the save root, `/`-separated, the
/// shape `walk_source` already produces.
///
/// `scope.shields` are filename patterns lifted from the manifest (`*.sav`,
/// `save*`). Anything matching one is save data and leaves by the top door
/// without meeting another rule. `scope.include`, when set, is checked first:
/// a file a shared save does not name is [`FileClass::Junk`], never backed up,
/// never restored, never counted.
/// The suffix a restore gives each file it stages beside its destination
/// before renaming it into place. Never save data ([`classify`]); a leftover
/// is swept at the save's start and before the next merge.
pub const RESTORE_TMP_SUFFIX: &str = ".hoard-restore.tmp";

/// Is `path` (a name, or a path whose last part is the name) a restore's
/// staged copy? The suffix whatever its case, as [`classify`] takes it.
pub fn is_restore_tmp(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(RESTORE_TMP_SUFFIX)
}

pub fn classify(rel_path: &str, scope: Scope<'_>) -> FileClass {
    // 0. A shared save is its named files and nothing else.
    if !included(scope.include, rel_path) {
        return FileClass::Junk;
    }

    let lower = rel_path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);

    // 0b. Hoard's own restore staging, ahead of the shields: a shield ending
    //     in `*` (438 catalog games) would take a crash's leftover for save
    //     data and push it (L-1).
    if is_restore_tmp(name) {
        return FileClass::Junk;
    }

    // 1. The manifest rules: if it says this is a save, it is a save.
    if scope.shields.iter().any(|p| glob_match(p, name)) {
        return FileClass::SaveData;
    }

    // 2. Unambiguous litter, the only thing that stops being uploaded.
    if JUNK_NAMES.contains(&name) {
        return FileClass::Junk;
    }
    // The AppleDouble `._foo` files macOS scatters on non-HFS volumes.
    if name.starts_with("._") {
        return FileClass::Junk;
    }
    if let Some(ext) = extension_of(name) {
        if JUNK_EXTS.contains(&ext) {
            return FileClass::Junk;
        }
    }
    let segments: Vec<&str> = lower.split('/').collect();
    // Everything hanging off a telemetry directory.
    if segments
        .iter()
        .take(segments.len().saturating_sub(1))
        .any(|s| JUNK_SEGMENTS.contains(s))
    {
        return FileClass::Junk;
    }
    // The Unity Analytics event queue, `Unity/<guid>/Analytics/...`. The GUID
    // identifies the *install*, so restoring it onto another machine clones its
    // analytics identity.
    if is_under_unity_analytics(&segments) {
        return FileClass::Junk;
    }

    // 3. Config and the rest of this machine's data. Uploaded, never restored
    //    on its own.
    if let Some(ext) = extension_of(name) {
        if CONFIG_EXTS.contains(&ext) {
            return FileClass::DeviceLocal;
        }
    }
    let stem = stem_of(name);
    if CONFIG_STEMS.contains(&stem) || CONFIG_STEM_SUFFIXES.iter().any(|s| stem.ends_with(s)) {
        return FileClass::DeviceLocal;
    }

    FileClass::SaveData
}

/// `Unity/<something>/Analytics/...` at any depth. The `unity` ancestor is
/// required so a game's own `analytics` folder is not mistaken for it.
fn is_under_unity_analytics(segments: &[&str]) -> bool {
    let Some(unity_at) = segments.iter().position(|s| *s == "unity") else {
        return false;
    };
    // The file itself does not count as a containing directory.
    segments
        .iter()
        .enumerate()
        .any(|(i, s)| i > unity_at && i + 1 < segments.len() && *s == "analytics")
}

/// Lowercase extension without the dot. `None` when there is none, or when the
/// dot opens the name: `.bashrc` has no extension, that is its name.
fn extension_of(name: &str) -> Option<&str> {
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    Some(ext)
}

/// The name without its extension.
fn stem_of(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => name,
    }
}

/// Is this manifest pattern any use as a shield?
///
/// `*` and `*.*` match everything, so they would shield the whole folder and
/// leave the filter doing nothing. They do not say *what* a save is, only "there
/// are files here", and 1,519 catalogue templates are exactly that.
pub fn is_useful_shield(pattern: &str) -> bool {
    let p = pattern.trim();
    if !p.contains('*') && !p.contains('?') {
        // A literal name is informative, and then it is not acting as a
        // wildcard at all: it still works as an exact shield.
        return !p.is_empty();
    }
    !matches!(p, "*" | "*.*" | "?" | "**")
}

/// Single-segment glob: `*` is anything including empty, `?` is one character.
/// No classes and no alternatives, because the manifest does not use them in the
/// last segment.
///
/// Written here rather than reused from `pathexpand` because the kernel does not
/// depend on `hoard-agent` (ADR 0021's hard rule: the kernel imports no shells).
fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    // The last `*` seen and where the name was then, so we can backtrack.
    let (mut star, mut backtrack) = (usize::MAX, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            backtrack = ni;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            backtrack += 1;
            ni = backtrack;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(path: &str) -> FileClass {
        classify(path, Scope::default())
    }

    fn shielded(path: &str, shields: &[String]) -> FileClass {
        classify(
            path,
            Scope {
                shields,
                include: &[],
            },
        )
    }

    #[test]
    fn plain_save_files_are_save_data() {
        for p in [
            "save1.sav",
            "autosave/autosave0.sav",
            "bmonster_4_6_2026_auto_1242.pss",
            "savedGames.gd",
            "level.dat",
            "world/region/r.0.0.mca",
        ] {
            assert_eq!(c(p), FileClass::SaveData, "{p}");
        }
    }

    /// A real user's folder that currently syncs whole. This is the case that
    /// motivated the module.
    #[test]
    fn the_unity_folder_that_started_this() {
        assert_eq!(c("Player.log"), FileClass::Junk);
        assert_eq!(c("Player-prev.log"), FileClass::Junk);
        assert_eq!(c("steam_autocloud.vdf"), FileClass::Junk);
        assert_eq!(
            c("Unity/0a8833bc-a8ad-47f7-abed-f8d04a6f02f8/Analytics/values"),
            FileClass::Junk
        );
        assert_eq!(
            c("Unity/ShaderVariantAnalytics/ShaderRuntimeInfoEvent.json"),
            FileClass::Junk
        );
        // And the saves in the same folder come out untouched.
        assert_eq!(c("savedGames2.gd"), FileClass::SaveData);
        assert_eq!(c("savedGamesDeepBackup.gd.restore"), FileClass::SaveData);
    }

    #[test]
    fn os_and_temp_junk() {
        for p in [
            ".DS_Store",
            "Thumbs.db",
            "desktop.ini",
            "._save.sav",
            "crash_2026.dmp",
            "save.sav.tmp",
            "download.part",
        ] {
            assert_eq!(c(p), FileClass::Junk, "{p}");
        }
    }

    #[test]
    fn config_is_device_local_not_junk() {
        for p in [
            "graphics.ini",
            "settings.toml",
            "config.json",
            "GraphicsSettings.json",
            "Fallout4Prefs.ini",
            "UserOptions.dat",
            "keybinds.cfg",
            "video.yaml",
            "debug.log",
        ] {
            assert_eq!(c(p), FileClass::DeviceLocal, "{p}");
        }
    }

    /// The ladder's rule: anything doubtful still uploads. Only unambiguous
    /// litter stays out of the snapshot.
    #[test]
    fn only_junk_is_dropped_from_the_backup() {
        assert!(!FileClass::Junk.is_backed_up());
        assert!(FileClass::DeviceLocal.is_backed_up());
        assert!(FileClass::SaveData.is_backed_up());
    }

    #[test]
    fn device_local_needs_an_explicit_yes_to_be_restored() {
        assert!(!FileClass::DeviceLocal.is_restored(false));
        assert!(FileClass::DeviceLocal.is_restored(true));
        // Litter does not come back even on request; the switch is for config.
        assert!(!FileClass::Junk.is_restored(true));
        assert!(FileClass::SaveData.is_restored(false));
    }

    /// 582 catalogue templates use `*.ini` as their save pattern. Unshielded,
    /// the extension rule would take them all.
    #[test]
    fn the_manifest_shield_beats_every_rule_below_it() {
        let shields = vec!["*.ini".to_string()];
        assert_eq!(shielded("save01.ini", &shields), FileClass::SaveData);
        assert_eq!(shielded("save01.ini", &[]), FileClass::DeviceLocal);

        let log_shield = vec!["*.log".to_string()];
        assert_eq!(shielded("player.log", &log_shield), FileClass::SaveData);
        assert_eq!(shielded("player.log", &[]), FileClass::Junk);
    }

    #[test]
    fn shields_match_on_the_basename_at_any_depth() {
        let shields = vec!["*.bksav".to_string()];
        assert_eq!(
            shielded("Saves/slot3/quick.bksav", &shields),
            FileClass::SaveData
        );
    }

    #[test]
    fn degenerate_patterns_are_not_shields() {
        // These would shield the whole folder and leave the filter doing
        // nothing.
        assert!(!is_useful_shield("*"));
        assert!(!is_useful_shield("*.*"));
        assert!(!is_useful_shield("**"));
        assert!(is_useful_shield("*.sav"));
        assert!(is_useful_shield("save*"));
        assert!(is_useful_shield("gamedata.bin"));
    }

    #[test]
    fn the_gate_is_shut_for_config_by_default() {
        let gate = RestoreGate::default();
        assert!(gate.allows("slot1.sav"));
        assert!(!gate.allows("graphics.ini"));
        assert!(!gate.allows("Player.log"));
    }

    #[test]
    fn the_gate_opens_for_config_when_asked_but_never_for_junk() {
        let gate = RestoreGate {
            shields: Vec::new(),
            include: Vec::new(),
            allow_device_local: true,
        };
        assert!(gate.allows("graphics.ini"));
        // Litter does not come back even on request.
        assert!(!gate.allows("Player.log"));
        assert!(!gate.allows(".DS_Store"));
    }

    #[test]
    fn a_shielded_config_file_still_goes_through_a_shut_gate() {
        let gate = RestoreGate {
            shields: vec!["*.ini".to_string()],
            include: Vec::new(),
            allow_device_local: false,
        };
        assert!(gate.allows("save01.ini"));
    }

    fn inc(patterns: &[&str]) -> Vec<String> {
        patterns.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn an_empty_include_list_is_no_filter() {
        assert!(included(&[], "anything/at/all.bin"));
        assert!(included(&[], ""));
    }

    /// The Valheim template: one world's files under `worlds_local/`, the
    /// wildcard only inside the last segment.
    #[test]
    fn include_matches_segment_by_segment() {
        let list = inc(&[
            "worlds_local/Alpha.db",
            "worlds_local/Alpha.fwl",
            "worlds_local/Alpha_backup_*",
        ]);
        assert!(included(&list, "worlds_local/Alpha.db"));
        assert!(included(
            &list,
            "worlds_local/Alpha_backup_auto-20260913.db"
        ));
        assert!(!included(&list, "worlds_local/Beta.db"));
        assert!(!included(&list, "characters_local/Alpha.fch"));
        // Same name, wrong depth: `*` never crosses a `/`.
        assert!(!included(&list, "Alpha.db"));
        assert!(!included(&list, "worlds_local/old/Alpha.db"));
        // A pattern deeper than the path matches nothing; one at the same
        // depth matches segment by segment.
        assert!(!included(&inc(&["*/*/*"]), "worlds_local/Alpha.db"));
        assert!(included(&inc(&["*/*"]), "worlds_local/Alpha.db"));
    }

    /// A pattern shorter than the path names a directory and covers everything
    /// beneath it: how a game that keeps a world in a folder of its own is named.
    #[test]
    fn a_shorter_pattern_covers_the_directory_it_names() {
        let list = inc(&["saves/Alpha"]);
        assert!(included(&list, "saves/Alpha/level.dat"));
        assert!(included(&list, "saves/Alpha/region/r.0.0.mca"));
        assert!(!included(&list, "saves/Alpha2/level.dat"));
        assert!(!included(&list, "saves/Beta/level.dat"));
        assert!(!included(&list, "saves"));
        // `*` alone therefore covers the whole root, which is what it says.
        assert!(included(&inc(&["*"]), "worlds_local/Alpha.db"));
    }

    /// The walk descends only where a pattern can still match something: the
    /// prefix test is the same segment rule as [`included`], stopped at the
    /// directory's depth.
    #[test]
    fn a_directory_no_pattern_can_reach_is_not_descended() {
        let list = inc(&["worlds_local/Alpha.db", "worlds_local/Alpha_backup_*"]);
        assert!(reaches_beneath(&list, "worlds_local"));
        assert!(!reaches_beneath(&list, "characters_local"));
        // Deeper than any pattern: nothing under it can match.
        assert!(!reaches_beneath(&list, "worlds_local/old"));
        // A shorter pattern covers the directory and all beneath it.
        assert!(reaches_beneath(
            &inc(&["saves/Alpha"]),
            "saves/Alpha/region"
        ));
        assert!(!reaches_beneath(&inc(&["saves/Alpha"]), "saves/Beta"));
        assert!(reaches_beneath(&inc(&["*/*"]), "worlds_local"));
        // No list, no pruning.
        assert!(reaches_beneath(&[], "anything/at/all"));
    }

    /// Exact in case: the pattern was made from the name on disk.
    #[test]
    fn include_is_case_sensitive() {
        assert!(!included(
            &inc(&["worlds_local/alpha.db"]),
            "worlds_local/Alpha.db"
        ));
    }

    /// HRD-Q-0027: only a folder named outright, and only beneath it.
    #[test]
    fn a_mirrored_folder_is_a_wildcard_free_entry_covering_paths_beneath_it() {
        let list = inc(&[
            "worlds_local/Alpha",
            "worlds_local/Alpha.db",
            "worlds_local/Alpha_backup_*",
            "saves/slot?",
        ]);
        assert!(in_mirrored_folder(&list, "worlds_local/Alpha/_main.8.db2"));
        assert!(in_mirrored_folder(&list, "worlds_local/Alpha/sub/x.chunk"));
        // A sibling whose name starts the same is another world.
        assert!(!in_mirrored_folder(&list, "worlds_local/Alpha2/x"));
        assert!(!in_mirrored_folder(&list, "worlds_local/Alpha2"));
        // The entry itself, and the legacy flat files beside it.
        assert!(!in_mirrored_folder(&list, "worlds_local/Alpha"));
        assert!(!in_mirrored_folder(&list, "worlds_local/Alpha.db"));
        // Wildcard entries keep the merge, file or folder.
        assert!(!in_mirrored_folder(
            &list,
            "worlds_local/Alpha_backup_auto-1/_main.3.db2"
        ));
        assert!(!in_mirrored_folder(&list, "saves/slot1/a.sav"));
        // Case is exact, as in `included`.
        assert!(!in_mirrored_folder(&list, "worlds_local/alpha/_main.8.db2"));
        // Characters, and a save that is not shared at all.
        assert!(!in_mirrored_folder(&list, "characters_local/Me.fch"));
        assert!(!in_mirrored_folder(&[], "worlds_local/Alpha/_main.8.db2"));
        assert_eq!(
            mirrored_folders(&list).collect::<Vec<_>>(),
            vec!["worlds_local/Alpha", "worlds_local/Alpha.db"]
        );
    }

    /// HRD-Q-0027, a converted world: once the version holds the folder, the
    /// flat files the list names outright are replaced too; a backup the
    /// wildcard names, a character and another world never are. A legacy
    /// version replaces only what is beneath the folder.
    #[test]
    fn a_version_holding_the_folder_replaces_the_flat_files_too() {
        let list = inc(&[
            "worlds_local/Alpha",
            "worlds_local/Alpha.db",
            "worlds_local/Alpha.fwl",
            "worlds_local/Alpha.db.old",
            "worlds_local/Alpha_backup_*",
        ]);
        assert!(holds_mirrored_folder(
            &list,
            ["characters_local/Me.fch", "worlds_local/Alpha/_main.2.db2"].into_iter()
        ));
        assert!(!holds_mirrored_folder(
            &list,
            ["worlds_local/Alpha.db", "worlds_local/Alpha.fwl"].into_iter()
        ));
        for flat in [
            "worlds_local/Alpha.db",
            "worlds_local/Alpha.fwl",
            "worlds_local/Alpha.db.old",
        ] {
            assert!(replaced_by_version(&list, flat, true), "{flat}");
            assert!(!replaced_by_version(&list, flat, false), "{flat}");
        }
        assert!(replaced_by_version(
            &list,
            "worlds_local/Alpha/_main.1.db2",
            false
        ));
        for kept in [
            "worlds_local/Alpha_backup_auto-1.db",
            "worlds_local/Alpha_backup_auto-1/_main.1.db2",
            "worlds_local/Alpha2.db",
            "characters_local/Me.fch",
        ] {
            assert!(!replaced_by_version(&list, kept, true), "{kept}");
        }
        assert!(!replaced_by_version(&[], "worlds_local/Alpha.db", true));
    }

    /// `?` is one character of one segment, like everywhere else in the module.
    #[test]
    fn include_question_mark_is_one_character() {
        let list = inc(&["slot?.sav"]);
        assert!(included(&list, "slot1.sav"));
        assert!(!included(&list, "slot12.sav"));
        assert!(!included(&list, "a/slot1.sav"));
    }

    /// Outside the list a file is litter to every consumer: not uploaded, not
    /// restored, not counted, whatever the shields say about it.
    #[test]
    fn a_file_outside_the_include_list_is_junk_before_any_shield() {
        let include = inc(&["worlds_local/Alpha.db"]);
        let shields = inc(&["*.db"]);
        let scope = Scope {
            shields: &shields,
            include: &include,
        };
        assert_eq!(
            classify("worlds_local/Alpha.db", scope),
            FileClass::SaveData
        );
        assert_eq!(classify("worlds_local/Beta.db", scope), FileClass::Junk);
        assert!(!classify("worlds_local/Beta.db", scope).is_backed_up());
        assert!(!classify("worlds_local/Beta.db", scope).is_restored(true));
    }

    /// L-1: a restore's staged copy left by a crash is litter even where a
    /// shield ending in `*` would take every file for save data, and inside a
    /// shared world's folder.
    #[test]
    fn a_restore_temp_file_is_junk_before_the_shields() {
        let star = inc(&["*"]);
        let tmp = "worlds_local/Alpha/0_0.chunk.hoard-restore.tmp";
        assert_eq!(shielded(tmp, &star), FileClass::Junk);
        let include = inc(&["worlds_local/Alpha"]);
        let scope = Scope {
            shields: &star,
            include: &include,
        };
        assert_eq!(classify(tmp, scope), FileClass::Junk);
        assert_eq!(
            classify("worlds_local/Alpha/0_0.chunk", scope),
            FileClass::SaveData
        );
    }

    #[test]
    fn the_gate_carries_the_include_list() {
        let gate = RestoreGate {
            shields: Vec::new(),
            include: inc(&["worlds_local/Alpha.db", "worlds_local/Alpha.fwl"]),
            allow_device_local: true,
        };
        assert!(gate.allows("worlds_local/Alpha.db"));
        assert!(!gate.allows("worlds_local/Beta.db"));
        assert!(!gate.allows("characters_local/Me.fch"));
        // Wide open still means wide open.
        assert!(RestoreGate::permissive().allows("characters_local/Me.fch"));
    }

    #[test]
    fn glob_basics() {
        assert!(glob_match("*.sav", "slot1.sav"));
        assert!(!glob_match("*.sav", "slot1.savx"));
        assert!(glob_match("save*", "save"));
        assert!(glob_match("profile?.sav", "profile1.sav"));
        assert!(!glob_match("profile?.sav", "profile12.sav"));
        assert!(glob_match("*save*.dat", "my_save_2.dat"));
    }

    /// A game's own `analytics` folder is not Unity telemetry. The `Unity/`
    /// ancestor is what condemns it.
    #[test]
    fn analytics_alone_is_not_enough() {
        assert_eq!(c("analytics/run1.sav"), FileClass::SaveData);
        assert_eq!(c("Unity/x/Analytics/run1.sav"), FileClass::Junk);
    }

    #[test]
    fn dotfiles_have_no_extension() {
        assert_eq!(extension_of(".bashrc"), None);
        assert_eq!(extension_of("save.sav"), Some("sav"));
        assert_eq!(stem_of("graphicssettings.json"), "graphicssettings");
    }
}
