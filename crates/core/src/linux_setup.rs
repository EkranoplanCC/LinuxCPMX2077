//! What the game needs on Linux before mods load, checked and fixed in
//! place: the Visual C++ runtime in the Wine/Proton prefix, the `winmm` and
//! `version` DLL overrides that let CET and RED4ext load, `-modded` for
//! REDmod, and mod folders that exist twice with different capitalisation.
//!
//! Every fix is offered, never applied on its own: the user confirms it, a
//! record of what changed is kept under the app's data dir, and Undo puts
//! things back. Steam's settings are only written while Steam is closed,
//! because Steam rewrites `localconfig.vdf` when it quits.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::desktop::user_command;
use crate::game::{self, GameInstall, Store};
use crate::install::{KNOWN_ROOTS, resolve_ci};
use crate::{Error, Result, STEAM_APP_ID, vdf};

/// The DLL overrides CET (`version`) and RED4ext (`winmm`) need under Wine.
pub const OVERRIDE_VALUE: &str = "winmm,version=n,b";
/// Oldest `msvcp140.dll` (major, minor) that current CET and RED4ext builds
/// load with: MSVC 17.10 changed std::mutex in a way older runtimes crash on
/// (error 998 at startup).
pub const MIN_VC_RUNTIME: (u16, u16) = (14, 40);
/// File name starts of everything the redistributable may write, for the backup.
const VC_FILE_STEMS: &[&str] = &["concrt140", "msvcp140", "vcruntime140", "vcomp140", "vcamp140", "vccorlib140", "mfc140", "mfcm140"];
const SYSTEM_DIRS: &[&str] = &["drive_c/windows/system32", "drive_c/windows/syswow64"];
const PROTONTRICKS_FLATPAK: &str = "com.github.Matoking.protontricks";

pub const FIX_VC: &str = "vc-runtime";
pub const FIX_LAUNCH: &str = "launch-options";
pub const FIX_OVERRIDES: &str = "dll-overrides";
pub const FIX_CASE: &str = "case-folders";

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Ok,
    Problem,
}

/// A button the UI shows, with the question it asks first. `blocked` says
/// why it can't run right now (Steam open, a tool missing).
#[derive(Debug, Clone, Serialize)]
pub struct Action {
    pub label: String,
    pub confirm: String,
    pub blocked: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub id: String,
    pub title: String,
    pub state: State,
    pub detail: String,
    pub fix: Option<Action>,
    /// Present once a fix has been applied and can be taken back.
    pub undo: Option<Action>,
}

/// A way to install Windows components into a prefix.
#[derive(Debug, Clone, PartialEq)]
pub enum Tricks {
    Protontricks(PathBuf),
    ProtontricksFlatpak,
    Winetricks(PathBuf),
}

/// What's running and installed on this machine. Tests build one by hand.
#[derive(Debug, Clone, Default)]
pub struct Probe {
    pub steam_running: bool,
    pub game_running: bool,
    pub protontricks: Option<Tricks>,
    pub winetricks: Option<PathBuf>,
    pub cabextract: bool,
}

impl Probe {
    pub fn system() -> Self {
        let procs = processes();
        let protontricks = which("protontricks").map(Tricks::Protontricks).or_else(|| {
            let ok = user_command("flatpak")
                .args(["info", PROTONTRICKS_FLATPAK])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            ok.then_some(Tricks::ProtontricksFlatpak)
        });
        Probe {
            steam_running: procs.iter().any(|(comm, _)| comm == "steam"),
            game_running: procs.iter().any(|(_, cmd)| cmd.to_ascii_lowercase().contains("cyberpunk2077.exe")),
            protontricks,
            winetricks: which("winetricks"),
            cabextract: which("cabextract").is_some(),
        }
    }
}

/// `(comm, cmdline)` of every process we can read.
fn processes() -> Vec<(String, String)> {
    let Ok(rd) = std::fs::read_dir("/proc") else { return vec![] };
    rd.flatten()
        .filter(|e| e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|e| {
            let comm = std::fs::read_to_string(e.path().join("comm")).ok()?;
            let cmd = std::fs::read(e.path().join("cmdline")).unwrap_or_default();
            Some((comm.trim().to_string(), String::from_utf8_lossy(&cmd).replace('\0', " ")))
        })
        .collect()
}

fn which(name: &str) -> Option<PathBuf> {
    let appdir = std::env::var("APPDIR").ok().filter(|a| !a.is_empty());
    std::env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .filter(|d| !d.is_empty() && appdir.as_deref().is_none_or(|a| !d.starts_with(a)))
        .map(|d| Path::new(d).join(name))
        .find(|p| is_executable(p))
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// Everything the checks and fixes work on.
pub struct Ctx<'a> {
    pub home: &'a Path,
    pub game: &'a GameInstall,
    /// Per-game folder for fix records, backups and logs.
    pub state_dir: PathBuf,
    pub probe: Probe,
}

// ---- Visual C++ runtime ------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Vc {
    Missing,
    /// Wine's own msvcp140: reports a high version number but isn't the real
    /// runtime, so CET and RED4ext can still fail on it.
    WineBuiltin,
    Native { version: (u16, u16), overridden: bool },
}

impl Vc {
    pub fn ok(&self) -> bool {
        matches!(self, Vc::Native { version, overridden: true } if *version >= MIN_VC_RUNTIME)
    }
}

/// Wine marks its own PE DLLs with this string right after the DOS header.
fn is_wine_builtin(dll: &Path) -> bool {
    use std::io::Read;
    let mut buf = [0u8; 0x50];
    std::fs::File::open(dll).and_then(|mut f| f.read_exact(&mut buf)).is_ok() && &buf[0x40..0x50] == b"Wine builtin DLL"
}

pub fn vc_state(prefix: &Path) -> Vc {
    let dll = prefix.join("drive_c/windows/system32/msvcp140.dll");
    if !dll.is_file() {
        return Vc::Missing;
    }
    if is_wine_builtin(&dll) {
        return Vc::WineBuiltin;
    }
    let version = game::exe_versions(&dll)
        .ok()
        .and_then(|(v, _)| v)
        .and_then(|v| {
            let mut p = v.split('.').map(|x| x.parse::<u16>().ok());
            Some((p.next()??, p.next()??))
        })
        .unwrap_or((0, 0));
    let reg = std::fs::read_to_string(prefix.join("user.reg")).unwrap_or_default();
    let overridden = read_overrides(&reg).get("msvcp140").is_some_and(|v| v.starts_with('n'));
    Vc::Native { version, overridden }
}

// ---- Wine registry DLL overrides ---------------------------------------------

const OVERRIDES_SECTION: &str = "[Software\\\\Wine\\\\DllOverrides]";

fn reg_entry(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix('"')?;
    let (name, value) = rest.split_once("\"=")?;
    let value = value.trim().trim_matches('"').to_string();
    Some((name.trim_start_matches('*').to_ascii_lowercase(), value))
}

/// `HKCU\Software\Wine\DllOverrides` from a prefix's `user.reg`, keys lowercased.
pub fn read_overrides(reg: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut inside = false;
    for line in reg.lines() {
        if line.starts_with('[') {
            inside = line.starts_with(OVERRIDES_SECTION);
            continue;
        }
        if inside && let Some((k, v)) = reg_entry(line) {
            out.insert(k, v);
        }
    }
    out
}

/// Set (`Some`) or remove (`None`) DLL overrides in `user.reg` text.
pub fn write_overrides(reg: &str, set: &[(String, Option<String>)]) -> String {
    let mut lines: Vec<String> = reg.lines().map(String::from).collect();
    let start = match lines.iter().position(|l| l.starts_with(OVERRIDES_SECTION)) {
        Some(i) => i,
        None => {
            if lines.last().is_some_and(|l| !l.is_empty()) {
                lines.push(String::new());
            }
            let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            lines.push(format!("{OVERRIDES_SECTION} {now}"));
            lines.len() - 1
        }
    };
    for (name, value) in set {
        let end = lines[start + 1..].iter().position(|l| l.starts_with('[')).map(|i| start + 1 + i).unwrap_or(lines.len());
        let existing = (start + 1..end).find(|&i| reg_entry(&lines[i]).is_some_and(|(k, _)| k.eq_ignore_ascii_case(name)));
        match (existing, value) {
            (Some(i), Some(v)) => lines[i] = format!("\"{name}\"=\"{v}\""),
            (Some(i), None) => {
                lines.remove(i);
            }
            (None, Some(v)) => {
                // After the section's last entry, before the blank separator.
                let mut at = end;
                while at > start + 1 && lines[at - 1].trim().is_empty() {
                    at -= 1;
                }
                lines.insert(at, format!("\"{name}\"=\"{v}\""));
            }
            (None, None) => {}
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

// ---- Steam launch options ----------------------------------------------------

pub fn has_override(opts: &str) -> bool {
    let lower = opts.to_ascii_lowercase();
    lower.contains("winedlloverrides") && lower.contains("winmm") && lower.contains("version")
}

pub fn has_modded(opts: &str) -> bool {
    opts.split_whitespace().any(|o| o.eq_ignore_ascii_case("-modded"))
}

/// The user's launch options with what mods need added, keeping everything
/// else (other variables, other DLL overrides, game arguments).
pub fn merge_launch_options(existing: &str, need_override: bool, need_modded: bool) -> String {
    let mut s = existing.trim().to_string();
    if need_override && !has_override(&s) {
        let key = "WINEDLLOVERRIDES=";
        if let Some(i) = s.to_ascii_uppercase().find(key) {
            let vstart = i + key.len();
            let (old, vend) = if s[vstart..].starts_with('"') {
                let close = s[vstart + 1..].find('"').map(|j| vstart + 1 + j).unwrap_or(s.len());
                (s[vstart + 1..close].to_string(), (close + 1).min(s.len()))
            } else {
                let end = s[vstart..].find(char::is_whitespace).map(|j| vstart + j).unwrap_or(s.len());
                (s[vstart..end].to_string(), end)
            };
            let mut parts = vec![OVERRIDE_VALUE.to_string()];
            for entry in old.split(';').filter(|e| !e.is_empty()) {
                let (dlls, mode) = entry.split_once('=').unwrap_or((entry, ""));
                let kept: Vec<&str> = dlls
                    .split(',')
                    .filter(|d| {
                        let d = d.trim().trim_end_matches(".dll").to_ascii_lowercase();
                        !d.is_empty() && d != "winmm" && d != "version"
                    })
                    .collect();
                if !kept.is_empty() {
                    parts.push(if mode.is_empty() { kept.join(",") } else { format!("{}={mode}", kept.join(",")) });
                }
            }
            s = format!("{}WINEDLLOVERRIDES=\"{}\"{}", &s[..i], parts.join(";"), &s[vend..]);
        } else if s.contains("%command%") {
            s = format!("WINEDLLOVERRIDES=\"{OVERRIDE_VALUE}\" {s}");
        } else {
            // Bare options are game arguments; they go after %command%.
            s = format!("WINEDLLOVERRIDES=\"{OVERRIDE_VALUE}\" %command% {s}");
        }
    }
    if need_modded && !has_modded(&s) {
        s = match s.find("%command%") {
            Some(i) => format!("{} -modded{}", &s[..i + "%command%".len()], &s[i + "%command%".len()..]),
            None => format!("{s} -modded"),
        };
    }
    s.trim().to_string()
}

const LAUNCH_PATH: &[&str] = &["UserLocalConfigStore", "Software", "Valve", "Steam", "apps", STEAM_APP_ID, "LaunchOptions"];

fn launch_in(src: &str) -> Option<String> {
    let mut v = &vdf::parse(src);
    for k in LAUNCH_PATH {
        v = v.get(k)?;
    }
    v.as_str().map(String::from)
}

/// Every Steam account's `localconfig.vdf` that has an apps list to edit.
fn localconfigs(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in game::steam_roots(home) {
        let Ok(users) = std::fs::read_dir(root.join("userdata")) else { continue };
        for u in users.flatten() {
            let p = u.path().join("config/localconfig.vdf");
            if std::fs::read_to_string(&p).is_ok_and(|s| vdf::set_value(&s, LAUNCH_PATH, "", 2).is_some()) {
                out.push(p);
            }
        }
    }
    out
}

/// Replace a file through a temp file next to it, keeping its permissions.
fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension("cpmx-tmp");
    std::fs::write(&tmp, content)?;
    if let Ok(m) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, m.permissions());
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ---- Folders that differ only in case ------------------------------------------

/// Folder names the game itself uses; when two spellings exist, these win.
const CANONICAL: &[&str] = &[
    "archive", "pc", "mod", "bin", "x64", "plugins", "cyber_engine_tweaks", "mods", "r6", "scripts", "tweaks", "cache", "red4ext",
    "engine", "tools", "config",
];

/// Game-relative folders that exist under several spellings. Wine sees only
/// one of them, so mods in the others don't load.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CaseDup {
    pub keep: String,
    pub others: Vec<String>,
}

fn count_files(dir: &Path) -> usize {
    walkdir::WalkDir::new(dir).into_iter().flatten().filter(|e| e.file_type().is_file()).count()
}

pub fn case_duplicates(game_dir: &Path) -> Vec<CaseDup> {
    let mut out = Vec::new();
    scan_dups(game_dir, game_dir, 0, &mut out);
    out
}

fn scan_dups(game_dir: &Path, dir: &Path, depth: usize, out: &mut Vec<CaseDup>) {
    if depth > 10 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut groups: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for e in rd.flatten() {
        if !e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        // At the top only the folders mods go into; the rest is the game's.
        if depth == 0 && !KNOWN_ROOTS.iter().any(|r| r.eq_ignore_ascii_case(&name)) {
            continue;
        }
        groups.entry(name.to_lowercase()).or_default().push(e.path());
    }
    let mut keys: Vec<_> = groups.keys().cloned().collect();
    keys.sort();
    for k in keys {
        let mut members = groups.remove(&k).unwrap_or_default();
        members.sort();
        let keep = if members.len() > 1 {
            let name = |p: &PathBuf| p.file_name().unwrap_or_default().to_string_lossy().to_string();
            let keep = members
                .iter()
                .position(|p| CANONICAL.contains(&name(p).as_str()))
                .unwrap_or_else(|| {
                    let counts: Vec<usize> = members.iter().map(|m| count_files(m)).collect();
                    let max = counts.iter().copied().max().unwrap_or(0);
                    counts.iter().position(|&c| c == max).unwrap_or(0)
                });
            let rel = |p: &PathBuf| p.strip_prefix(game_dir).unwrap_or(p).to_string_lossy().to_string();
            out.push(CaseDup {
                keep: rel(&members[keep]),
                others: members.iter().enumerate().filter(|(i, _)| *i != keep).map(|(_, p)| rel(p)).collect(),
            });
            members.swap_remove(keep)
        } else {
            members.swap_remove(0)
        };
        scan_dups(game_dir, &keep, depth + 1, out);
    }
}

/// Move everything from the other spellings into the kept folder. Files that
/// already exist there are left where they are and returned as conflicts.
type Moves = Vec<(String, String)>;

fn merge_dups(game_dir: &Path, dups: &[CaseDup]) -> Result<(Moves, Vec<String>)> {
    let mut moves = Vec::new();
    let mut conflicts = Vec::new();
    for d in dups {
        let keep = game_dir.join(&d.keep);
        for other in &d.others {
            let other_abs = game_dir.join(other);
            let files: Vec<PathBuf> = walkdir::WalkDir::new(&other_abs)
                .into_iter()
                .flatten()
                .filter(|e| !e.file_type().is_dir())
                .map(|e| e.into_path())
                .collect();
            for f in files {
                let rel = f.strip_prefix(&other_abs).unwrap_or(&f).to_string_lossy().to_string();
                let dst = resolve_ci(&keep, &rel);
                let from = f.strip_prefix(game_dir).unwrap_or(&f).to_string_lossy().to_string();
                if dst.symlink_metadata().is_ok() || !dst.starts_with(&keep) {
                    conflicts.push(from);
                    continue;
                }
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::rename(&f, &dst)?;
                moves.push((from, dst.strip_prefix(game_dir).unwrap_or(&dst).to_string_lossy().to_string()));
            }
            remove_empty_dirs(&other_abs);
        }
    }
    Ok((moves, conflicts))
}

fn remove_empty_dirs(dir: &Path) {
    let dirs: Vec<PathBuf> = walkdir::WalkDir::new(dir)
        .contents_first(true)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_dir())
        .map(|e| e.into_path())
        .collect();
    for d in dirs {
        let _ = std::fs::remove_dir(d); // fails, as it should, when not empty
    }
}

// ---- Records of applied fixes ------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
struct Record {
    applied_at: u64,
    /// localconfig.vdf files and their launch options before.
    #[serde(default)]
    launch: Vec<(PathBuf, Option<String>)>,
    /// DLL overrides before.
    #[serde(default)]
    overrides: Vec<(String, Option<String>)>,
    /// Game-relative (from, to) file moves.
    #[serde(default)]
    moves: Moves,
    /// Runtime DLLs that existed before, per system dir (copies in the backup).
    #[serde(default)]
    vc_files: Vec<(String, Vec<String>)>,
}

fn record_path(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.state_dir.join(format!("{id}.json"))
}

fn load_record(ctx: &Ctx, id: &str) -> Option<Record> {
    serde_json::from_str(&std::fs::read_to_string(record_path(ctx, id)).ok()?).ok()
}

fn save_record(ctx: &Ctx, id: &str, r: &Record) -> Result<()> {
    std::fs::create_dir_all(&ctx.state_dir)?;
    std::fs::write(record_path(ctx, id), serde_json::to_vec_pretty(r)?)?;
    Ok(())
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ---- Checks --------------------------------------------------------------------

const CLOSE_GAME: &str = "Cyberpunk 2077 is running. Quit the game first.";
const CLOSE_STEAM: &str =
    "Steam is running. Quit Steam completely (Steam › Exit), then try again: Steam rewrites this setting when it quits.";

fn action(label: &str, confirm: String, blocked: Option<&str>) -> Option<Action> {
    Some(Action { label: label.into(), confirm, blocked: blocked.map(String::from) })
}

fn undo_action(ctx: &Ctx, id: &str, what: &str, blocked: Option<&str>) -> Option<Action> {
    load_record(ctx, id).map(|_| Action {
        label: "Undo".into(),
        confirm: format!("Undo the earlier fix and put back {what} as it was before?"),
        blocked: blocked.map(String::from),
    })
}

/// How the runtime would be installed for this game, or why it can't be.
fn vc_installer(ctx: &Ctx) -> std::result::Result<(Tricks, Option<PathBuf>), String> {
    let p = &ctx.probe;
    let winetricks_with = |wine: Option<PathBuf>| -> std::result::Result<(Tricks, Option<PathBuf>), String> {
        match (&p.winetricks, wine) {
            (Some(w), Some(wine)) if p.cabextract => Ok((Tricks::Winetricks(w.clone()), Some(wine))),
            (Some(_), Some(_)) => Err("winetricks needs cabextract; install your distribution's cabextract package.".into()),
            _ => Err(String::new()),
        }
    };
    match ctx.game.store {
        Store::Steam => {
            if let Some(t) = &p.protontricks {
                return Ok((t.clone(), None));
            }
            let wine = ctx.game.proton_prefix.as_deref().and_then(steam_proton_wine);
            winetricks_with(wine).map_err(|e| {
                if e.is_empty() {
                    "Install protontricks first (Flathub: flatpak install flathub com.github.Matoking.protontricks, \
                     or your distribution's protontricks package), then click again."
                        .into()
                } else {
                    e
                }
            })
        }
        _ => {
            let wine = game::heroic_game(ctx.home, &ctx.game.path).and_then(|h| h.wine);
            winetricks_with(wine).map_err(|e| {
                if e.is_empty() {
                    "Install winetricks and cabextract from your distribution, then click again. \
                     The Wine build is read from Heroic's settings for this game."
                        .into()
                } else {
                    e
                }
            })
        }
    }
}

/// The Proton build that last ran this prefix (`config_info` names its fonts dir).
fn steam_proton_wine(prefix: &Path) -> Option<PathBuf> {
    let info = std::fs::read_to_string(prefix.parent()?.join("config_info")).ok()?;
    let fonts = info.lines().nth(1)?;
    let dir = ["/files/", "/dist/"].iter().find_map(|m| fonts.find(m).map(|i| &fonts[..i]))?;
    game::proton_wine(Path::new(dir))
}

/// On Windows the game runs natively: there's no prefix, no overrides and no
/// case-sensitive filesystem, so there is nothing to check.
pub fn checks(ctx: &Ctx) -> Vec<Check> {
    if cfg!(windows) {
        return Vec::new();
    }
    let g = ctx.game;
    let needs = game::needs_overrides(g);
    let redmods = game::has_redmods(&g.path);
    let running = ctx.probe.game_running.then_some(CLOSE_GAME);
    let mut out = Vec::new();

    // Visual C++ runtime.
    let vc_record = load_record(ctx, FIX_VC).is_some();
    if needs || vc_record {
        match &g.proton_prefix {
            None if g.store == Store::Steam => out.push(Check {
                id: FIX_VC.into(),
                title: "Visual C++ runtime".into(),
                state: State::Problem,
                detail: "Proton hasn't created this game's prefix yet. Start the game once from Steam, quit, then rescan.".into(),
                fix: None,
                undo: None,
            }),
            None => {}
            Some(prefix) => {
                let vc = vc_state(prefix);
                let detail = match &vc {
                    _ if vc.ok() => "Visual C++ 2015-2022 runtime is installed in the prefix.".to_string(),
                    Vc::Missing => "The prefix has no Visual C++ runtime. CET and RED4ext fail to start without it (error 998 or a missing DLL).".into(),
                    Vc::WineBuiltin => "The prefix only has Wine's stand-in for the Visual C++ runtime. CET and RED4ext need Microsoft's (error 998 at startup).".into(),
                    Vc::Native { version, overridden: false } if *version >= MIN_VC_RUNTIME => {
                        "Microsoft's Visual C++ runtime is in the prefix, but Wine is set to use its own copy instead, so CET and RED4ext can fail with error 998.".into()
                    }
                    Vc::Native { version, .. } => format!(
                        "The Visual C++ runtime in the prefix is {}.{}, older than CET and RED4ext need ({}.{} or newer); they fail at startup with error 998.",
                        version.0, version.1, MIN_VC_RUNTIME.0, MIN_VC_RUNTIME.1
                    ),
                };
                let fix = (!vc.ok()).then(|| {
                    let (blocked, how) = match vc_installer(ctx) {
                        Ok((t, _)) => (running.map(String::from), match t {
                            Tricks::Protontricks(_) | Tricks::ProtontricksFlatpak => "protontricks",
                            Tricks::Winetricks(_) => "winetricks",
                        }),
                        Err(e) => (Some(running.map(String::from).unwrap_or(e)), "protontricks"),
                    };
                    Action {
                        label: "Install vcrun2022".into(),
                        confirm: format!(
                            "Install Microsoft's Visual C++ 2015-2022 runtime (vcrun2022) into the game's prefix with {how}?\n\n\
                             It downloads the installer from Microsoft and can take a few minutes. The runtime files and prefix \
                             registry are backed up first, and Undo puts them back."
                        ),
                        blocked,
                    }
                });
                out.push(Check {
                    id: FIX_VC.into(),
                    title: "Visual C++ runtime".into(),
                    state: if vc.ok() { State::Ok } else { State::Problem },
                    detail,
                    fix,
                    undo: undo_action(ctx, FIX_VC, "the prefix's runtime files and registry", running),
                });
            }
        }
    }

    // DLL overrides and -modded.
    if g.store == Store::Steam {
        let rec = load_record(ctx, FIX_LAUNCH).is_some();
        if needs || redmods || rec {
            let cur = g.launch_options.clone().unwrap_or_default();
            let want = merge_launch_options(&cur, needs, redmods);
            let ok = want == cur.trim();
            let mut missing = Vec::new();
            if needs && !has_override(&cur) {
                missing.push(format!("WINEDLLOVERRIDES=\"{OVERRIDE_VALUE}\" so CET and RED4ext load"));
            }
            if redmods && !has_modded(&cur) {
                missing.push("-modded so REDmod mods load".into());
            }
            let steam_block = if ctx.probe.steam_running { Some(CLOSE_STEAM) } else { None };
            let no_config = localconfigs(ctx.home).is_empty().then_some("Steam has no settings for this game yet. Start it once from Steam, then try again.");
            out.push(Check {
                id: FIX_LAUNCH.into(),
                title: "Steam launch options".into(),
                state: if ok { State::Ok } else { State::Problem },
                detail: if ok {
                    format!("Launch options are set: {cur}")
                } else {
                    format!("Steam's launch options need {}.", missing.join(" and "))
                },
                fix: if ok {
                    None
                } else {
                    action(
                        "Set launch options",
                        format!(
                            "Set Cyberpunk 2077's Steam launch options to:\n\n{want}\n\nNow: {}\n\nUndo puts the old ones back.",
                            if cur.trim().is_empty() { "(empty)" } else { cur.trim() }
                        ),
                        steam_block.or(no_config),
                    )
                },
                undo: undo_action(ctx, FIX_LAUNCH, "the previous launch options", steam_block),
            });
        }
    } else if let Some(prefix) = &g.proton_prefix {
        let rec = load_record(ctx, FIX_OVERRIDES).is_some();
        if needs || rec {
            let reg = std::fs::read_to_string(prefix.join("user.reg")).unwrap_or_default();
            let o = read_overrides(&reg);
            let ok = ["winmm", "version"].iter().all(|d| o.get(*d).is_some_and(|v| v.starts_with('n')));
            out.push(Check {
                id: FIX_OVERRIDES.into(),
                title: "DLL overrides".into(),
                state: if ok { State::Ok } else { State::Problem },
                detail: if ok {
                    "winmm and version load the mod frameworks' DLLs first.".into()
                } else {
                    "CET and RED4ext need Wine to load their winmm.dll and version.dll before its own.".into()
                },
                fix: if ok {
                    None
                } else {
                    action(
                        "Set overrides",
                        "Set winmm and version to “native, then builtin” in the game's Wine prefix? This works for any launcher, \
                         so no launch options are needed. Undo puts the old settings back."
                            .into(),
                        running,
                    )
                },
                undo: undo_action(ctx, FIX_OVERRIDES, "the previous DLL overrides", running),
            });
        }
    }

    // Folders that exist under two spellings.
    let dups = case_duplicates(&g.path);
    let rec = load_record(ctx, FIX_CASE).is_some();
    if !dups.is_empty() || rec {
        let list: Vec<String> = dups.iter().map(|d| format!("{} ({})", d.keep, d.others.join(", "))).collect();
        out.push(Check {
            id: FIX_CASE.into(),
            title: "Folder names".into(),
            state: if dups.is_empty() { State::Ok } else { State::Problem },
            detail: if dups.is_empty() {
                "Every mod folder has a single spelling.".into()
            } else {
                format!(
                    "These folders exist more than once with different capitalisation, and the game only reads one of them: {}",
                    list.join("; ")
                )
            },
            fix: (!dups.is_empty()).then(|| Action {
                label: "Merge folders".into(),
                confirm: format!(
                    "Move the files from the other spellings into the folder the game reads?\n\n{}\n\nFiles that exist in both are left in place. Undo moves everything back.",
                    list.join("\n")
                ),
                blocked: running.map(String::from),
            }),
            undo: undo_action(ctx, FIX_CASE, "the moved files", running),
        });
    }
    out
}

// ---- Applying and undoing ------------------------------------------------------

fn find<'a>(checks: &'a [Check], id: &str) -> Result<&'a Check> {
    checks.iter().find(|c| c.id == id).ok_or_else(|| Error::Other(format!("nothing to do for {id}")))
}

/// Apply one fix after the user confirmed it. Returns a line for the user.
pub fn apply(ctx: &Ctx, id: &str) -> Result<String> {
    let all = checks(ctx);
    let check = find(&all, id)?;
    let fix = check.fix.as_ref().ok_or_else(|| Error::Other(format!("{} is already fine", check.title)))?;
    if let Some(b) = &fix.blocked {
        return Err(Error::Other(b.clone()));
    }
    let g = ctx.game;
    match id {
        FIX_VC => apply_vc(ctx),
        FIX_LAUNCH => {
            let needs = game::needs_overrides(g);
            let redmods = game::has_redmods(&g.path);
            let mut rec = load_record(ctx, FIX_LAUNCH).unwrap_or_default();
            let first = rec.launch.is_empty();
            let mut set = String::new();
            for cfg in localconfigs(ctx.home) {
                let src = std::fs::read_to_string(&cfg)?;
                let before = launch_in(&src);
                let want = merge_launch_options(before.as_deref().unwrap_or(""), needs, redmods);
                if before.as_deref() == Some(want.as_str()) {
                    continue;
                }
                let new = vdf::set_value(&src, LAUNCH_PATH, &want, 2)
                    .ok_or_else(|| Error::Other(format!("could not edit {}", cfg.display())))?;
                if first || !rec.launch.iter().any(|(p, _)| p == &cfg) {
                    rec.launch.push((cfg.clone(), before));
                }
                write_atomic(&cfg, &new)?;
                set = want;
            }
            rec.applied_at = now();
            save_record(ctx, FIX_LAUNCH, &rec)?;
            Ok(format!("Launch options set: {set}"))
        }
        FIX_OVERRIDES => {
            let prefix = g.proton_prefix.as_ref().ok_or_else(|| Error::Other("no prefix".into()))?;
            let reg_path = prefix.join("user.reg");
            let reg = std::fs::read_to_string(&reg_path).unwrap_or_default();
            let before = read_overrides(&reg);
            let mut rec = load_record(ctx, FIX_OVERRIDES).unwrap_or_default();
            if rec.overrides.is_empty() {
                rec.overrides = ["winmm", "version"].iter().map(|d| (d.to_string(), before.get(*d).cloned())).collect();
            }
            let set: Vec<(String, Option<String>)> =
                ["winmm", "version"].iter().map(|d| (d.to_string(), Some("native,builtin".to_string()))).collect();
            write_atomic(&reg_path, &write_overrides(&reg, &set))?;
            rec.applied_at = now();
            save_record(ctx, FIX_OVERRIDES, &rec)?;
            Ok("winmm and version now load native first.".into())
        }
        FIX_CASE => {
            let dups = case_duplicates(&g.path);
            let (moves, conflicts) = merge_dups(&g.path, &dups)?;
            let mut rec = load_record(ctx, FIX_CASE).unwrap_or_default();
            rec.moves.extend(moves.iter().cloned());
            rec.applied_at = now();
            save_record(ctx, FIX_CASE, &rec)?;
            let mut msg = format!("Moved {} file(s) into the folders the game reads.", moves.len());
            if !conflicts.is_empty() {
                msg.push_str(&format!(" Left {} that already exist there: {}", conflicts.len(), conflicts.join(", ")));
            }
            Ok(msg)
        }
        _ => Err(Error::Other(format!("unknown fix {id}"))),
    }
}

fn vc_backup_dir(ctx: &Ctx) -> PathBuf {
    ctx.state_dir.join(FIX_VC)
}

fn is_vc_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".dll") && VC_FILE_STEMS.iter().any(|s| lower.starts_with(s))
}

fn apply_vc(ctx: &Ctx) -> Result<String> {
    let prefix = ctx.game.proton_prefix.clone().ok_or_else(|| Error::Other("no prefix".into()))?;
    let (tricks, wine) = vc_installer(ctx).map_err(Error::Other)?;
    // Back up once: a second run keeps the copy from before the first.
    if load_record(ctx, FIX_VC).is_none() {
        let backup = vc_backup_dir(ctx);
        let _ = std::fs::remove_dir_all(&backup);
        let mut rec = Record { applied_at: now(), ..Default::default() };
        for dir in SYSTEM_DIRS {
            let mut names = Vec::new();
            if let Ok(rd) = std::fs::read_dir(prefix.join(dir)) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    if is_vc_file(&name) && e.file_type().is_ok_and(|t| t.is_file()) {
                        std::fs::create_dir_all(backup.join(dir))?;
                        std::fs::copy(e.path(), backup.join(dir).join(&name))?;
                        names.push(name);
                    }
                }
            }
            rec.vc_files.push((dir.to_string(), names));
        }
        for reg in ["user.reg", "system.reg"] {
            if prefix.join(reg).is_file() {
                std::fs::copy(prefix.join(reg), backup.join(reg))?;
            }
        }
        save_record(ctx, FIX_VC, &rec)?;
    }

    let mut cmd = match &tricks {
        Tricks::Protontricks(p) => {
            let mut c = user_command(&p.to_string_lossy());
            c.args([STEAM_APP_ID, "-q", "--force", "vcrun2022"]);
            c
        }
        Tricks::ProtontricksFlatpak => {
            let mut c = user_command("flatpak");
            c.args(["run", PROTONTRICKS_FLATPAK, STEAM_APP_ID, "-q", "--force", "vcrun2022"]);
            c
        }
        Tricks::Winetricks(w) => {
            let mut c = user_command(&w.to_string_lossy());
            c.env("WINEPREFIX", &prefix).args(["-q", "--force", "vcrun2022"]);
            if let Some(wine) = &wine {
                c.env("WINE", wine);
                if let Some(server) = wine.parent().map(|d| d.join("wineserver")).filter(|p| p.is_file()) {
                    c.env("WINESERVER", server);
                }
            }
            c
        }
    };
    let log_path = ctx.state_dir.join("vc-runtime.log");
    let log = std::fs::File::create(&log_path)?;
    let status = cmd.stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log).status()?;
    let vc = vc_state(&prefix);
    if vc.ok() {
        let v = match vc {
            Vc::Native { version, .. } => format!(" {}.{}", version.0, version.1),
            _ => String::new(),
        };
        return Ok(format!("Visual C++ runtime{v} installed. Start the game to check CET and RED4ext."));
    }
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let tail: Vec<&str> = log.lines().rev().take(8).collect();
    Err(Error::Other(format!(
        "vcrun2022 didn't install ({}). Undo restores the prefix. Last output:\n{}\nFull log: {}",
        if status.success() { "the runtime is still not in place" } else { "the installer failed" },
        tail.into_iter().rev().collect::<Vec<_>>().join("\n"),
        log_path.display()
    )))
}

/// Put back what a fix changed.
pub fn undo(ctx: &Ctx, id: &str) -> Result<String> {
    let all = checks(ctx);
    let check = find(&all, id)?;
    let undo = check.undo.as_ref().ok_or_else(|| Error::Other("nothing to undo".into()))?;
    if let Some(b) = &undo.blocked {
        return Err(Error::Other(b.clone()));
    }
    let rec = load_record(ctx, id).ok_or_else(|| Error::Other("nothing to undo".into()))?;
    let g = ctx.game;
    let msg = match id {
        FIX_VC => {
            let prefix = g.proton_prefix.clone().ok_or_else(|| Error::Other("no prefix".into()))?;
            let backup = vc_backup_dir(ctx);
            for (dir, names) in &rec.vc_files {
                if let Ok(rd) = std::fs::read_dir(prefix.join(dir)) {
                    for e in rd.flatten() {
                        let name = e.file_name().to_string_lossy().to_string();
                        if is_vc_file(&name) && !names.contains(&name) {
                            std::fs::remove_file(e.path())?;
                        }
                    }
                }
                for name in names {
                    std::fs::copy(backup.join(dir).join(name), prefix.join(dir).join(name))?;
                }
            }
            for reg in ["user.reg", "system.reg"] {
                if backup.join(reg).is_file() {
                    std::fs::copy(backup.join(reg), prefix.join(reg))?;
                }
            }
            let _ = std::fs::remove_dir_all(&backup);
            "The prefix's Visual C++ runtime and registry are back to how they were.".to_string()
        }
        FIX_LAUNCH => {
            for (cfg, before) in &rec.launch {
                let Ok(src) = std::fs::read_to_string(cfg) else { continue };
                if let Some(new) = vdf::set_value(&src, LAUNCH_PATH, before.as_deref().unwrap_or(""), 2) {
                    write_atomic(cfg, &new)?;
                }
            }
            "The previous launch options are back.".into()
        }
        FIX_OVERRIDES => {
            let prefix = g.proton_prefix.as_ref().ok_or_else(|| Error::Other("no prefix".into()))?;
            let reg_path = prefix.join("user.reg");
            let reg = std::fs::read_to_string(&reg_path).unwrap_or_default();
            write_atomic(&reg_path, &write_overrides(&reg, &rec.overrides))?;
            "The previous DLL overrides are back.".into()
        }
        FIX_CASE => {
            let mut skipped = 0;
            for (from, to) in rec.moves.iter().rev() {
                let (src, dst) = (g.path.join(to), g.path.join(from));
                if !src.is_file() || dst.symlink_metadata().is_ok() {
                    skipped += 1;
                    continue;
                }
                if let Some(p) = dst.parent() {
                    std::fs::create_dir_all(p)?;
                }
                std::fs::rename(&src, &dst)?;
            }
            if skipped > 0 {
                format!("Files moved back; {skipped} had changed since and were left where they are.")
            } else {
                "Files moved back to their old folders.".into()
            }
        }
        _ => return Err(Error::Other(format!("unknown fix {id}"))),
    };
    std::fs::remove_file(record_path(ctx, id))?;
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::{Framework, GAME_EXE};

    fn touch(p: &Path, content: &[u8]) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn install(dir: &Path, store: Store, prefix: Option<PathBuf>, launch: Option<&str>) -> GameInstall {
        touch(&dir.join(GAME_EXE), b"x");
        GameInstall {
            path: dir.to_path_buf(),
            store,
            proton_prefix: prefix,
            build_id: None,
            exe_file_version: None,
            exe_product_version: None,
            frameworks: vec![Framework { id: "cet".into(), name: "CET".into(), installed: true }],
            launch_options: launch.map(String::from),
            warnings: vec![],
        }
    }

    #[test]
    fn merges_launch_options() {
        let o = OVERRIDE_VALUE;
        assert_eq!(merge_launch_options("", true, false), format!("WINEDLLOVERRIDES=\"{o}\" %command%"));
        assert_eq!(merge_launch_options("", true, true), format!("WINEDLLOVERRIDES=\"{o}\" %command% -modded"));
        assert_eq!(merge_launch_options("-skipStartScreen", true, false), format!("WINEDLLOVERRIDES=\"{o}\" %command% -skipStartScreen"));
        assert_eq!(
            merge_launch_options("PROTON_LOG=1 %command% --launcher-skip", true, true),
            format!("WINEDLLOVERRIDES=\"{o}\" PROTON_LOG=1 %command% -modded --launcher-skip")
        );
        // An existing override list keeps its other DLLs.
        assert_eq!(
            merge_launch_options("WINEDLLOVERRIDES=\"dxgi=n;winmm=b\" %command%", true, false),
            format!("WINEDLLOVERRIDES=\"{o};dxgi=n\" %command%")
        );
        assert_eq!(merge_launch_options("WINEDLLOVERRIDES=d3d11=n %command%", true, false), format!("WINEDLLOVERRIDES=\"{o};d3d11=n\" %command%"));
        let done = format!("WINEDLLOVERRIDES=\"{o}\" %command% -modded");
        assert_eq!(merge_launch_options(&done, true, true), done, "already right: unchanged");
    }

    #[test]
    fn edits_wine_dll_overrides() {
        let reg = "WINE REGISTRY Version 2\n\n[Software\\\\Wine\\\\DllOverrides] 1700000000\n#time=1da\n\"*d3d11\"=\"native\"\n\"winmm\"=\"builtin\"\n\n[Software\\\\Wine\\\\Fonts] 1\n\"x\"=\"y\"\n";
        let o = read_overrides(reg);
        assert_eq!(o.get("d3d11").map(String::as_str), Some("native"));
        let set = vec![("winmm".to_string(), Some("native,builtin".to_string())), ("version".to_string(), Some("native,builtin".to_string()))];
        let out = write_overrides(reg, &set);
        let o2 = read_overrides(&out);
        assert_eq!(o2.get("winmm").map(String::as_str), Some("native,builtin"));
        assert_eq!(o2.get("version").map(String::as_str), Some("native,builtin"));
        assert!(out.contains("[Software\\\\Wine\\\\Fonts] 1\n\"x\"=\"y\""), "{out}");
        assert!(!read_overrides(&out).contains_key("x"));
        // Undo: back to what was there, and the key that wasn't is removed.
        let back = write_overrides(&out, &[("winmm".into(), Some("builtin".into())), ("version".into(), None)]);
        assert_eq!(read_overrides(&back), o);
        // A prefix without the section gets one.
        let fresh = write_overrides("WINE REGISTRY Version 2\n", &set);
        assert_eq!(read_overrides(&fresh).len(), 2);
    }

    #[test]
    fn detects_wine_builtin_runtime() {
        let pfx = tempfile::tempdir().unwrap();
        assert_eq!(vc_state(pfx.path()), Vc::Missing);
        let mut stub = vec![0u8; 0x80];
        stub[..2].copy_from_slice(b"MZ");
        stub[0x40..0x50].copy_from_slice(b"Wine builtin DLL");
        touch(&pfx.path().join("drive_c/windows/system32/msvcp140.dll"), &stub);
        assert_eq!(vc_state(pfx.path()), Vc::WineBuiltin);
        assert!(!vc_state(pfx.path()).ok());
    }

    #[test]
    fn steam_launch_options_fix_and_undo() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join(".local/share/Steam");
        std::fs::create_dir_all(root.join("steamapps")).unwrap();
        let cfg = root.join("userdata/123/config/localconfig.vdf");
        let before = "\"UserLocalConfigStore\"\n{\n\t\"Software\"\n\t{\n\t\t\"Valve\"\n\t\t{\n\t\t\t\"Steam\"\n\t\t\t{\n\t\t\t\t\"apps\"\n\t\t\t\t{\n\t\t\t\t\t\"1091500\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"LaunchOptions\"\t\t\"-skipStartScreen\"\n\t\t\t\t\t}\n\t\t\t\t}\n\t\t\t}\n\t\t}\n\t}\n}\n";
        touch(&cfg, before.as_bytes());
        let gdir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let g = install(gdir.path(), Store::Steam, None, Some("-skipStartScreen"));
        let mut ctx = Ctx { home: home.path(), game: &g, state_dir: state.path().to_path_buf(), probe: Probe { steam_running: true, ..Default::default() } };

        let c = checks(&ctx).into_iter().find(|c| c.id == FIX_LAUNCH).unwrap();
        assert_eq!(c.state, State::Problem);
        assert!(c.fix.as_ref().unwrap().blocked.as_deref().unwrap().contains("Quit Steam"));
        assert!(apply(&ctx, FIX_LAUNCH).is_err(), "never while Steam runs");
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), before);

        ctx.probe.steam_running = false;
        apply(&ctx, FIX_LAUNCH).unwrap();
        let now = launch_in(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(now, format!("WINEDLLOVERRIDES=\"{OVERRIDE_VALUE}\" %command% -skipStartScreen"));

        let g2 = GameInstall { launch_options: Some(now), ..g.clone() };
        let ctx2 = Ctx { game: &g2, ..ctx };
        let c = checks(&ctx2).into_iter().find(|c| c.id == FIX_LAUNCH).unwrap();
        assert_eq!(c.state, State::Ok);
        assert!(c.undo.is_some());
        undo(&ctx2, FIX_LAUNCH).unwrap();
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), before, "undo restores the file exactly");
    }

    #[test]
    fn heroic_prefix_gets_registry_overrides() {
        let home = tempfile::tempdir().unwrap();
        let gdir = tempfile::tempdir().unwrap();
        let pfx = tempfile::tempdir().unwrap();
        touch(&pfx.path().join("user.reg"), b"WINE REGISTRY Version 2\n");
        let state = tempfile::tempdir().unwrap();
        let g = install(gdir.path(), Store::Gog, Some(pfx.path().to_path_buf()), None);
        let ctx = Ctx { home: home.path(), game: &g, state_dir: state.path().to_path_buf(), probe: Probe::default() };
        let all = checks(&ctx);
        assert!(all.iter().all(|c| c.id != FIX_LAUNCH), "no Steam options for GOG");
        let vc = all.iter().find(|c| c.id == FIX_VC).unwrap();
        assert!(vc.fix.as_ref().unwrap().blocked.as_deref().unwrap().contains("winetricks"));
        apply(&ctx, FIX_OVERRIDES).unwrap();
        assert_eq!(checks(&ctx).into_iter().find(|c| c.id == FIX_OVERRIDES).unwrap().state, State::Ok);
        undo(&ctx, FIX_OVERRIDES).unwrap();
        assert!(read_overrides(&std::fs::read_to_string(pfx.path().join("user.reg")).unwrap()).is_empty());
    }

    #[test]
    fn merges_case_duplicate_folders_and_moves_back() {
        let home = tempfile::tempdir().unwrap();
        let gdir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let game = gdir.path();
        touch(&game.join("archive/pc/mod/a.archive"), b"a");
        touch(&game.join("Archive/PC/Mod/b.archive"), b"b");
        touch(&game.join("Archive/PC/Mod/a.archive"), b"other a");
        touch(&game.join("r6/scripts/x.reds"), b"x");
        let g = install(game, Store::Manual, None, None);
        let dups = case_duplicates(game);
        assert_eq!(dups, vec![CaseDup { keep: "archive".into(), others: vec!["Archive".into()] }]);

        let ctx = Ctx { home: home.path(), game: &g, state_dir: state.path().to_path_buf(), probe: Probe::default() };
        let msg = apply(&ctx, FIX_CASE).unwrap();
        assert!(msg.contains("Moved 1") && msg.contains("Archive/PC/Mod/a.archive"), "{msg}");
        assert_eq!(std::fs::read(game.join("archive/pc/mod/b.archive")).unwrap(), b"b");
        assert_eq!(std::fs::read(game.join("archive/pc/mod/a.archive")).unwrap(), b"a", "conflict left alone");

        undo(&ctx, FIX_CASE).unwrap();
        assert_eq!(std::fs::read(game.join("Archive/PC/Mod/b.archive")).unwrap(), b"b");
        assert!(!game.join("archive/pc/mod/b.archive").exists());
    }

    #[test]
    #[cfg(unix)]
    fn vc_fix_backs_up_runs_winetricks_and_undo_restores() {
        let home = tempfile::tempdir().unwrap();
        let gdir = tempfile::tempdir().unwrap();
        let pfx = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let sys32 = pfx.path().join("drive_c/windows/system32");
        touch(&sys32.join("msvcp140.dll"), b"old");
        touch(&pfx.path().join("user.reg"), b"WINE REGISTRY Version 2\n");
        // A stand-in winetricks that writes a runtime and touches the registry.
        let bin = tempfile::tempdir().unwrap();
        let wt = bin.path().join("winetricks");
        touch(&wt, b"#!/bin/sh\n[ -n \"$WINE\" ] || exit 3\necho new > \"$WINEPREFIX/drive_c/windows/system32/msvcp140.dll\"\necho new > \"$WINEPREFIX/drive_c/windows/system32/vcruntime140_1.dll\"\necho changed >> \"$WINEPREFIX/user.reg\"\n");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wt, std::fs::Permissions::from_mode(0o755)).unwrap();
        let wine = bin.path().join("wine");
        touch(&wine, b"");
        let g = install(gdir.path(), Store::Gog, Some(pfx.path().to_path_buf()), None);
        let mut ctx = Ctx {
            home: home.path(),
            game: &g,
            state_dir: state.path().to_path_buf(),
            probe: Probe { winetricks: Some(wt), cabextract: true, ..Default::default() },
        };
        // Without Heroic's settings there's no Wine build to use.
        let c = checks(&ctx).into_iter().find(|c| c.id == FIX_VC).unwrap();
        assert!(c.fix.unwrap().blocked.is_some());

        let cfg = home.path().join(".config/heroic");
        touch(
            &cfg.join("gog_store/installed.json"),
            serde_json::json!({"installed": [{"appName": "1423049311", "install_path": gdir.path()}]}).to_string().as_bytes(),
        );
        touch(
            &cfg.join("GamesConfig/1423049311.json"),
            serde_json::json!({"1423049311": {"winePrefix": pfx.path(), "wineVersion": {"bin": wine, "type": "wine"}}}).to_string().as_bytes(),
        );
        ctx.probe.game_running = true;
        assert!(apply(&ctx, FIX_VC).unwrap_err().to_string().contains("Quit the game"));
        ctx.probe.game_running = false;

        // The stand-in writes no real DLL, so the check after still fails,
        // and says Undo is there.
        let err = apply(&ctx, FIX_VC).unwrap_err().to_string();
        assert!(err.contains("Undo"), "{err}");
        assert_eq!(std::fs::read(sys32.join("msvcp140.dll")).unwrap(), b"new\n");

        undo(&ctx, FIX_VC).unwrap();
        assert_eq!(std::fs::read(sys32.join("msvcp140.dll")).unwrap(), b"old");
        assert!(!sys32.join("vcruntime140_1.dll").exists(), "files the installer added are removed");
        assert_eq!(std::fs::read_to_string(pfx.path().join("user.reg")).unwrap(), "WINE REGISTRY Version 2\n");
        assert!(checks(&ctx).into_iter().find(|c| c.id == FIX_VC).unwrap().undo.is_none());
    }
}
