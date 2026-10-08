//! What the Ultra+ team recommends next to Ultra+ (Nexus mod 10490): mods
//! that conflict with its path tracing, mods to add, mods for ray tracing
//! only, and what it needs. The lists come from the Ultra+ Cyberpunk page
//! (theultraplace.com), whose source is a public GitLab repository. A copy
//! ships in `data/ultraplus.json`; "Refresh" re-reads the page and keeps the
//! result in the settings table.

use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::db::{Db, ModRow};
use crate::dependencies::DepState;
use crate::game::exists_ci;
use crate::known_issues::{KnownIssue, ModInfo, Severity};
use crate::{APP_NAME, APP_VERSION, Error, Result};

pub const NEXUS_MOD_ID: i64 = 10490;
/// The page people read.
pub const PAGE_URL: &str = "https://theultraplace.com/games/cyberpunk2077/";
/// The same page's source, which the app reads.
pub const SOURCE_URL: &str = "https://gitlab.com/ultra-plus/ultraplus-wiki/-/raw/main/_games/Cyberpunk2077.md";
/// Folders Ultra+ installs, for copies not installed through the manager
/// (by hand or with Ultra+ Manager).
const MARKERS: &[&str] = &["bin/x64/plugins/cyber_engine_tweaks/mods/UltraPlus", "red4ext/plugins/UltraTool"];
const SETTING: &str = "ultraplus_guide";
const MAX_PAGE_BYTES: usize = 2 << 20;
const MAX_NAME: usize = 120;
const MAX_NOTE: usize = 300;
const MAX_ITEMS: usize = 60;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pick {
    #[serde(default)]
    pub nexus_mod_id: Option<i64>,
    pub name: String,
    #[serde(default)]
    pub note: Option<String>,
    /// Off-Nexus home page.
    #[serde(default)]
    pub url: Option<String>,
    /// Game-relative paths that show it's installed without the manager.
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionRef {
    pub slug: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Guide {
    /// Not compatible with Ultra+ path tracing.
    pub conflicts: Vec<Pick>,
    /// For path tracing and ray tracing.
    pub recommended: Vec<Pick>,
    /// In addition, for ray tracing (not path tracing).
    pub ray_tracing: Vec<Pick>,
    pub required: Vec<Pick>,
    /// Conflicts Ultra+ checks for in game that the page doesn't list.
    #[serde(default)]
    pub extra_conflicts: Vec<Pick>,
    #[serde(default)]
    pub collection: Option<CollectionRef>,
    /// When the built-in list was taken from the page (YYYY-MM-DD).
    #[serde(default)]
    pub checked: Option<String>,
    /// Unix time of the last refresh from the page; `None` for the built-in list.
    #[serde(default)]
    pub fetched_at: Option<i64>,
}

fn bundled_ref() -> &'static Guide {
    static DATA: OnceLock<Guide> = OnceLock::new();
    DATA.get_or_init(|| serde_json::from_str(include_str!("../data/ultraplus.json")).expect("data/ultraplus.json is valid"))
}

pub fn bundled() -> Guide {
    bundled_ref().clone()
}

/// The last refreshed list, or the built-in one.
pub fn saved(db: &Db) -> Guide {
    db.get_setting(SETTING)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(bundled)
}

/// Read the page again. Network only, so callers can do it without holding
/// the database; keep the result with [`save`].
pub fn fetch() -> Result<Guide> {
    let http = reqwest::blocking::Client::builder()
        .user_agent(format!("{APP_NAME}/{APP_VERSION}"))
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(30))
        .build()?;
    let resp = http.get(SOURCE_URL).send()?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!("The Ultra+ page answered {}", resp.status())));
    }
    let bytes = resp.bytes()?;
    if bytes.len() > MAX_PAGE_BYTES {
        return Err(Error::Other("The Ultra+ page is unexpectedly large; kept the previous list".into()));
    }
    let mut guide = parse(&String::from_utf8_lossy(&bytes))?;
    guide.fetched_at = Some(crate::nexus::now_unix());
    Ok(guide)
}

pub fn save(db: &Db, guide: &Guide) -> Result<()> {
    db.set_setting(SETTING, &serde_json::to_string(guide)?)
}

#[derive(Clone, Copy, PartialEq)]
enum Section {
    None,
    Conflicts,
    Recommended,
    RayTracing,
    Required,
}

/// The four lists from the page's Markdown. Only list items that link to a
/// Cyberpunk mod on Nexus count; the rest of the page is ignored. Built-in
/// extras (RedHotTools, the Ultrapunk collection) carry over.
pub fn parse(md: &str) -> Result<Guide> {
    let base = bundled_ref();
    let mut g = Guide {
        conflicts: Vec::new(),
        recommended: Vec::new(),
        ray_tracing: Vec::new(),
        required: Vec::new(),
        extra_conflicts: base.extra_conflicts.clone(),
        collection: base.collection.clone(),
        checked: None,
        fetched_at: None,
    };
    let mut section = Section::None;
    for line in md.lines() {
        let t = line.trim();
        if t.starts_with('#') {
            let h = t.to_lowercase();
            section = if h.contains("conflict") || h.contains("incompatib") {
                Section::Conflicts
            } else if h.contains("recommended") && (h.contains("only") || h.contains("rt+pt")) {
                Section::RayTracing
            } else if h.contains("recommended") {
                Section::Recommended
            } else if h.starts_with("## ") && h.contains("requirement") {
                Section::Required
            } else {
                Section::None
            };
            continue;
        }
        // "**Recommended for Path Tracing:**" ends the required list.
        if section == Section::Required && t.starts_with("**") && !t.to_lowercase().starts_with("**required") {
            section = Section::None;
            continue;
        }
        let Some(item) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) else { continue };
        let Some(pick) = parse_item(item) else { continue };
        let list = match section {
            Section::Conflicts => &mut g.conflicts,
            Section::Recommended => &mut g.recommended,
            Section::RayTracing => &mut g.ray_tracing,
            Section::Required => &mut g.required,
            Section::None => continue,
        };
        if list.len() < MAX_ITEMS && !list.iter().any(|p| p.nexus_mod_id == pick.nexus_mod_id) {
            list.push(pick);
        }
    }
    if g.conflicts.is_empty() && g.recommended.is_empty() {
        return Err(Error::Other("Couldn't find the recommended mods on the Ultra+ page (it may have changed); kept the previous list".into()));
    }
    Ok(g)
}

/// `[Name](https://www.nexusmods.com/cyberpunk2077/mods/123) — note`
fn parse_item(item: &str) -> Option<Pick> {
    let rest = item.strip_prefix('[')?;
    let (name, rest) = rest.split_once("](")?;
    let (url, rest) = rest.split_once(')')?;
    let id = nexus_mod_id(url)?;
    let note = rest.trim_start_matches(|c: char| c.is_whitespace() || matches!(c, '—' | '–' | '-' | ':'));
    Some(Pick { nexus_mod_id: Some(id), name: clean(name, MAX_NAME)?, note: clean(note, MAX_NOTE), url: None, paths: Vec::new() })
}

fn nexus_mod_id(url: &str) -> Option<i64> {
    let u = url::Url::parse(url).ok()?;
    if !matches!(u.host_str()?, "www.nexusmods.com" | "nexusmods.com") {
        return None;
    }
    let mut seg = u.path_segments()?;
    if !seg.next()?.eq_ignore_ascii_case("cyberpunk2077") || seg.next()? != "mods" {
        return None;
    }
    seg.next()?.parse().ok().filter(|i| *i > 0)
}

/// Plain text: Markdown emphasis and HTML tags dropped, control characters out.
fn clean(s: &str, max: usize) -> Option<String> {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if in_tag || c.is_control() || c == '*' || c == '`' => {}
            _ => out.push(c),
        }
    }
    let out: String = out.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(max).collect();
    (!out.is_empty()).then_some(out)
}

#[derive(Debug, Clone, Serialize)]
pub struct Item {
    #[serde(flatten)]
    pub pick: Pick,
    pub state: DepState,
    /// The installed mod, when the manager installed it.
    pub installed_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// Ultra+ is in the game folder.
    pub detected: bool,
    /// The manager's entry for Ultra+, if it installed it.
    pub mod_id: Option<i64>,
    pub version: Option<String>,
    pub conflicts: Vec<Item>,
    pub recommended: Vec<Item>,
    pub ray_tracing: Vec<Item>,
    pub required: Vec<Item>,
    pub collection: Option<CollectionRef>,
    pub checked: Option<String>,
    pub fetched_at: Option<i64>,
    pub page_url: &'static str,
}

fn state_of(p: &Pick, mods: &[ModRow], game_dir: &Path) -> (DepState, Option<i64>) {
    let m = p.nexus_mod_id.and_then(|id| mods.iter().find(|m| m.nexus_mod_id == Some(id)));
    let on_disk = p.paths.iter().any(|f| exists_ci(game_dir, f))
        || match p.nexus_mod_id {
            Some(107) => exists_ci(game_dir, "bin/x64/plugins/cyber_engine_tweaks.asi"),
            Some(2380) => exists_ci(game_dir, "red4ext/RED4ext.dll"),
            _ => false,
        };
    match m {
        Some(m) if m.enabled() => (DepState::Installed, Some(m.id)),
        Some(m) if on_disk => (DepState::Present, Some(m.id)),
        Some(m) => (DepState::Disabled, Some(m.id)),
        None if on_disk => (DepState::Present, None),
        None => (DepState::Missing, None),
    }
}

/// Whether Ultra+ is installed (and switched on), and where the manager has it.
pub fn find<'a>(game_dir: &Path, mods: &'a [ModRow]) -> (bool, Option<&'a ModRow>) {
    let m = mods.iter().find(|m| m.nexus_mod_id == Some(NEXUS_MOD_ID));
    let detected = m.is_some_and(|m| m.enabled()) || MARKERS.iter().any(|p| exists_ci(game_dir, p));
    (detected, m)
}

pub fn report(game_dir: &Path, mods: &[ModRow], guide: &Guide) -> Report {
    let (detected, m) = find(game_dir, mods);
    let items = |list: &[Pick]| -> Vec<Item> {
        list.iter()
            .map(|p| {
                let (state, installed_id) = state_of(p, mods, game_dir);
                Item { pick: p.clone(), state, installed_id }
            })
            .collect()
    };
    let mut conflicts = items(&guide.conflicts);
    conflicts.extend(items(&guide.extra_conflicts));
    Report {
        detected,
        mod_id: m.map(|m| m.id),
        version: m.and_then(|m| m.version.clone()),
        conflicts,
        recommended: items(&guide.recommended),
        ray_tracing: items(&guide.ray_tracing),
        required: items(&guide.required),
        collection: guide.collection.clone(),
        checked: guide.checked.clone(),
        fetched_at: guide.fetched_at,
        page_url: PAGE_URL,
    }
}

/// Netrunner's known problems: installed mods Ultra+ conflicts with, and the
/// setting notes for recommended mods that are installed.
pub fn known_issues(game_dir: &Path, mods: &[ModInfo], guide: &Guide) -> Vec<KnownIssue> {
    let installed = mods.iter().any(|m| m.nexus_mod_id == Some(NEXUS_MOD_ID)) || MARKERS.iter().any(|p| exists_ci(game_dir, p));
    if !installed {
        return Vec::new();
    }
    let owners = |p: &Pick| -> Vec<&ModInfo> {
        mods.iter()
            .filter(|m| {
                p.nexus_mod_id.is_some_and(|id| m.nexus_mod_id == Some(id))
                    || p.paths.iter().any(|f| {
                        let f = f.to_lowercase();
                        m.files.iter().any(|x| *x == f || x.starts_with(&format!("{f}/")))
                    })
            })
            .collect()
    };
    let issue = |id: String, severity, title: String, explanation: String, fix: String, hits: Vec<&ModInfo>| KnownIssue {
        id,
        severity,
        title,
        explanation,
        fix,
        link: PAGE_URL.into(),
        mod_ids: hits.iter().map(|m| m.id).collect(),
        mod_names: hits.iter().map(|m| m.name.clone()).collect(),
    };
    let mut out = Vec::new();
    for p in guide.conflicts.iter().chain(&guide.extra_conflicts) {
        let hits = owners(p);
        let on_disk = p.paths.iter().any(|f| exists_ci(game_dir, f));
        if hits.is_empty() && !on_disk {
            continue;
        }
        let rt = guide.ray_tracing.iter().any(|r| r.nexus_mod_id.is_some() && r.nexus_mod_id == p.nexus_mod_id);
        let explanation = match (&p.note, rt) {
            (Some(n), _) => format!("{n}."),
            (None, true) => "The Ultra+ team lists it as not compatible with Ultra+ path tracing, but recommends it when you play with ray tracing only.".into(),
            (None, false) => "The Ultra+ team lists it as not compatible with Ultra+ path tracing.".into(),
        };
        let fix = if rt {
            format!("If you play with path tracing, turn off or uninstall {}. Keep it for ray tracing only.", p.name)
        } else {
            format!("Turn off or uninstall {}.", p.name)
        };
        out.push(issue(
            format!("ultraplus-conflict-{}", p.nexus_mod_id.map(|i| i.to_string()).unwrap_or_else(|| p.name.to_lowercase())),
            Severity::Warning,
            format!("{} conflicts with Ultra+", p.name),
            explanation,
            fix,
            hits,
        ));
    }
    for p in guide.recommended.iter().filter(|p| p.note.is_some()) {
        let hits = owners(p);
        if hits.is_empty() {
            continue;
        }
        out.push(issue(
            format!("ultraplus-note-{}", p.nexus_mod_id.unwrap_or_default()),
            Severity::Info,
            format!("Ultra+ setting for {}", p.name),
            "The Ultra+ team recommends this mod with Ultra+, with a setting change.".into(),
            p.note.clone().unwrap_or_default(),
            hits,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = "---
layout: game
---
## Overview
Some [link](https://www.nexusmods.com/cyberpunk2077/mods/1) in text.

### ⚠️ Mod Conflicts (Path Tracing Only)
> The following mods are **not compatible** with V4 Path Tracing:

- [Disable Fake Lights](https://www.nexusmods.com/cyberpunk2077/mods/16060)
- [Elsewhere](https://example.com/mods/5)

### ✅ Recommended Mods → <u>Path Tracing and Ray Tracing</u>

> **Tip:** use the [Ultrapunk](https://ultrapunk.theultraplace.com/) collection.

- [Draw Distance ReImagined](https://www.nexusmods.com/cyberpunk2077/mods/27210)
- [The <b>Nullifier</b>](https://www.nexusmods.com/cyberpunk2077/mods/23091) — **Important:** set 'Fix Broken PT Lights' to disabled
- [Other game](https://www.nexusmods.com/skyrim/mods/3)

### ✅ Recommended Mods → <u>Ray Tracing/RT+PT only</u>

- [Ray Traced Lighting Fixes](https://www.nexusmods.com/cyberpunk2077/mods/29720)

## Requirements

**Required:**
- [Cyber Engine Tweaks (CET)](https://www.nexusmods.com/cyberpunk2077/mods/107)
- [RED4ext](https://www.nexusmods.com/cyberpunk2077/mods/2380)

**Recommended for Path Tracing:**
- [Should not count](https://www.nexusmods.com/cyberpunk2077/mods/9)

## Usage Tips
- [Nor this](https://www.nexusmods.com/cyberpunk2077/mods/8)
";

    fn ids(list: &[Pick]) -> Vec<i64> {
        list.iter().filter_map(|p| p.nexus_mod_id).collect()
    }

    #[test]
    fn parses_page_lists() {
        let g = parse(PAGE).unwrap();
        assert_eq!(ids(&g.conflicts), vec![16060]);
        assert_eq!(ids(&g.recommended), vec![27210, 23091]);
        assert_eq!(ids(&g.ray_tracing), vec![29720]);
        assert_eq!(ids(&g.required), vec![107, 2380]);
        let nul = &g.recommended[1];
        assert_eq!(nul.name, "The Nullifier");
        assert_eq!(nul.note.as_deref(), Some("Important: set 'Fix Broken PT Lights' to disabled"));
        assert_eq!(g.recommended[0].note, None);
        // Extras carry over from the built-in file.
        assert!(g.extra_conflicts.iter().any(|p| p.name == "RedHotTools"));
        assert_eq!(g.collection.unwrap().slug, "eem6yz");
    }

    #[test]
    fn rejects_a_page_without_lists() {
        assert!(parse("# Nothing here\n- [x](https://www.nexusmods.com/cyberpunk2077/mods/1)").is_err());
    }

    #[test]
    fn bundled_list_is_valid() {
        let g = bundled();
        assert!(g.conflicts.len() >= 5 && g.recommended.len() >= 8);
        assert!(g.recommended.iter().chain(&g.conflicts).all(|p| p.nexus_mod_id.is_some()));
    }

    fn row(id: i64, nexus: i64, status: &str) -> ModRow {
        ModRow {
            id,
            game_id: 1,
            name: format!("mod {nexus}"),
            version: Some("9.3.10".into()),
            source: "nexus".into(),
            nexus_mod_id: Some(nexus),
            nexus_file_id: None,
            archive_name: String::new(),
            archive_sha256: String::new(),
            archive_md5: String::new(),
            game_build_id: None,
            game_version: None,
            status: status.into(),
            installed_at: String::new(),
            file_count: 1,
            category: None,
            source_ref: None,
            source_file: None,
        }
    }

    #[test]
    fn reports_states() {
        let tmp = tempfile::tempdir().unwrap();
        let g = tmp.path();
        let guide = bundled();
        assert!(!report(g, &[], &guide).detected);

        std::fs::create_dir_all(g.join("red4ext/plugins/ultratool")).unwrap();
        std::fs::create_dir_all(g.join("red4ext/plugins/RedHotTools")).unwrap();
        std::fs::write(g.join("red4ext/RED4ext.dll"), b"").unwrap();
        let mods = vec![row(1, 16060, "installed"), row(2, 27210, "disabled")];
        let r = report(g, &mods, &guide);
        assert!(r.detected, "found by folder, any case");
        assert_eq!(r.mod_id, None);
        let state = |list: &[Item], name: &str| list.iter().find(|i| i.pick.name.contains(name)).unwrap().state;
        assert_eq!(state(&r.conflicts, "Disable Fake Lights"), DepState::Installed);
        assert_eq!(state(&r.conflicts, "RedHotTools"), DepState::Present);
        assert_eq!(state(&r.recommended, "Draw Distance ReImagined"), DepState::Disabled);
        assert_eq!(state(&r.recommended, "Always Best Quality"), DepState::Missing);
        assert_eq!(state(&r.required, "RED4ext"), DepState::Present);
        assert_eq!(state(&r.required, "Cyber Engine Tweaks"), DepState::Missing);

        let up = vec![row(3, NEXUS_MOD_ID, "installed")];
        let r = report(tmp.path(), &up, &guide);
        assert_eq!((r.mod_id, r.version.as_deref()), (Some(3), Some("9.3.10")));
    }

    #[test]
    fn known_issue_cards() {
        let tmp = tempfile::tempdir().unwrap();
        let g = tmp.path();
        let guide = bundled();
        let info = |id, name: &str, nexus, files: &[&str]| ModInfo {
            id,
            name: name.into(),
            nexus_mod_id: Some(nexus),
            files: files.iter().map(|f| f.to_lowercase()).collect(),
        };
        let mods = vec![info(1, "DFL", 16060, &[]), info(2, "Faster Rainmap", 8610, &[]), info(3, "Nullifier", 23091, &[])];
        assert!(known_issues(g, &mods, &guide).is_empty(), "nothing without Ultra+");

        let mut with = mods;
        with.push(info(4, "Ultra+", NEXUS_MOD_ID, &[]));
        let found = known_issues(g, &with, &guide);
        let ids: Vec<&str> = found.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["ultraplus-conflict-16060", "ultraplus-conflict-8610", "ultraplus-note-23091"]);
        assert!(found[0].fix.contains("ray tracing only"), "listed for RT: keep it there");
        assert!(!found[1].fix.contains("ray tracing"));
        assert_eq!(found[2].severity, Severity::Info);
        assert_eq!(found[0].mod_names, vec!["DFL".to_string()]);
    }
}
