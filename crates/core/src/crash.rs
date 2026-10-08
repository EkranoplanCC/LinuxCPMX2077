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
use crate::known_issues::{self, KnownIssue};
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
    /// Written during the last game session. When the session can't be
    /// told apart, every issue counts as current.
    pub last_session: bool,
    /// When the line was written, if the log stamps its lines.
    pub time_unix: Option<i64>,
    /// Plain-language meaning, when it's a known message.
    pub meaning: Option<String>,
    pub fix: Option<String>,
}

/// The last time the game ran, worked out from the framework logs: RED4ext,
/// CET and redscript all start a fresh log (or a fresh block) at launch.
#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub started_unix: i64,
    /// Newest write to any log in this session.
    pub last_write_unix: i64,
    /// A crash report was written during this session.
    pub crashed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CrashReport {
    pub logs: Vec<LogFile>,
    /// Last session first, errors before warnings.
    pub issues: Vec<Issue>,
    /// Newest crash report folder in the prefix, with its time.
    pub latest_crash: Option<LogFile>,
    /// Mods mentioned by errors in the last session, most mentions first.
    pub suspects: Vec<(String, usize)>,
    pub session: Option<Session>,
    /// What happened at the last start, step by step.
    pub timeline: Vec<crate::startup::Step>,
    /// Known problem mods and messages from the modding wiki.
    pub known_issues: Vec<KnownIssue>,
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

/// Crash report folders: in every user of the Proton prefix, or in the
/// user's own `%LOCALAPPDATA%` when the game runs on Windows.
fn crash_report_dirs(game_dir: &Path) -> Vec<PathBuf> {
    let mut queues: Vec<PathBuf> = crate::winsys::crash_report_queue().into_iter().collect();
    if let Some(pfx) = proton_prefix(game_dir)
        && let Ok(rd) = std::fs::read_dir(pfx.join("drive_c/users"))
    {
        queues.extend(rd.flatten().map(|user| resolve_ci(&user.path(), "AppData/Local/REDEngine/ReportQueue")));
    }
    let mut out = Vec::new();
    for queue in queues {
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
    // RED4ext logs (named by date in newer versions), per-plugin logs
    // (ArchiveXL-<date>.log and other plugins' own logs) and per-mod CET
    // logs, one level deep.
    for dir in ["red4ext/logs", "red4ext/plugins", "bin/x64/plugins/cyber_engine_tweaks/mods"] {
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
    if let Some(home) = dirs::home_dir().filter(|_| !cfg!(windows)) {
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

/// A timestamp found at the start of a log line, in seconds since the epoch
/// if the line gave its time zone (`zoned`), else in local wall-clock
/// seconds that still need the local offset taken off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub secs: i64,
    pub zoned: bool,
}

fn month(name: &str) -> Option<i64> {
    const M: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let n = name.get(..3)?.to_ascii_lowercase();
    M.iter().position(|m| *m == n).map(|i| i as i64 + 1)
}

/// Read the time at the start of a log line. Handles the formats the
/// frameworks write:
/// `[2023-04-15 12:04:18.956]` (RED4ext, CET), `[2023-04-14 20:28:18 UTC+01:00]`
/// (ArchiveXL, TweakXL) and `[ERROR - Thu, 13 Apr 2023 21:54:13 +0200]`
/// (redscript).
pub fn line_time(line: &str) -> Option<Stamp> {
    let head: String = line.chars().take(80).collect();
    let b = head.as_bytes();
    let digit = |i: usize| b.get(i).is_some_and(|c| c.is_ascii_digit());
    // ISO date and time.
    for i in 0..b.len().saturating_sub(18) {
        if (0..4).all(|k| digit(i + k)) && b[i + 4] == b'-' && digit(i + 5) && digit(i + 6) && b[i + 7] == b'-'
            && digit(i + 8) && digit(i + 9) && (b[i + 10] == b' ' || b[i + 10] == b'T')
            && digit(i + 11) && digit(i + 12) && b[i + 13] == b':' && b[i + 16] == b':'
        {
            let base = &head[i..i + 19];
            let mut rest = &head[i + 19..];
            if let Some(r) = rest.strip_prefix('.') {
                rest = r.trim_start_matches(|c: char| c.is_ascii_digit());
            }
            let rest = rest.trim_start();
            let rest = rest.strip_prefix("UTC").unwrap_or(rest);
            let zone: String = rest.chars().take_while(|c| matches!(c, '+' | '-' | ':' | 'Z') || c.is_ascii_digit()).collect();
            if (zone.starts_with('+') || zone.starts_with('-') || zone == "Z")
                && let Some(t) = crate::nexus::parse_time(&format!("{base} {zone}")) {
                    return Some(Stamp { secs: t, zoned: true });
                }
            return crate::nexus::parse_time(base).map(|t| Stamp { secs: t, zoned: false });
        }
    }
    // RFC 2822: `Thu, 13 Apr 2023 21:54:13 +0200`.
    let comma = head.find(", ")?;
    let parts: Vec<&str> = head[comma + 2..].split_whitespace().take(5).collect();
    if parts.len() < 4 {
        return None;
    }
    let day: i64 = parts[0].parse().ok()?;
    let mo = month(parts[1])?;
    let year: i64 = parts[2].parse().ok()?;
    let time = parts[3];
    let iso = format!("{year:04}-{mo:02}-{day:02} {time}");
    let zone = parts.get(4).map(|z| z.trim_end_matches(']')).filter(|z| z.starts_with('+') || z.starts_with('-'));
    match zone {
        Some(z) => crate::nexus::parse_time(&format!("{iso} {z}")).map(|t| Stamp { secs: t, zoned: true }),
        None => crate::nexus::parse_time(&iso).map(|t| Stamp { secs: t, zoned: false }),
    }
}

/// Converts log time stamps to Unix time.
#[derive(Debug, Clone, Copy, Default)]
pub struct Clock {
    /// Local time minus UTC, in seconds, for stamps without a zone.
    pub offset: i64,
}

impl Clock {
    pub fn unix(&self, s: Stamp) -> i64 {
        if s.zoned { s.secs } else { s.secs - self.offset }
    }

    /// Work out the local offset from a log whose last line has a zoneless
    /// stamp: that line was written at (about) the file's modification time.
    /// Rounded to 15 minutes, as time zones are.
    fn learn(text: &str, modified_unix: i64) -> Option<Clock> {
        let last = text.lines().rev().take(50).find_map(line_time)?;
        if last.zoned {
            return None;
        }
        let raw = last.secs - modified_unix;
        let offset = (raw as f64 / 900.0).round() as i64 * 900;
        // A log edited long after its last line says nothing about the zone.
        ((raw - offset).abs() < 600 && offset.abs() <= 14 * 3600).then_some(Clock { offset })
    }
}

fn head_text(path: &Path, max_bytes: usize) -> String {
    let Ok(mut f) = std::fs::File::open(path) else { return String::new() };
    let mut buf = vec![0; max_bytes];
    let n = f.read(&mut buf).unwrap_or(0);
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

/// The newest main RED4ext log (`red4ext.log` or `red4ext-<date>.log`).
pub(crate) fn main_red4ext_log(logs: &[LogFile]) -> Option<&LogFile> {
    logs.iter()
        .filter(|l| {
            let n = l.name.to_ascii_lowercase();
            n.starts_with("red4ext/logs/red4ext") && !n["red4ext/logs/".len()..].contains('/')
        })
        .max_by_key(|l| l.modified_unix)
}

pub(crate) const CET_LOG: &str = "bin/x64/plugins/cyber_engine_tweaks/cyber_engine_tweaks.log";
pub(crate) const REDSCRIPT_LOG: &str = "r6/logs/redscript_rCURRENT.log";

/// Logs that are started fresh at every launch.
fn launch_logs(logs: &[LogFile]) -> Vec<&LogFile> {
    let mut out: Vec<&LogFile> = main_red4ext_log(logs).into_iter().collect();
    out.extend(logs.iter().filter(|l| l.name.eq_ignore_ascii_case(CET_LOG) || l.name.eq_ignore_ascii_case(REDSCRIPT_LOG)));
    out
}

/// Find the last session: the latest launch-log start, a little early so
/// lines written in the same second by another framework still count.
fn find_session(logs: &[LogFile]) -> (Clock, Option<i64>) {
    let launch = launch_logs(logs);
    let mut clock = Clock::default();
    for l in &launch {
        let Some(m) = l.modified_unix else { continue };
        if let Ok(tail) = tail_lines(&l.path, 50)
            && let Some(c) = Clock::learn(&tail, m as i64) {
                clock = c;
                break;
            }
    }
    let mut start: Option<i64> = None;
    for l in &launch {
        let Some(m) = l.modified_unix else { continue };
        let first = head_text(&l.path, 16 * 1024).lines().take(50).find_map(line_time);
        // Only trust a start that isn't after the file's last write.
        let t = first.map(|s| clock.unix(s)).filter(|t| *t <= m as i64 + 3600);
        if let Some(t) = t {
            start = Some(start.map_or(t, |s: i64| s.max(t)));
        }
    }
    (clock, start.map(|s| s - 120))
}

/// Matches a normalized log line against each mod's needles.
fn mods_named(norm: &str, needles: &[(i64, String, BTreeSet<String>)]) -> (Vec<i64>, Vec<String>) {
    let mut ids = Vec::new();
    let mut names = Vec::new();
    for (id, name, ns) in needles {
        if ns.iter().any(|n| norm.contains(n.as_str())) || (name.len() >= 5 && norm.contains(&name.to_lowercase())) {
            ids.push(*id);
            names.push(name.clone());
        }
    }
    (ids, names)
}

/// A redscript error is a header line (`At …\x.reds:2:5:`) followed by the
/// reason on the next lines. Joins them so the reason isn't lost.
fn is_reds_header(line: &str) -> bool {
    let t = line.trim_end();
    t.ends_with(':') && t.to_ascii_lowercase().contains(".reds:")
}

/// One log line (or joined redscript block) with the time it was written.
struct Entry {
    text: String,
    time: Option<i64>,
}

/// Split a log into entries, carrying the last stamp forward to lines
/// without one.
fn entries(log: &str, text: &str, clock: Clock) -> Vec<Entry> {
    let lines: Vec<&str> = text.lines().collect();
    let reds = log.to_ascii_lowercase().contains("redscript");
    let mut out = Vec::new();
    let mut last: Option<i64> = None;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(s) = line_time(line) {
            last = Some(clock.unix(s));
        }
        let mut text = line.trim().to_string();
        i += 1;
        if reds && is_reds_header(line) {
            let mut extra = 0;
            while i < lines.len() && extra < 4 && !lines[i].trim_start().starts_with('[') {
                let t = lines[i].trim();
                // The caret line under the code adds nothing.
                if !t.is_empty() && !t.chars().all(|c| c == '^' || c == '~') {
                    text.push_str(" | ");
                    text.push_str(t);
                }
                i += 1;
                extra += 1;
            }
        }
        out.push(Entry { text, time: last });
    }
    out
}

pub(crate) struct Scanned<'a> {
    pub log: &'a LogFile,
    /// Lines from the last session only (all lines when the session is unknown).
    pub session_text: String,
    pub in_session: bool,
}

pub fn analyze(db: &Db, game: &GameRow) -> Result<CrashReport> {
    let game_dir = Path::new(&game.path);
    let logs = known_logs(game_dir);
    // Disabled mods aren't in the game, so they can't be behind a new error.
    let mods: Vec<_> = db.mods(game.id)?.into_iter().filter(|m| m.enabled()).collect();
    let mut needles: Vec<(i64, String, BTreeSet<String>)> = Vec::new();
    let mut infos = Vec::new();
    for m in &mods {
        needles.push((m.id, m.name.clone(), mod_needles(db, m.id)?));
        infos.push(known_issues::ModInfo {
            id: m.id,
            name: m.name.clone(),
            nexus_mod_id: m.nexus_mod_id,
            files: db.mod_files(m.id)?.into_iter().map(|f| f.rel_path.to_lowercase()).collect(),
        });
    }
    let (clock, start) = find_session(&logs);

    let mut issues: Vec<Issue> = Vec::new();
    let mut scanned = Vec::new();
    for log in &logs {
        let Ok(text) = tail_lines(&log.path, SCAN_LINES) else { continue };
        let file_current = match (start, log.modified_unix) {
            (Some(s), Some(m)) => m as i64 >= s,
            (Some(_), None) => false,
            (None, _) => true,
        };
        let mut session_text = String::new();
        let mut seen: std::collections::HashMap<String, usize> = Default::default();
        for e in entries(&log.name, &text, clock) {
            let current = file_current && match (start, e.time) {
                (Some(s), Some(t)) => t >= s,
                _ => true,
            };
            if current {
                session_text.push_str(&e.text);
                session_text.push('\n');
            }
            let Some(level) = classify(&log.name, &e.text) else { continue };
            if known_issues::is_noise(&e.text) {
                continue;
            }
            let trimmed: String = e.text.chars().take(600).collect();
            // Repeated lines (e.g. the same warning every frame) count once,
            // as recent as they last happened.
            if let Some(&k) = seen.get(&trimmed) {
                let i: &mut Issue = &mut issues[k];
                i.last_session |= current;
                i.time_unix = e.time.or(i.time_unix);
                continue;
            }
            let norm = normalize(&e.text);
            let (mut hit_ids, mut hit_names) = mods_named(&norm, &needles);
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
            let hint = known_issues::explain(&e.text);
            seen.insert(trimmed.clone(), issues.len());
            issues.push(Issue {
                level,
                log: log.name.clone(),
                line: trimmed,
                mod_ids: hit_ids,
                mod_names: hit_names,
                last_session: current,
                time_unix: e.time.or(log.modified_unix.map(|m| m as i64)),
                meaning: hint.map(|h| h.meaning.clone()),
                fix: hint.map(|h| h.fix.clone()),
            });
        }
        scanned.push(Scanned { log, session_text, in_session: file_current });
    }
    // Last session first, then errors before warnings; keep the report a
    // readable size.
    issues.sort_by_key(|i| (!i.last_session, i.level));
    issues.truncate(500);

    let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
    for i in issues.iter().filter(|i| i.level == Level::Error && i.last_session) {
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
    let session = start.map(|s| {
        let in_session = |l: &&LogFile| l.modified_unix.is_some_and(|m| m as i64 >= s);
        Session {
            started_unix: s + 120,
            last_write_unix: logs.iter().filter(in_session).filter_map(|l| l.modified_unix).max().map_or(s + 120, |m| m as i64),
            crashed: logs.iter().filter(in_session).any(|l| l.name.starts_with("crash-report/")),
        }
    });

    let frameworks = crate::game::detect_frameworks(game_dir);
    let timeline = crate::startup::timeline(&crate::startup::Input {
        game_dir,
        frameworks: &frameworks,
        scanned: &scanned,
        issues: &issues,
        needles: &needles,
        session: session.as_ref(),
    });

    let session_lines: Vec<String> = issues.iter().filter(|i| i.last_session).map(|i| i.line.clone()).collect();
    let mut known_issues = known_issues::check(&known_issues::Context { game_dir, mods: &infos, log_lines: &session_lines });
    known_issues.extend(crate::ultraplus::known_issues(game_dir, &infos, &crate::ultraplus::saved(db)));
    known_issues.sort_by_key(|i| i.severity);
    Ok(CrashReport { logs, issues, latest_crash, suspects, session, timeline, known_issues })
}

/// Mods named in a line, for the startup timeline.
pub(crate) fn mods_in(line: &str, needles: &[(i64, String, BTreeSet<String>)]) -> (Vec<i64>, Vec<String>) {
    mods_named(&normalize(line), needles)
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
            "[INFO] Compiling\n[ERROR - x] At C:\\Games\\Cyberpunk 2077\\r6\\scripts\\BetterHair\\hair.reds:2:5:\n  let a: Foo;\n  ^^^\n  unresolved type 'Foo'\n[ERROR - x] At C:\\Games\\Cyberpunk 2077\\r6\\scripts\\BetterHair\\hair.reds:2:5:\n  let a: Foo;\n  ^^^\n  unresolved type 'Foo'\n",
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
        // The reason on the lines after the header is kept, and explained.
        assert!(reds[0].line.contains("unresolved type 'Foo'"), "{}", reds[0].line);
        assert!(reds[0].meaning.is_some());
        let r4: &Issue = r.issues.iter().find(|i| i.log.contains("red4ext")).unwrap();
        assert_eq!(r4.mod_ids, vec![plug]);
        assert!(r.issues.iter().any(|i| i.log.starts_with("crash-report/") && i.line.contains("ACCESS_VIOLATION")));
        assert_eq!(r.suspects[0].1, 1);
        assert!(!r.suspects.iter().any(|(n, _)| n == "Unrelated"));
    }

    #[test]
    fn reads_log_time_stamps() {
        let t = |s: &str| line_time(s).unwrap();
        let naive = t("[2023-04-15 12:04:18.956] [error] |Something| Some Explanation");
        assert!(!naive.zoned);
        assert_eq!(naive.secs, crate::nexus::parse_time("2023-04-15 12:04:18").unwrap());
        let zoned = t("[2023-04-14 20:28:18 UTC+01:00] [1234] [ErrorSource] DoSomething(): Error !");
        assert!(zoned.zoned);
        assert_eq!(zoned.secs, crate::nexus::parse_time("2023-04-14 19:28:18").unwrap());
        let reds = t("[WARN - Thu, 13 Apr 2023 21:54:13 +0200] At C:\\x.reds:1:1:");
        assert!(reds.zoned);
        assert_eq!(reds.secs, crate::nexus::parse_time("2023-04-13 19:54:13").unwrap());
        assert!(line_time("no time here").is_none());
    }

    fn set_mtime(p: &Path, iso: &str) {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(crate::nexus::parse_time(iso).unwrap() as u64);
        std::fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
    }

    #[test]
    fn sorts_by_session_and_builds_the_timeline() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("SteamLibrary/steamapps");
        let game_dir = lib.join("common/Cyberpunk 2077");
        for d in ["red4ext/logs", "red4ext/plugins/SuperPlugin", "r6/logs", "engine/tools"] {
            std::fs::create_dir_all(game_dir.join(d)).unwrap();
        }
        std::fs::write(game_dir.join("red4ext/RED4ext.dll"), b"").unwrap();
        std::fs::write(game_dir.join("engine/tools/scc.exe"), b"").unwrap();
        // A RED4ext log from a launch in January, with an old error.
        let old = game_dir.join("red4ext/logs/red4ext-2026-01-01-10-00-00.log");
        std::fs::write(&old, "[2026-01-01 10:00:00.000] [RED4ext] [info] RED4ext (v1.20.0) is initializing...\n[2026-01-01 10:00:01.000] [RED4ext] [error] Could not load plugin 'OldPlugin'\n").unwrap();
        set_mtime(&old, "2026-01-01 10:00:01");
        // Today's launch died while loading a plugin.
        let new = game_dir.join("red4ext/logs/red4ext-2026-10-08-12-00-00.log");
        std::fs::write(&new, "[2026-10-08 12:00:00.000] [RED4ext] [info] RED4ext (v1.29.1) is initializing...\n[2026-10-08 12:00:01.000] [RED4ext] [warning] Plugin 'Shiny' is outdated\n[2026-10-08 12:00:02.000] [RED4ext] [info] Loading plugin from 'C:\\Games\\Cyberpunk 2077\\red4ext\\plugins\\SuperPlugin\\SuperPlugin.dll'...\n").unwrap();
        set_mtime(&new, "2026-10-08 12:00:30");
        // A redscript log left over from January.
        let reds = game_dir.join("r6/logs/redscript_rCURRENT.log");
        std::fs::write(&reds, "[ERROR - Thu, 01 Jan 2026 10:00:05 +0000] At C:\\Games\\Cyberpunk 2077\\r6\\scripts\\Old\\old.reds:1:1:\n  unresolved type 'Bar'\n").unwrap();
        set_mtime(&reds, "2026-01-01 10:00:05");
        // A crash report from today.
        let report = lib.join("compatdata/1091500/pfx/drive_c/users/steamuser/AppData/Local/REDEngine/ReportQueue/r1");
        std::fs::create_dir_all(&report).unwrap();
        std::fs::write(report.join("stacktrace.txt"), "Error reason: Unhandled exception\nExpression: EXCEPTION_ACCESS_VIOLATION (0xC0000005)\n").unwrap();
        set_mtime(&report.join("stacktrace.txt"), "2026-10-08 12:00:40");

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
        let plug = db.insert_mod(&game, &NewMod { name: "Super Plugin".into(), source: "manual".into(), ..Default::default() }).unwrap();
        db.add_mod_file(&ModFile { mod_id: plug, rel_path: "red4ext/plugins/SuperPlugin/SuperPlugin.dll".into(), staged_path: "x".into(), sha256: String::new(), size: 0 }).unwrap();

        let r = analyze(&db, &game).unwrap();
        let sess = r.session.as_ref().expect("session found");
        assert_eq!(sess.started_unix, crate::nexus::parse_time("2026-10-08 12:00:00").unwrap());
        assert!(sess.crashed);
        let old_issue = r.issues.iter().find(|i| i.line.contains("OldPlugin")).unwrap();
        assert!(!old_issue.last_session);
        assert!(!r.issues.iter().find(|i| i.line.contains("unresolved type 'Bar'")).unwrap().last_session);
        let shiny = r.issues.iter().position(|i| i.line.contains("Shiny")).unwrap();
        let old_pos = r.issues.iter().position(|i| i.line.contains("OldPlugin")).unwrap();
        assert!(shiny < old_pos, "last session sorts first even for warnings");
        assert!(r.issues[0].last_session);

        let step = |id: &str| r.timeline.iter().find(|s| s.id == id).unwrap();
        use crate::startup::Status;
        assert_eq!(step("red4ext").status, Status::Warning);
        let p = step("plugins");
        assert_eq!(p.status, Status::Failed);
        assert!(p.first_problem);
        assert!(p.summary.contains("SuperPlugin.dll"), "{}", p.summary);
        assert_eq!(p.mod_ids, vec![plug]);
        // Old redscript errors don't count against today's start.
        assert_ne!(step("redscript").status, Status::Failed);
        let c = step("crash");
        assert_eq!(c.status, Status::Failed);
        assert!(c.summary.contains("memory"), "{}", c.summary);
        assert_eq!(r.timeline.iter().filter(|s| s.first_problem).count(), 1);
    }
}
