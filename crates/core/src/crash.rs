//! Crash and load-failure analysis: read the logs the game and the modding
//! frameworks write, pull out the errors, and point at the installed mod each
//! one most likely comes from.
//!
//! Sources: Cyber Engine Tweaks (and per-mod CET logs), RED4ext and its
//! plugin logs (ArchiveXL, TweakXL, Codeware), the redscript compiler log,
//! the game's crash reports in the Proton prefix (`REDEngine/ReportQueue`),
//! and Proton's own `steam-1091500.log` when `PROTON_LOG=1` is set.

use std::collections::BTreeSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::db::{Db, GameRow};
use crate::install::resolve_ci;
use crate::{Result, STEAM_APP_ID};

/// Lines read from the end of each log.
const SCAN_LINES: usize = 3000;

#[derive(Debug, Clone, Serialize)]
pub struct LogFile {
    /// Stable display name, e.g. `r6/logs/redscript_rCURRENT.log` or
    /// `crash-report/<folder>/<file>`.
    pub name: String,
    #[serde(skip)]
    pub path: PathBuf,
    pub size: u64,
    pub modified_unix: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Error,
    Warning,
}

#[derive(Debug, Clone, Serialize)]
pub struct Issue {
    pub level: Level,
    pub log: String,
    pub line: String,
    /// Mods whose files or plugin names appear in the line.
    pub mod_ids: Vec<i64>,
    pub mod_names: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CrashReport {
    pub logs: Vec<LogFile>,
    pub issues: Vec<Issue>,
    /// Newest crash report folder in the prefix, with its time.
    pub latest_crash: Option<LogFile>,
    /// Mods mentioned by errors, most mentions first.
    pub suspects: Vec<(String, usize)>,
}

/// Proton prefix for a Steam install: `<library>/steamapps/compatdata/1091500/pfx`.
pub fn proton_prefix(game_dir: &Path) -> Option<PathBuf> {
    let steamapps = game_dir.parent()?.parent()?;
    let pfx = steamapps.join("compatdata").join(STEAM_APP_ID).join("pfx");
    pfx.is_dir().then_some(pfx)
}

fn file_info(name: String, path: PathBuf) -> Option<LogFile> {
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let modified_unix = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    Some(LogFile { name, path, size: meta.len(), modified_unix })
}

fn crash_report_dirs(game_dir: &Path) -> Vec<PathBuf> {
    let Some(pfx) = proton_prefix(game_dir) else { return vec![] };
    let users = pfx.join("drive_c/users");
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&users) else { return out };
    for user in rd.flatten() {
        let queue = resolve_ci(&user.path(), "AppData/Local/REDEngine/ReportQueue");
        if let Ok(reports) = std::fs::read_dir(&queue) {
            out.extend(reports.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
        }
    }
    out
}

/// Every log we know how to find for this install. Names are fixed patterns,
/// never caller-supplied paths, so this list doubles as an allow-list.
pub fn known_logs(game_dir: &Path) -> Vec<LogFile> {
    let mut out: Vec<LogFile> = Vec::new();
    let push = |out: &mut Vec<LogFile>, name: String, path: PathBuf| {
        if !out.iter().any(|l| l.name.eq_ignore_ascii_case(&name))
            && let Some(f) = file_info(name, path) {
                out.push(f);
            }
    };
    let fixed = [
        "bin/x64/plugins/cyber_engine_tweaks/cyber_engine_tweaks.log",
        "bin/x64/plugins/cyber_engine_tweaks/scripting.log",
        "r6/logs/redscript_rCURRENT.log",
        "red4ext/logs/red4ext.log",
        "red4ext/plugins/ArchiveXL/ArchiveXL.log",
        "red4ext/plugins/TweakXL/TweakXL.log",
        "red4ext/plugins/Codeware/Codeware.log",
    ];
    for rel in fixed {
        push(&mut out, rel.to_string(), resolve_ci(game_dir, rel));
    }
    // Per-plugin RED4ext logs and per-mod CET logs, one level deep.
    for dir in ["red4ext/logs", "bin/x64/plugins/cyber_engine_tweaks/mods"] {
        let base = resolve_ci(game_dir, dir);
        let Ok(rd) = std::fs::read_dir(&base) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let candidates: Vec<PathBuf> = if p.is_dir() {
                std::fs::read_dir(&p).map(|r| r.flatten().map(|x| x.path()).collect()).unwrap_or_default()
            } else {
                vec![p]
            };
            for c in candidates {
                if c.extension().is_some_and(|x| x.eq_ignore_ascii_case("log"))
                    && let Ok(rel) = c.strip_prefix(game_dir) {
                        push(&mut out, rel.to_string_lossy().replace('\\', "/"), c.clone());
                    }
            }
        }
    }
    // Text files in the game's crash reports.
    for dir in crash_report_dirs(game_dir) {
        let folder = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let ext = p.extension().map(|x| x.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
            if matches!(ext.as_str(), "txt" | "log" | "json" | "xml") {
                let fname = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                push(&mut out, format!("crash-report/{folder}/{fname}"), p);
            }
        }
    }
    // Proton's log lands in $HOME when PROTON_LOG=1.
    if let Some(home) = dirs::home_dir() {
        push(&mut out, format!("steam-{STEAM_APP_ID}.log"), home.join(format!("steam-{STEAM_APP_ID}.log")));
    }
    out
}

/// One log from [`known_logs`] by its display name. Only names that list
/// returns can be read or opened, never a path from the caller.
pub fn find_log(game_dir: &Path, name: &str) -> Result<LogFile> {
    known_logs(game_dir)
        .into_iter()
        .find(|l| l.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| crate::Error::Other(format!("unknown log `{name}`")))
}

pub fn tail_lines(path: &Path, max: usize) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    // Read at most the last 4 MiB.
    let start = len.saturating_sub(4 * 1024 * 1024);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    let lines: Vec<&str> = text.lines().collect();
    let from = lines.len().saturating_sub(max);
    Ok(lines[from..].join("\n"))
}

/// Classify one log line. Proton logs are huge and noisy, so only clear
/// failures count there.
pub fn classify(log: &str, line: &str) -> Option<Level> {
    let l = line.to_ascii_lowercase();
    if log.starts_with("steam-") {
        return (l.contains("unhandled exception") || l.contains("page fault") || l.contains("fatal")).then_some(Level::Error);
    }
    let error_words = ["[error", "error:", " error ", "exception", "failed", "fatal", "crash", "could not", "couldn't", "unable to", "access violation"];
    let warn_words = ["[warn", "warning"];
    if error_words.iter().any(|w| l.contains(w)) {
        // Lines that merely report zero errors aren't problems.
        if l.contains("0 error") || l.contains("no error") {
            return None;
        }
        return Some(Level::Error);
    }
    if warn_words.iter().any(|w| l.contains(w)) {
        return Some(Level::Warning);
    }
    None
}

/// Needles that identify a mod in log lines: its file names, script and CET
/// folder names, and RED4ext plugin names. Very short or generic names are
/// skipped to avoid false matches.
fn mod_needles(db: &Db, mod_id: i64) -> Result<BTreeSet<String>> {
    const GENERIC: &[&str] = &["init.lua", "main.reds", "config.json", "settings.json", "info.json", "readme.txt", "modules", "scripts", "plugins", "mods"];
    let mut out = BTreeSet::new();
    for f in db.mod_files(mod_id)? {
        let lower = f.rel_path.to_lowercase();
        let parts: Vec<&str> = lower.split('/').collect();
        if let Some(name) = parts.last() {
            out.insert(name.to_string());
        }
        // Folder that identifies the mod: r6/scripts/<X>, red4ext/plugins/<X>,
        // cyber_engine_tweaks/mods/<X>, mods/<X>.
        for (i, p) in parts.iter().enumerate() {
            let next = parts.get(i + 1);
            let is_anchor = matches!(*p, "scripts" | "tweaks" | "plugins" | "mods") && parts.len() > i + 2;
            if is_anchor
                && let Some(n) = next {
                    out.insert(n.to_string());
                }
        }
    }
    Ok(out.into_iter().filter(|n| n.len() >= 5 && !GENERIC.contains(&n.as_str())).collect())
}

fn normalize(line: &str) -> String {
    line.to_lowercase().replace('\\', "/")
}

pub fn analyze(db: &Db, game: &GameRow) -> Result<CrashReport> {
    let game_dir = Path::new(&game.path);
    let logs = known_logs(game_dir);
    // Disabled mods aren't in the game, so they can't be behind a new error.
    let mods: Vec<_> = db.mods(game.id)?.into_iter().filter(|m| m.enabled()).collect();
    let mut needles: Vec<(i64, String, BTreeSet<String>)> = Vec::new();
    for m in &mods {
        needles.push((m.id, m.name.clone(), mod_needles(db, m.id)?));
    }

    let mut issues = Vec::new();
    for log in &logs {
        let Ok(text) = tail_lines(&log.path, SCAN_LINES) else { continue };
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for line in text.lines() {
            let Some(level) = classify(&log.name, line) else { continue };
            let trimmed: String = line.trim().chars().take(400).collect();
            // Repeated lines (e.g. the same warning every frame) count once.
            if !seen.insert(trimmed.clone()) {
                continue;
            }
            let norm = normalize(line);
            let mut hit_ids = Vec::new();
            let mut hit_names = Vec::new();
            for (id, name, ns) in &needles {
                if ns.iter().any(|n| norm.contains(n.as_str())) || (name.len() >= 5 && norm.contains(&name.to_lowercase())) {
                    hit_ids.push(*id);
                    hit_names.push(name.clone());
                }
            }
            // A per-mod CET log belongs to that mod even when lines don't
            // name it.
            if hit_ids.is_empty() && log.name.contains("cyber_engine_tweaks/mods/") {
                let folder = log.name.split('/').nth(5).unwrap_or("").to_lowercase();
                for (id, name, ns) in &needles {
                    if !folder.is_empty() && ns.contains(&folder) {
                        hit_ids.push(*id);
                        hit_names.push(name.clone());
                    }
                }
            }
            issues.push(Issue { level, log: log.name.clone(), line: trimmed, mod_ids: hit_ids, mod_names: hit_names });
        }
    }
    // Errors first; keep the report a readable size.
    issues.sort_by_key(|i| i.level);
    issues.truncate(500);

    let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
    for i in issues.iter().filter(|i| i.level == Level::Error) {
        for n in &i.mod_names {
            *counts.entry(n.clone()).or_default() += 1;
        }
    }
    let mut suspects: Vec<(String, usize)> = counts.into_iter().collect();
    suspects.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    let latest_crash = logs
        .iter()
        .filter(|l| l.name.starts_with("crash-report/"))
        .max_by_key(|l| l.modified_unix)
        .cloned();
    Ok(CrashReport { logs, issues, latest_crash, suspects })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{ModFile, NewMod};
    use crate::game::{GameInstall, Store};

    #[test]
    fn classifies_lines() {
        assert_eq!(classify("r6/logs/redscript_rCURRENT.log", "[ERROR - Fri] At r6\\scripts\\x.reds:3:1: unresolved"), Some(Level::Error));
        assert_eq!(classify("red4ext/logs/red4ext.log", "[warning] Plugin 'X' is outdated"), Some(Level::Warning));
        assert_eq!(classify("r6/logs/redscript_rCURRENT.log", "Compilation complete, 0 errors"), None);
        assert_eq!(classify("steam-1091500.log", "err:module:import_dll Library x.dll not found"), None);
        assert_eq!(classify("steam-1091500.log", "wine: Unhandled page fault on read access"), Some(Level::Error));
    }

    #[test]
    fn attributes_errors_to_mods() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("SteamLibrary/steamapps");
        let game_dir = lib.join("common/Cyberpunk 2077");
        std::fs::create_dir_all(game_dir.join("r6/logs")).unwrap();
        std::fs::create_dir_all(game_dir.join("red4ext/logs")).unwrap();
        std::fs::write(
            game_dir.join("r6/logs/redscript_rCURRENT.log"),
            "[INFO] Compiling\n[ERROR - x] At C:\\Games\\Cyberpunk 2077\\r6\\scripts\\BetterHair\\hair.reds:2:5:\n  unresolved type 'Foo'\n[ERROR - x] At C:\\Games\\Cyberpunk 2077\\r6\\scripts\\BetterHair\\hair.reds:2:5:\n",
        )
        .unwrap();
        std::fs::write(game_dir.join("red4ext/logs/red4ext.log"), "[info] loaded\n[error] Could not load plugin 'SuperPlugin': incompatible game version\n").unwrap();
        let report_dir = lib.join("compatdata/1091500/pfx/drive_c/users/steamuser/AppData/Local/REDEngine/ReportQueue/abc-123");
        std::fs::create_dir_all(&report_dir).unwrap();
        std::fs::write(report_dir.join("metadata.txt"), "Error reason: Unhandled exception\nExpression: EXCEPTION_ACCESS_VIOLATION\n").unwrap();

        let db = Db::open_in_memory().unwrap();
        let gid = db
            .upsert_game(&GameInstall {
                path: game_dir.clone(),
                store: Store::Steam,
                proton_prefix: None,
                build_id: None,
                exe_file_version: None,
                exe_product_version: None,
                frameworks: vec![],
                launch_options: None,
                warnings: vec![],
            })
            .unwrap();
        let game = db.game(gid).unwrap();
        let add = |name: &str, files: &[&str]| {
            let id = db.insert_mod(&game, &NewMod { name: name.into(), source: "manual".into(), ..Default::default() }).unwrap();
            for f in files {
                db.add_mod_file(&ModFile { mod_id: id, rel_path: f.to_string(), staged_path: f.to_string(), sha256: String::new(), size: 0 }).unwrap();
            }
            id
        };
        let hair = add("Better Hair", &["r6/scripts/BetterHair/hair.reds"]);
        let plug = add("Super Plugin", &["red4ext/plugins/SuperPlugin/SuperPlugin.dll"]);
        add("Unrelated", &["archive/pc/mod/neon.archive"]);

        let r = analyze(&db, &game).unwrap();
        assert!(r.logs.iter().any(|l| l.name == "crash-report/abc-123/metadata.txt"));
        assert_eq!(r.latest_crash.as_ref().unwrap().name, "crash-report/abc-123/metadata.txt");
        let reds: Vec<&Issue> = r.issues.iter().filter(|i| i.log.contains("redscript")).collect();
        assert_eq!(reds.len(), 1, "duplicate lines collapse");
        assert_eq!(reds[0].mod_ids, vec![hair]);
        let r4: &Issue = r.issues.iter().find(|i| i.log.contains("red4ext")).unwrap();
        assert_eq!(r4.mod_ids, vec![plug]);
        assert!(r.issues.iter().any(|i| i.log.starts_with("crash-report/") && i.line.contains("ACCESS_VIOLATION")));
        assert_eq!(r.suspects[0].1, 1);
        assert!(!r.suspects.iter().any(|(n, _)| n == "Unrelated"));
    }
}
