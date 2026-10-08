//! Where downloaded archives live and how the Downloads tab groups them.
//!
//! Files go in the download location (Settings, defaults to
//! [`paths::downloads_dir`]) inside one folder per mod, so every version of a
//! mod sits together: `<location>/<Mod name>/<file>`. The Downloads tab shows
//! the same thing as a tree, one mod with its versions under it, each marked
//! stable, beta or nightly.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::db::{Db, DownloadRow};
use crate::{Error, Result, paths};

/// `settings` key holding the chosen download location.
pub const LOCATION_KEY: &str = "downloads_dir";

/// The download location (created if missing).
pub fn location(db: &Db) -> Result<PathBuf> {
    match db.get_setting(LOCATION_KEY)? {
        Some(p) if !p.trim().is_empty() => {
            let dir = PathBuf::from(p);
            std::fs::create_dir_all(&dir)?;
            Ok(dir)
        }
        _ => paths::downloads_dir(),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Location {
    pub path: String,
    pub default: String,
    pub is_default: bool,
}

pub fn describe_location(db: &Db) -> Result<Location> {
    let path = location(db)?;
    let default = paths::downloads_dir()?;
    Ok(Location {
        is_default: path == default,
        path: path.to_string_lossy().into_owned(),
        default: default.to_string_lossy().into_owned(),
    })
}

/// Check a new location before switching to it: an absolute, writable
/// directory that isn't inside the manager's staging or backup folders.
pub fn validate_location(dir: &Path) -> Result<PathBuf> {
    if !dir.is_absolute() {
        return Err(Error::Other("pick a full folder path".into()));
    }
    std::fs::create_dir_all(dir)?;
    let dir = dir.canonicalize()?;
    for reserved in [paths::staging_dir()?, paths::backups_dir()?] {
        let reserved = reserved.canonicalize().unwrap_or(reserved);
        if dir.starts_with(&reserved) {
            return Err(Error::Other(format!("{} is used by the manager itself; pick another folder", reserved.display())));
        }
    }
    let probe = dir.join(".cpmx2077-write-test");
    std::fs::write(&probe, b"")
        .map_err(|e| Error::Other(format!("can't write to {}: {e}", dir.display())))?;
    let _ = std::fs::remove_file(&probe);
    Ok(dir)
}

/// Folder name for a mod's files: its name, made safe for any filesystem.
pub fn folder_name(mod_name: Option<&str>, d: &DownloadRow) -> String {
    let fallback = || match (d.source.as_str(), d.nexus_mod_id, d.source_ref.as_deref()) {
        (_, Some(id), _) => format!("Nexus mod {id}"),
        (_, None, Some(r)) => r.rsplit('/').next().unwrap_or(r).to_string(),
        _ => "Other files".to_string(),
    };
    let raw = mod_name.filter(|n| !n.trim().is_empty()).map(str::to_string).unwrap_or_else(fallback);
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim_end_matches('.').trim();
    let mut out: String = cleaned.chars().take(80).collect();
    out = out.trim().to_string();
    if out.is_empty() { "Other files".into() } else { out }
}

/// Where a download belongs inside `root`.
pub fn folder_for(root: &Path, d: &DownloadRow) -> PathBuf {
    root.join(folder_name(d.mod_name.as_deref(), d))
}

/// A file to move: every download row that points at `from` follows it.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedMove {
    pub ids: Vec<i64>,
    pub from: PathBuf,
    pub to: PathBuf,
}

/// Moves that put every download into its mod's folder under `root`. Files
/// already in place are left alone.
pub fn plan_moves(rows: &[DownloadRow], root: &Path) -> Vec<PlannedMove> {
    let mut by_path: BTreeMap<PathBuf, PlannedMove> = BTreeMap::new();
    for d in rows {
        let from = PathBuf::from(&d.path);
        let name = from.file_name().map(PathBuf::from).unwrap_or_else(|| PathBuf::from(&d.file_name));
        let to = folder_for(root, d).join(name);
        let m = by_path.entry(from.clone()).or_insert_with(|| PlannedMove { ids: Vec::new(), from: from.clone(), to });
        m.ids.push(d.id);
    }
    by_path.into_values().filter(|m| m.from != m.to).collect()
}

/// Move a file, copying when it crosses filesystems. If `to` is taken by a
/// different file, a free name like `file (2).zip` is used. Returns where
/// the file ended up.
pub fn move_file(from: &Path, to: &Path) -> Result<PathBuf> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let to = free_path(from, to)?;
    if to.exists() {
        // The same file is already there.
        std::fs::remove_file(from)?;
        return Ok(to);
    }
    match std::fs::rename(from, &to) {
        Ok(()) => Ok(to),
        Err(_) => {
            // Another filesystem: copy, check, then remove the original.
            let part = to.with_extension("part-move");
            std::fs::copy(from, &part)?;
            if crate::hash::sha256_file(&part)? != crate::hash::sha256_file(from)? {
                let _ = std::fs::remove_file(&part);
                return Err(Error::Integrity(format!("copy of {} doesn't match the original", from.display())));
            }
            std::fs::rename(&part, &to)?;
            std::fs::remove_file(from)?;
            Ok(to)
        }
    }
}

/// `to`, or `to` itself when it already holds the same bytes as `from`, or
/// the first free `name (n).ext` next to it.
fn free_path(from: &Path, to: &Path) -> Result<PathBuf> {
    if !to.exists() {
        return Ok(to.to_path_buf());
    }
    if crate::hash::sha256_file(to)? == crate::hash::sha256_file(from)? {
        return Ok(to.to_path_buf());
    }
    let stem = to.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = to.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    for n in 2..10_000 {
        let candidate = to.with_file_name(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(Error::Other(format!("no free file name next to {}", to.display())))
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MoveReport {
    pub moved: usize,
    /// Files the library lists that aren't on disk any more.
    pub missing: Vec<String>,
    /// Files that couldn't be moved, with the reason. They stay where they were.
    pub failed: Vec<String>,
}

/// Run `moves`, telling `done` where each file ended up so the library can
/// follow it. Missing files are reported, not fatal.
pub fn run_moves(moves: &[PlannedMove], mut done: impl FnMut(&PlannedMove, &Path) -> Result<()>) -> MoveReport {
    let mut report = MoveReport::default();
    for m in moves {
        if !m.from.exists() {
            report.missing.push(m.from.to_string_lossy().into_owned());
            continue;
        }
        match move_file(&m.from, &m.to).and_then(|to| done(m, &to)) {
            Ok(()) => report.moved += 1,
            Err(e) => report.failed.push(format!("{}: {e}", m.from.display())),
        }
    }
    report
}

/// Remove folders left empty under `root` (one level of mod folders).
pub fn prune_empty_dirs(root: &Path) {
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                let _ = std::fs::remove_dir(&p); // only succeeds when empty
            }
        }
    }
}

// ---- release channels ------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Stable,
    Beta,
    Nightly,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Beta => "beta",
            Channel::Nightly => "nightly",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "stable" => Some(Channel::Stable),
            "beta" => Some(Channel::Beta),
            "nightly" => Some(Channel::Nightly),
            _ => None,
        }
    }
}

/// Whether a file is a stable release, a beta (alpha, release candidate,
/// preview, experimental) or a nightly (dev or snapshot build), from its
/// version, names and the source's own pre-release flag. Words are matched
/// whole, so a mod called "Alphabet" isn't a beta.
pub fn channel_of(texts: &[Option<&str>], prerelease: bool) -> Channel {
    let mut found = None;
    for t in texts.iter().flatten() {
        let lower = t.to_lowercase();
        if lower.contains("pre-release") || lower.contains("prerelease") {
            found = found.max(Some(Channel::Beta));
        }
        for word in lower.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()) {
            // "beta2", "rc1": the word with its number.
            let stem = word.trim_end_matches(|c: char| c.is_ascii_digit());
            let c = match stem {
                "nightly" | "nightlies" | "snapshot" | "canary" => Some(Channel::Nightly),
                "beta" | "alpha" | "rc" | "preview" | "experimental" | "unstable" | "wip" => Some(Channel::Beta),
                _ => None,
            };
            found = found.max(c);
        }
    }
    match found {
        Some(c) => c,
        None if prerelease => Channel::Beta,
        None => Channel::Stable,
    }
}

/// The stored channel, or one worked out from what the row records.
pub fn row_channel(d: &DownloadRow) -> Channel {
    d.channel
        .as_deref()
        .and_then(Channel::parse)
        .unwrap_or_else(|| channel_of(&[d.version.as_deref(), Some(d.file_name.as_str())], false))
}

// ---- duplicates --------------------------------------------------------------

/// One file at its source: the same key means the same download.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileKey {
    Nexus { mod_id: i64, file_id: i64 },
    Source { source: String, id: String, file: String },
}

impl FileKey {
    pub fn matches(&self, d: &DownloadRow) -> bool {
        match self {
            FileKey::Nexus { mod_id, file_id } => {
                (d.source.is_empty() || d.source == "nexus") && d.nexus_mod_id == Some(*mod_id) && d.nexus_file_id == Some(*file_id)
            }
            FileKey::Source { source, id, file } => {
                d.source == *source && d.source_ref.as_deref() == Some(id) && d.source_file.as_deref() == Some(file)
            }
        }
    }
}

/// The file is still on disk, whole: there, and as big as when it was saved.
pub fn intact(d: &DownloadRow) -> bool {
    std::fs::metadata(&d.path).is_ok_and(|m| m.is_file() && m.len() as i64 == d.size)
}

/// The copy of `key` already in the downloads, if one is still intact.
/// `rows` come newest first, so the newest copy wins.
pub fn existing<'a>(rows: &'a [DownloadRow], key: &FileKey) -> Option<&'a DownloadRow> {
    rows.iter().find(|d| key.matches(d) && intact(d))
}

/// An intact download with exactly this content, other than the file at
/// `except` (a file the same bytes came in under another name or id).
pub fn same_content<'a>(rows: &'a [DownloadRow], sha256: &str, except: &Path) -> Option<&'a DownloadRow> {
    if sha256.is_empty() {
        return None;
    }
    rows.iter().find(|d| d.sha256.eq_ignore_ascii_case(sha256) && Path::new(&d.path) != except && intact(d))
}

/// Files being downloaded right now, so the same file isn't fetched twice
/// at once (two clicks, or a click while the queue has it).
#[derive(Debug, Default)]
pub struct InFlight(std::sync::Mutex<std::collections::HashSet<FileKey>>);

/// Holds a file's place in [`InFlight`] until dropped.
pub struct InFlightGuard<'a> {
    set: &'a InFlight,
    key: FileKey,
}

impl InFlight {
    pub fn begin(&self, key: FileKey) -> Result<InFlightGuard<'_>> {
        if !self.0.lock().unwrap().insert(key.clone()) {
            return Err(Error::Other("this file is already downloading".into()));
        }
        Ok(InFlightGuard { set: self, key })
    }
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.set.0.lock().unwrap().remove(&self.key);
    }
}

// ---- grouping --------------------------------------------------------------

/// One version of a mod in the Downloads tab.
#[derive(Debug, Clone, Serialize)]
pub struct DownloadEntry {
    #[serde(flatten)]
    pub row: DownloadRow,
    pub channel: Channel,
    pub on_disk: bool,
}

/// A mod with all its downloaded versions, newest first.
#[derive(Debug, Clone, Serialize)]
pub struct DownloadGroup {
    /// Stable key for remembering which folders are open.
    pub key: String,
    pub name: String,
    pub folder: String,
    pub entries: Vec<DownloadEntry>,
    pub total_size: i64,
}

/// What makes downloads "the same mod".
fn group_key(d: &DownloadRow) -> String {
    match (d.nexus_mod_id, d.source_ref.as_deref()) {
        (Some(id), _) => format!("nexus:{id}"),
        (None, Some(r)) => format!("{}:{r}", if d.source.is_empty() { "nexus" } else { d.source.as_str() }),
        _ => format!("file:{}", d.id),
    }
}

/// Group downloads by mod. `rows` come newest first (as [`Db::downloads`]
/// returns them); several rows for the same file on disk (a file downloaded
/// twice) show once, as the newest.
pub fn group(rows: Vec<DownloadRow>) -> Vec<DownloadGroup> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, DownloadGroup> = HashMap::new();
    let mut seen_paths = std::collections::HashSet::new();
    for d in rows {
        if !seen_paths.insert(d.path.clone()) {
            continue;
        }
        let key = group_key(&d);
        let g = groups.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            DownloadGroup {
                key: key.clone(),
                name: d.mod_name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| d.file_name.clone()),
                folder: folder_name(d.mod_name.as_deref(), &d),
                entries: Vec::new(),
                total_size: 0,
            }
        });
        g.total_size += d.size;
        g.entries.push(DownloadEntry { channel: row_channel(&d), on_disk: Path::new(&d.path).exists(), row: d });
    }
    order.into_iter().filter_map(|k| groups.remove(&k)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyed(id: i64, path: &Path, size: i64, sha: &str) -> DownloadRow {
        DownloadRow {
            id,
            nexus_mod_id: Some(7),
            nexus_file_id: Some(9),
            file_name: "f.zip".into(),
            path: path.to_string_lossy().into_owned(),
            size,
            sha256: sha.into(),
            source: "nexus".into(),
            ..Default::default()
        }
    }

    #[test]
    fn existing_copy_is_found_only_while_it_is_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("7-9-f.zip");
        std::fs::write(&path, b"hello").unwrap();
        let key = FileKey::Nexus { mod_id: 7, file_id: 9 };
        let rows = vec![keyed(2, &path, 5, "ab")];
        assert_eq!(existing(&rows, &key).map(|d| d.id), Some(2));
        assert!(existing(&rows, &FileKey::Nexus { mod_id: 7, file_id: 10 }).is_none(), "another file of the mod");
        let github = FileKey::Source { source: "github".into(), id: "a/b".into(), file: "1".into() };
        assert!(existing(&rows, &github).is_none());

        std::fs::write(&path, b"hell").unwrap();
        assert!(existing(&rows, &key).is_none(), "a cut-short file is downloaded again");
        std::fs::remove_file(&path).unwrap();
        assert!(existing(&rows, &key).is_none(), "a deleted file is downloaded again");
    }

    #[test]
    fn source_keys_match_source_rows() {
        let d = DownloadRow {
            source: "github".into(),
            source_ref: Some("psiberx/cp2077-archive-xl".into()),
            source_file: Some("123".into()),
            ..Default::default()
        };
        let key = |file: &str| FileKey::Source { source: "github".into(), id: "psiberx/cp2077-archive-xl".into(), file: file.into() };
        assert!(key("123").matches(&d));
        assert!(!key("124").matches(&d));
        assert!(!FileKey::Nexus { mod_id: 0, file_id: 0 }.matches(&d));
    }

    #[test]
    fn same_content_skips_the_new_file_itself() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.zip");
        let new = dir.path().join("new.zip");
        std::fs::write(&old, b"hello").unwrap();
        std::fs::write(&new, b"hello").unwrap();
        let rows = vec![keyed(1, &old, 5, "ABCD")];
        assert_eq!(same_content(&rows, "abcd", &new).map(|d| d.id), Some(1));
        assert!(same_content(&rows, "abcd", &old).is_none(), "the same path is not a duplicate of itself");
        assert!(same_content(&rows, "", &new).is_none());
    }

    #[test]
    fn in_flight_blocks_a_second_download_until_the_first_ends() {
        let set = InFlight::default();
        let key = FileKey::Nexus { mod_id: 1, file_id: 2 };
        let first = set.begin(key.clone()).unwrap();
        assert!(set.begin(key.clone()).is_err());
        assert!(set.begin(FileKey::Nexus { mod_id: 1, file_id: 3 }).is_ok());
        drop(first);
        assert!(set.begin(key).is_ok());
    }

    fn row(id: i64, nexus: Option<i64>, name: &str, version: &str, path: &str) -> DownloadRow {
        DownloadRow {
            id,
            nexus_mod_id: nexus,
            file_name: path.rsplit('/').next().unwrap().into(),
            path: path.into(),
            mod_name: Some(name.into()),
            version: Some(version.into()),
            source: "nexus".into(),
            ..Default::default()
        }
    }

    #[test]
    fn channels_from_versions_and_names() {
        let c = |v: &str| channel_of(&[Some(v)], false);
        assert_eq!(c("1.21.0"), Channel::Stable);
        assert_eq!(c("1.0.0-beta.2"), Channel::Beta);
        assert_eq!(c("v2.0 RC1"), Channel::Beta);
        assert_eq!(c("0.9b2"), Channel::Stable, "a word with a version glued on isn't split");
        assert_eq!(c("1.4b"), Channel::Stable);
        assert_eq!(c("Nightly 2026-10-01"), Channel::Nightly);
        assert_eq!(c("1.32.0-dev-snapshot"), Channel::Nightly);
        assert_eq!(c("Alphabet Soup"), Channel::Stable);
        assert_eq!(c("Pre-Release 3"), Channel::Beta);
        assert_eq!(channel_of(&[Some("0.5.27"), Some("redscript-v0.5.27.zip")], true), Channel::Beta);
        assert_eq!(channel_of(&[Some("1.0"), Some("Better Lights Beta")], false), Channel::Beta);
        assert_eq!(channel_of(&[Some("1.0 beta"), Some("Mod nightly")], false), Channel::Nightly, "nightly wins over beta");
    }

    #[test]
    fn groups_versions_of_one_mod_newest_first() {
        let rows = vec![
            row(5, Some(107), "CET", "1.32", "/d/CET/107-3-cet.zip"),
            row(4, Some(4198), "ArchiveXL", "1.21", "/d/ArchiveXL/a.zip"),
            row(3, Some(107), "CET", "1.32", "/d/CET/107-3-cet.zip"), // downloaded twice
            row(2, Some(107), "CET", "1.31-beta", "/d/CET/107-2-cet.zip"),
            DownloadRow { id: 1, file_name: "manual.zip".into(), path: "/d/manual.zip".into(), ..Default::default() },
        ];
        let g = group(rows);
        assert_eq!(g.len(), 3);
        assert_eq!(g[0].name, "CET");
        assert_eq!(g[0].entries.iter().map(|e| e.row.id).collect::<Vec<_>>(), vec![5, 2]);
        assert_eq!(g[0].entries[1].channel, Channel::Beta);
        assert_eq!(g[1].key, "nexus:4198");
        assert_eq!(g[2].name, "manual.zip");
        assert_eq!(g[2].folder, "Other files");
    }

    #[test]
    fn folder_names_are_safe() {
        let d = DownloadRow { nexus_mod_id: Some(9), ..Default::default() };
        assert_eq!(folder_name(Some("Better/Lights: v2 "), &d), "Better_Lights_ v2");
        assert_eq!(folder_name(Some("..hidden"), &d), "hidden");
        assert_eq!(folder_name(Some("  "), &d), "Nexus mod 9");
        let gh = DownloadRow { source: "github".into(), source_ref: Some("psiberx/cp2077-archive-xl".into()), ..Default::default() };
        assert_eq!(folder_name(None, &gh), "cp2077-archive-xl");
    }

    #[test]
    fn moves_files_into_mod_folders() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("a.zip"), b"one").unwrap();
        std::fs::write(old.join("b.zip"), b"two").unwrap();
        // Something else already called b.zip in the target folder.
        std::fs::create_dir_all(new.join("Mod B")).unwrap();
        std::fs::write(new.join("Mod B/b.zip"), b"other").unwrap();
        let p = |n: &str| old.join(n).to_string_lossy().into_owned();
        let rows = vec![
            row(1, Some(1), "Mod A", "1", &p("a.zip")),
            row(2, Some(1), "Mod A", "1", &p("a.zip")),
            row(3, Some(2), "Mod B", "1", &p("b.zip")),
            row(4, Some(3), "Gone", "1", &p("gone.zip")),
        ];
        let moves = plan_moves(&rows, &new);
        assert_eq!(moves.len(), 3);
        let mut updated = Vec::new();
        let r = run_moves(&moves, |m, to| {
            updated.extend(m.ids.iter().map(|id| (*id, to.to_path_buf())));
            Ok(())
        });
        assert_eq!(r.moved, 2);
        assert_eq!(r.missing.len(), 1);
        assert!(r.failed.is_empty());
        assert_eq!(std::fs::read(new.join("Mod A/a.zip")).unwrap(), b"one");
        assert_eq!(std::fs::read(new.join("Mod B/b (2).zip")).unwrap(), b"two");
        assert_eq!(std::fs::read(new.join("Mod B/b.zip")).unwrap(), b"other", "the other file is untouched");
        assert!(updated.contains(&(2, new.join("Mod A/a.zip"))), "both rows for a.zip follow it");
        assert!(!old.join("a.zip").exists());
        // Already in place: nothing to do.
        let rows = vec![row(1, Some(1), "Mod A", "1", &new.join("Mod A/a.zip").to_string_lossy())];
        assert!(plan_moves(&rows, &new).is_empty());
    }

    #[test]
    fn same_file_already_at_target_is_reused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.zip"), b"same").unwrap();
        std::fs::create_dir_all(dir.path().join("M")).unwrap();
        std::fs::write(dir.path().join("M/x.zip"), b"same").unwrap();
        let to = move_file(&dir.path().join("x.zip"), &dir.path().join("M/x.zip")).unwrap();
        assert_eq!(to, dir.path().join("M/x.zip"));
        assert!(!dir.path().join("x.zip").exists());
    }
}
