//! The startup timeline: what happened, step by step, the last time the
//! game started. A modded start runs in a fixed order (RED4ext, its plugins,
//! the script compiler, CET, the RED4ext-based frameworks), and each step
//! writes a line when it gets through. The first step that failed is the
//! one to fix; errors after it usually follow from it.

use std::collections::BTreeSet;
use std::path::Path;

use serde::Serialize;

use crate::crash::{CET_LOG, Issue, Level, REDSCRIPT_LOG, Scanned, Session, main_red4ext_log, mods_in};
use crate::game::Framework;
use crate::install::resolve_ci;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Warning,
    Failed,
    /// Didn't run, usually because an earlier step failed.
    NotRun,
    NotInstalled,
    /// Not enough information (no logs yet).
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct Step {
    pub id: &'static str,
    /// Plain name, e.g. "Script mods (redscript)".
    pub title: &'static str,
    pub status: Status,
    pub summary: String,
    pub fix: Option<String>,
    /// Log lines behind the result, at most a few.
    pub details: Vec<String>,
    pub version: Option<String>,
    pub mod_ids: Vec<i64>,
    pub mod_names: Vec<String>,
    /// The first failed step: start here.
    pub first_problem: bool,
}

pub(crate) struct Input<'a> {
    pub game_dir: &'a Path,
    pub frameworks: &'a [Framework],
    pub scanned: &'a [Scanned<'a>],
    pub issues: &'a [Issue],
    pub needles: &'a [(i64, String, BTreeSet<String>)],
    pub session: Option<&'a Session>,
}

impl Input<'_> {
    fn installed(&self, id: &str) -> bool {
        self.frameworks.iter().any(|f| f.id == id && f.installed)
    }

    fn log(&self, name: &str) -> Option<&Scanned<'_>> {
        self.scanned.iter().find(|s| s.log.name.eq_ignore_ascii_case(name))
    }

    /// Last-session issues from logs whose name starts with `prefix`.
    fn issues_in(&self, prefix: &str, level: Level) -> Vec<&Issue> {
        let p = prefix.to_ascii_lowercase();
        self.issues.iter().filter(|i| i.last_session && i.level == level && i.log.to_ascii_lowercase().starts_with(&p)).collect()
    }

    fn version(&self, rel: &str) -> Option<String> {
        let path = resolve_ci(self.game_dir, rel);
        let (file, product) = crate::game::exe_versions(&path).ok()?;
        product.filter(|p| !p.is_empty()).or(file)
    }
}

fn step(id: &'static str, title: &'static str, status: Status, summary: impl Into<String>) -> Step {
    Step { id, title, status, summary: summary.into(), fix: None, details: vec![], version: None, mod_ids: vec![], mod_names: vec![], first_problem: false }
}

fn add_mods(s: &mut Step, ids: &[i64], names: &[String]) {
    for (id, name) in ids.iter().zip(names) {
        if !s.mod_ids.contains(id) {
            s.mod_ids.push(*id);
            s.mod_names.push(name.clone());
        }
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The text after `marker` (case-insensitive) up to the next space or `)`.
fn token_after(text: &str, marker: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let at = lower.find(&marker.to_ascii_lowercase())? + marker.len();
    let t: String = text[at..].trim_start().chars().take_while(|c| !c.is_whitespace() && *c != ')' && *c != ',').collect();
    let t = t.trim_start_matches('v').to_string();
    t.chars().next().is_some_and(|c| c.is_ascii_digit()).then_some(t)
}

/// The plugin path in `Loading plugin from 'C:\...\X.dll'...`.
fn quoted(line: &str) -> Option<&str> {
    let a = line.find('\'')? + 1;
    let b = line[a..].find('\'')? + a;
    Some(&line[a..b])
}

const LINUX_RED4EXT: &str = " On Linux and Steam Deck, check that the launch options include the winmm DLL override (see Game setup).";
const LINUX_CET: &str = " On Linux and Steam Deck, also check the version DLL override and the Visual C++ runtime under Game setup.";

fn linux(hint: &str) -> &str {
    if cfg!(target_os = "linux") { hint } else { "" }
}

pub(crate) fn timeline(cx: &Input) -> Vec<Step> {
    let known = cx.session.is_some();
    let mut out = Vec::new();

    // 1. RED4ext and 2. its plugins.
    let main = main_red4ext_log(&cx.scanned.iter().map(|s| s.log.clone()).collect::<Vec<_>>())
        .and_then(|l| cx.log(&l.name));
    let mut red_started = false;
    let mut red_ran = false;
    let red_text = main.filter(|m| m.in_session).map(|m| m.session_text.as_str()).unwrap_or("");
    if !cx.installed("red4ext") {
        out.push(step("red4ext", "Mod loader (RED4ext)", Status::NotInstalled, "Not installed. RED4ext plugins and the frameworks built on it need it."));
    } else {
        let mut s = match main {
            None => {
                let mut s = step("red4ext", "Mod loader (RED4ext)", Status::Failed, "RED4ext is installed but has never written a log, so it isn't starting.");
                s.fix = Some(format!("Reinstall RED4ext.{}", linux(LINUX_RED4EXT)));
                s
            }
            Some(m) if known && !m.in_session => {
                let mut s = step("red4ext", "Mod loader (RED4ext)", Status::Failed, "RED4ext didn't write a log in the last session, so it didn't start.");
                s.fix = Some(format!("Reinstall RED4ext.{}", linux(LINUX_RED4EXT)));
                s
            }
            Some(_) => {
                red_ran = true;
                let lower = red_text.to_ascii_lowercase();
                red_started = lower.contains("red4ext has been started");
                let errors = cx.issues_in("red4ext/logs/", Level::Error);
                if red_started {
                    step("red4ext", "Mod loader (RED4ext)", Status::Ok, "Started.")
                } else if let Some(e) = errors.first() {
                    let mut s = step("red4ext", "Mod loader (RED4ext)", Status::Failed, "RED4ext reported an error and didn't finish starting.");
                    s.details.push(e.line.clone());
                    s.fix = e.fix.clone();
                    s
                } else {
                    step("red4ext", "Mod loader (RED4ext)", Status::Warning, "RED4ext began starting but didn't report that it finished.")
                }
            }
        };
        s.version = cx.version("red4ext/RED4ext.dll").or_else(|| token_after(red_text, "RED4ext (v").or_else(|| token_after(red_text, "RED4ext v")));
        out.push(s);

        let mut p = step("plugins", "RED4ext plugins", Status::Unknown, "");
        if !red_ran {
            p.status = Status::NotRun;
            p.summary = "Not loaded, because RED4ext didn't run.".into();
        } else {
            let lines: Vec<&str> = red_text.lines().filter(|l| !l.trim().is_empty()).collect();
            let loaded = lines.iter().find_map(|l| {
                let lower = l.to_ascii_lowercase();
                let at = lower.find("plugin(s) loaded")?;
                lower[..at].split_whitespace().last()?.parse::<usize>().ok()
            });
            let died = lines.last().filter(|l| l.to_ascii_lowercase().contains("loading plugin from"));
            let problems: Vec<&Issue> = cx
                .issues
                .iter()
                .filter(|i| i.last_session && i.log.to_ascii_lowercase().starts_with("red4ext/logs/"))
                .filter(|i| {
                    let l = i.line.to_ascii_lowercase();
                    l.contains("plugin") || l.contains(".dll")
                })
                .collect();
            if let Some(line) = died {
                let path = quoted(line).unwrap_or(line);
                let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
                p.status = Status::Failed;
                p.summary = format!("The game stopped while loading {file}.");
                p.fix = Some("Update the mod that ships this plugin. If that doesn't help, remove it until it's updated.".into());
                p.details.push(line.trim().to_string());
                let (ids, names) = mods_in(line, cx.needles);
                add_mods(&mut p, &ids, &names);
            } else if !problems.is_empty() {
                let errors = problems.iter().filter(|i| i.level == Level::Error).count();
                p.status = if errors > 0 { Status::Failed } else { Status::Warning };
                p.summary = if errors > 0 {
                    format!("{} failed to load.", plural(errors, "plugin", "plugins"))
                } else {
                    format!("{} about plugins.", plural(problems.len(), "warning", "warnings"))
                };
                p.fix = problems.iter().find_map(|i| i.fix.clone()).or(Some("Update the mods named here, or remove them until they're updated.".into()));
                for i in problems.iter().take(5) {
                    p.details.push(i.line.clone());
                    add_mods(&mut p, &i.mod_ids, &i.mod_names);
                }
            } else if let Some(n) = loaded {
                p.status = Status::Ok;
                p.summary = format!("{} loaded.", plural(n, "plugin", "plugins"));
            } else if red_started {
                p.status = Status::Ok;
                p.summary = "Loaded.".into();
            } else {
                p.summary = "RED4ext didn't say how its plugins loaded.".into();
            }
        }
        out.push(p);
    }

    // 3. The script compiler.
    if !cx.installed("redscript") {
        out.push(step("redscript", "Script mods (redscript)", Status::NotInstalled, "Not installed. .reds script mods need it."));
    } else {
        let lower = red_text.to_ascii_lowercase();
        let scc_ok = lower.contains("scc invoked successfully");
        let scc_failed = lower.contains("scc invocation failed") || lower.contains("compilation has failed");
        let errors = cx.issues_in(REDSCRIPT_LOG, Level::Error);
        let reds_fresh = cx.log(REDSCRIPT_LOG).is_some_and(|l| l.in_session);
        let mut s = step("redscript", "Script mods (redscript)", Status::Unknown, "");
        if !errors.is_empty() || scc_failed {
            s.status = Status::Failed;
            s.summary = if errors.is_empty() {
                "Script mods failed to compile, so none of them loaded.".into()
            } else {
                format!("{} while compiling, so none of the script mods loaded.", plural(errors.len(), "error", "errors"))
            };
            s.fix = errors.iter().find_map(|i| i.fix.clone()).or(Some("Compile errors like these come from a mod built for another game or framework version, or one missing a requirement. Update the mods named here, or disable them.".into()));
            for i in errors.iter().take(5) {
                s.details.push(i.line.clone());
                add_mods(&mut s, &i.mod_ids, &i.mod_names);
            }
        } else if red_started && !scc_ok && cx.installed("red4ext") {
            s.status = Status::Warning;
            s.summary = "RED4ext started, but the script compiler wasn't run.".into();
            s.fix = Some("Delete the r6/cache folder, verify the game files in your launcher, then reinstall redscript.".into());
        } else if scc_ok || reds_fresh {
            s.status = Status::Ok;
            s.summary = "Compiled without errors.".into();
        } else if known {
            s.status = Status::NotRun;
            s.summary = "No script compile in the last session.".into();
        } else {
            s.summary = "No compile log yet.".into();
        }
        s.version = cx.version("engine/tools/scc.exe");
        out.push(s);
    }

    // 4. Cyber Engine Tweaks.
    if !cx.installed("cet") {
        out.push(step("cet", "Cyber Engine Tweaks (CET)", Status::NotInstalled, "Not installed. CET mods (in-game menus and Lua mods) need it."));
    } else {
        let log = cx.log(CET_LOG);
        let mut s = match log {
            Some(l) if l.in_session => {
                let errors = cx.issues_in(CET_LOG, Level::Error);
                let mut s = if errors.is_empty() {
                    step("cet", "Cyber Engine Tweaks (CET)", Status::Ok, "Started.")
                } else {
                    let mut s = step("cet", "Cyber Engine Tweaks (CET)", Status::Warning, format!("Started with {}.", plural(errors.len(), "error", "errors")));
                    s.details = errors.iter().take(5).map(|i| i.line.clone()).collect();
                    s
                };
                s.version = token_after(&l.session_text, "CET version");
                s
            }
            _ => {
                let mut s = step("cet", "Cyber Engine Tweaks (CET)", Status::Failed,
                    if log.is_none() { "CET is installed but has never written its log, so it isn't starting." } else { "CET didn't write its log in the last session, so it didn't start." });
                s.fix = Some(format!("CET needs the Visual C++ 2015-2022 runtime (14.40 or newer). Reinstall CET.{}", linux(LINUX_CET)));
                s
            }
        };
        if s.version.is_none() {
            s.version = cx.version("bin/x64/plugins/cyber_engine_tweaks.asi");
        }
        out.push(s);
    }

    // 5-7. Frameworks that run as RED4ext plugins.
    for (id, title, dir) in [
        ("archivexl", "Game file extensions (ArchiveXL)", "ArchiveXL"),
        ("tweakxl", "Item and stat changes (TweakXL)", "TweakXL"),
        ("codeware", "Script extensions (Codeware)", "Codeware"),
    ] {
        if !cx.installed(id) {
            continue;
        }
        let prefix = format!("red4ext/plugins/{dir}/");
        let fresh = cx.scanned.iter().any(|s| s.in_session && s.log.name.to_ascii_lowercase().starts_with(&prefix.to_ascii_lowercase()));
        let errors = cx.issues_in(&prefix, Level::Error);
        let mut s = if !red_ran {
            step(id, title, Status::NotRun, "Not loaded, because RED4ext didn't run.")
        } else if !errors.is_empty() {
            let mut s = step(id, title, Status::Warning, format!("Loaded with {}.", plural(errors.len(), "error", "errors")));
            s.fix = errors.iter().find_map(|i| i.fix.clone()).or(Some("The plugin logged errors naming these mods. Update them, or disable them.".into()));
            for i in errors.iter().take(5) {
                s.details.push(i.line.clone());
                add_mods(&mut s, &i.mod_ids, &i.mod_names);
            }
            s
        } else if fresh {
            step(id, title, Status::Ok, "Loaded.")
        } else if known {
            step(id, title, Status::Warning, "No log written during the last session; loading not confirmed.")
        } else {
            step(id, title, Status::Unknown, "No log yet.")
        };
        s.version = cx.version(&format!("red4ext/plugins/{dir}/{dir}.dll"));
        out.push(s);
    }

    // 8. Did the game crash?
    match cx.session {
        Some(sess) if sess.crashed => {
            let mut s = step("crash", "Game crash", Status::Failed, "The game crashed in the last session.");
            let reasons: Vec<&Issue> = cx.issues.iter().filter(|i| i.last_session && i.log.starts_with("crash-report/")).collect();
            if let Some(r) = reasons.iter().find(|i| i.meaning.is_some()) {
                s.summary = format!("The game crashed in the last session. {}", r.meaning.clone().unwrap_or_default());
                s.fix = r.fix.clone();
            }
            s.details = reasons.iter().take(3).map(|i| i.line.clone()).collect();
            out.push(s);
        }
        Some(_) => out.push(step("crash", "Game crash", Status::Ok, "No crash report from the last session.")),
        None => out.push(step("crash", "Game crash", Status::Unknown, "No game session found in the logs yet.")),
    }

    if let Some(first) = out.iter_mut().find(|s| s.status == Status::Failed) {
        first.first_problem = true;
    }
    out
}
