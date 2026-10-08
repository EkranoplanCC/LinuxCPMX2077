//! Known problem mods and log messages, from the modding wiki's
//! troubleshooting pages. The list lives in `data/known_issues.json` so it
//! can grow without code changes; this module only evaluates it.

use std::path::Path;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::game::exists_ci;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// One condition. Exactly one of the fields is set; `any` is true when one
/// of its conditions is.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cond {
    /// A file or folder exists (game-relative, any case).
    path: Option<String>,
    /// An installed mod has this Nexus mod id.
    nexus_mod: Option<i64>,
    /// An installed mod's name contains this (lowercase).
    mod_name: Option<String>,
    /// Something in these folders (two levels deep) has `contains` in its name.
    name_in: Option<Vec<String>>,
    contains: Option<String>,
    /// More than this many mods are installed.
    mod_count_over: Option<usize>,
    /// A log line from the last session contains this (lowercase).
    log_contains: Option<String>,
    any: Option<Vec<Cond>>,
    /// `false`: the condition must hold, but the mods it matches aren't the
    /// ones to blame (e.g. RED4ext in "cybercmd next to RED4ext").
    blame: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
struct Rule {
    id: String,
    severity: Severity,
    title: String,
    explanation: String,
    fix: String,
    link: String,
    all: Vec<Cond>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Explain {
    pub contains: String,
    pub meaning: String,
    pub fix: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Data {
    rules: Vec<Rule>,
    explain: Vec<Explain>,
    noise: Vec<String>,
}

fn data() -> &'static Data {
    static DATA: OnceLock<Data> = OnceLock::new();
    DATA.get_or_init(|| serde_json::from_str(include_str!("../data/known_issues.json")).expect("data/known_issues.json is valid"))
}

/// Plain-language meaning of a log line or crash reason, if it's a known one.
pub fn explain(line: &str) -> Option<&'static Explain> {
    let l = line.to_lowercase();
    data().explain.iter().find(|e| l.contains(&e.contains))
}

/// Harmless lines that look like errors (the wiki and FindAllErrors skip them).
pub fn is_noise(line: &str) -> bool {
    let l = line.to_lowercase();
    data().noise.iter().any(|n| l.contains(n.as_str()))
}

#[derive(Debug, Clone, Serialize)]
pub struct KnownIssue {
    pub id: String,
    pub severity: Severity,
    pub title: String,
    pub explanation: String,
    pub fix: String,
    pub link: String,
    pub mod_ids: Vec<i64>,
    pub mod_names: Vec<String>,
}

/// An installed (enabled) mod as the rules see it.
pub struct ModInfo {
    pub id: i64,
    pub name: String,
    pub nexus_mod_id: Option<i64>,
    /// Game-relative paths, lowercase.
    pub files: Vec<String>,
}

pub struct Context<'a> {
    pub game_dir: &'a Path,
    pub mods: &'a [ModInfo],
    /// Lines from the last session's logs (or all logs when the session is
    /// unknown).
    pub log_lines: &'a [String],
}

/// Entry names (lowercase) up to two levels below a game folder.
fn names_in(game_dir: &Path, dir: &str) -> Vec<String> {
    let base = crate::install::resolve_ci(game_dir, dir);
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&base) else { return out };
    for e in rd.flatten() {
        out.push(e.file_name().to_string_lossy().to_lowercase());
        if e.path().is_dir()
            && let Ok(sub) = std::fs::read_dir(e.path()) {
                out.extend(sub.flatten().map(|s| s.file_name().to_string_lossy().to_lowercase()));
            }
    }
    out
}

/// `None` when the condition is false, else the mods it points at.
fn eval(c: &Cond, cx: &Context) -> Option<Vec<i64>> {
    if let Some(p) = &c.path {
        if !exists_ci(cx.game_dir, p) {
            return None;
        }
        let p = p.to_lowercase();
        return Some(cx.mods.iter().filter(|m| m.files.iter().any(|f| *f == p || f.starts_with(&format!("{p}/")))).map(|m| m.id).collect());
    }
    if let Some(id) = c.nexus_mod {
        let hits: Vec<i64> = cx.mods.iter().filter(|m| m.nexus_mod_id == Some(id)).map(|m| m.id).collect();
        return (!hits.is_empty()).then_some(hits);
    }
    if let Some(n) = &c.mod_name {
        let hits: Vec<i64> = cx.mods.iter().filter(|m| m.name.to_lowercase().contains(n.as_str())).map(|m| m.id).collect();
        return (!hits.is_empty()).then_some(hits);
    }
    if let (Some(dirs), Some(needle)) = (&c.name_in, &c.contains) {
        let on_disk = dirs.iter().any(|d| names_in(cx.game_dir, d).iter().any(|n| n.contains(needle.as_str())));
        let owners: Vec<i64> = cx
            .mods
            .iter()
            .filter(|m| m.files.iter().any(|f| dirs.iter().any(|d| f.starts_with(&d.to_lowercase())) && f.contains(needle.as_str())))
            .map(|m| m.id)
            .collect();
        return (on_disk || !owners.is_empty()).then_some(owners);
    }
    if let Some(n) = c.mod_count_over {
        return (cx.mods.len() > n).then(Vec::new);
    }
    if let Some(s) = &c.log_contains {
        return cx.log_lines.iter().any(|l| l.to_lowercase().contains(s.as_str())).then(Vec::new);
    }
    if let Some(any) = &c.any {
        let mut hit = false;
        let mut ids = Vec::new();
        for c in any {
            if let Some(m) = eval(c, cx) {
                hit = true;
                ids.extend(m);
            }
        }
        return hit.then_some(ids);
    }
    None
}

pub fn check(cx: &Context) -> Vec<KnownIssue> {
    let mut out = Vec::new();
    for r in &data().rules {
        let mut ids: Vec<i64> = Vec::new();
        let mut all = true;
        for c in &r.all {
            match eval(c, cx) {
                Some(m) => {
                    if c.blame != Some(false) {
                        ids.extend(m);
                    }
                }
                None => {
                    all = false;
                    break;
                }
            }
        }
        if !all {
            continue;
        }
        ids.sort();
        ids.dedup();
        let mod_names = ids.iter().filter_map(|id| cx.mods.iter().find(|m| m.id == *id).map(|m| m.name.clone())).collect();
        out.push(KnownIssue {
            id: r.id.clone(),
            severity: r.severity,
            title: r.title.clone(),
            explanation: r.explanation.clone(),
            fix: r.fix.clone(),
            link: r.link.clone(),
            mod_ids: ids,
            mod_names,
        });
    }
    out.sort_by_key(|i| i.severity);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(id: i64, name: &str, nexus: Option<i64>, files: &[&str]) -> ModInfo {
        ModInfo { id, name: name.into(), nexus_mod_id: nexus, files: files.iter().map(|f| f.to_lowercase()).collect() }
    }

    #[test]
    fn data_file_parses_and_explains() {
        assert!(data().rules.len() >= 10);
        assert!(explain("Error reason: Unhandled exception\nExpression: EXCEPTION_ACCESS_VIOLATION").is_some());
        assert!(is_noise("[AMM Error] Non-localized string found: foo"));
        assert!(!is_noise("[error] Could not load plugin"));
    }

    #[test]
    fn matches_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let g = tmp.path();
        for d in ["red4ext", "r6/scripts/virtual-atelier", "r6/scripts/Virtual-Atelier-Full", "archive/pc/mod", "red4ext/plugins/cybercmd"] {
            std::fs::create_dir_all(g.join(d)).unwrap();
        }
        std::fs::write(g.join("red4ext/RED4ext.dll"), b"").unwrap();
        std::fs::write(g.join("archive/pc/mod/modlist.txt"), b"a.archive").unwrap();
        let mods = vec![
            m(1, "KSUV", Some(3783), &["archive/pc/mod/ksuv.archive"]),
            m(2, "VTK", Some(7054), &["archive/pc/mod/vtk.archive"]),
            m(3, "Virtual Atelier", Some(2987), &["r6/scripts/virtual-atelier-full/core/Events.reds"]),
            m(4, "RED4ext", Some(2380), &["red4ext/RED4ext.dll"]),
        ];
        let lines = vec!["[WARN] field with this name is already defined in the class".to_string()];
        let found = check(&Context { game_dir: g, mods: &mods, log_lines: &lines });
        let ids: Vec<&str> = found.iter().map(|i| i.id.as_str()).collect();
        for want in ["cybercmd-with-red4ext", "two-body-frameworks", "virtual-atelier-twice", "modlist-txt", "script-installed-twice"] {
            assert!(ids.contains(&want), "{want} missing from {ids:?}");
        }
        assert!(!ids.contains(&"ctd-helper"));
        // cybercmd is on disk but not from a tracked mod; RED4ext isn't blamed.
        assert!(found.iter().find(|i| i.id == "cybercmd-with-red4ext").unwrap().mod_ids.is_empty());
        let body = found.iter().find(|i| i.id == "two-body-frameworks").unwrap();
        assert_eq!(body.mod_ids, vec![1, 2]);
        let va = found.iter().find(|i| i.id == "virtual-atelier-twice").unwrap();
        assert_eq!(va.mod_names, vec!["Virtual Atelier".to_string()]);
        // Errors sort first.
        assert_eq!(found[0].severity, Severity::Error);
    }
}
