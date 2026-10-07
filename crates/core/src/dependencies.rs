//! What each installed mod needs, and whether the user has it. Two sources:
//! the requirements the author listed on the mod's Nexus page, and the
//! frameworks the compatibility scan saw the mod use (a `.reds` file needs
//! redscript, an `.xl` file ArchiveXL, and so on).

use std::collections::{BTreeSet, HashMap};

use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::Result;
use crate::db::{Db, ModRow};
use crate::nexus_browse::ListedRequirement;

/// Core frameworks: key in `game::detect_frameworks`, display name, Nexus
/// mod id, GitHub repository.
pub const FRAMEWORKS: &[(&str, &str, Option<i64>, Option<&str>)] = &[
    ("cet", "Cyber Engine Tweaks", Some(107), Some("maximegmd/cyberenginetweaks")),
    ("red4ext", "RED4ext", Some(2380), Some("wopss/red4ext")),
    ("redscript", "redscript", Some(1511), Some("jac3km4/redscript")),
    ("archivexl", "ArchiveXL", Some(4198), Some("psiberx/cp2077-archive-xl")),
    ("tweakxl", "TweakXL", Some(4197), Some("psiberx/cp2077-tweak-xl")),
    ("codeware", "Codeware", Some(7780), Some("psiberx/cp2077-codeware")),
    // Ships with the game (an optional Steam/GOG component).
    ("redmod", "REDmod", None, None),
];

fn framework(key: &str) -> Option<&'static (&'static str, &'static str, Option<i64>, Option<&'static str>)> {
    FRAMEWORKS.iter().find(|f| f.0 == key)
}

fn framework_for_nexus(id: i64) -> Option<&'static str> {
    FRAMEWORKS.iter().find(|f| f.2 == Some(id)).map(|f| f.0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DepState {
    /// Installed and enabled through the manager.
    Installed,
    /// Installed through the manager but switched off.
    Disabled,
    /// In the game folder, put there some other way (by hand, or a game
    /// component like REDmod or Phantom Liberty).
    Present,
    Missing,
    /// Can't be checked from here (a mod hosted elsewhere).
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct Dependency {
    pub name: String,
    pub state: DepState,
    /// The installed mod that provides it.
    pub installed_id: Option<i64>,
    /// To get it from Nexus.
    pub nexus_mod_id: Option<i64>,
    /// Framework key, to install it from GitHub with what it needs.
    pub framework: Option<String>,
    /// Where to get it, for requirements hosted outside Nexus.
    pub url: Option<String>,
    pub notes: Option<String>,
    /// Listed on the mod's Nexus page (vs. only seen in its files).
    pub listed: bool,
    /// Seen in the mod's files.
    pub detected: bool,
    pub dlc: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModDependencies {
    pub mod_id: i64,
    pub deps: Vec<Dependency>,
    /// Installed mods that need this one.
    pub required_by: Vec<i64>,
}

/// What the game folder already has, outside the manager's own mods.
#[derive(Debug, Clone, Default)]
pub struct GameHas {
    /// Framework keys found in the game folder.
    pub frameworks: BTreeSet<String>,
    pub phantom_liberty: bool,
}

pub fn game_has(game_dir: &std::path::Path) -> GameHas {
    GameHas {
        frameworks: crate::game::detect_frameworks(game_dir).into_iter().filter(|f| f.installed).map(|f| f.id).collect(),
        phantom_liberty: crate::game::exists_ci(game_dir, "archive/pc/ep1"),
    }
}

/// Every installed mod's dependencies. `requirements` maps a Nexus mod id
/// to what its page lists; `detected` maps an installed mod's id to the
/// framework keys its files use.
pub fn resolve(
    mods: &[ModRow],
    requirements: &Requirements,
    detected: &HashMap<i64, BTreeSet<String>>,
    has: &GameHas,
) -> Vec<ModDependencies> {
    let by_nexus: HashMap<i64, &ModRow> = mods.iter().filter_map(|m| Some((m.nexus_mod_id?, m))).collect();
    // The installed mod that is framework `key`, from Nexus or GitHub.
    let framework_mod = |key: &str| -> Option<&ModRow> {
        let (_, _, nexus, repo) = framework(key)?;
        nexus
            .and_then(|n| by_nexus.get(&n).copied())
            .or_else(|| mods.iter().find(|m| m.source_ref.as_deref().zip(*repo).is_some_and(|(r, f)| r.eq_ignore_ascii_case(f))))
    };
    let state_of = |m: Option<&ModRow>, present: bool| match m {
        Some(m) if m.enabled() => (DepState::Installed, Some(m.id)),
        Some(m) if present => (DepState::Present, Some(m.id)),
        Some(m) => (DepState::Disabled, Some(m.id)),
        None if present => (DepState::Present, None),
        None => (DepState::Missing, None),
    };

    let mut out: Vec<ModDependencies> = mods
        .iter()
        .map(|m| {
            let mut deps: Vec<Dependency> = Vec::new();
            for r in m.nexus_mod_id.and_then(|id| requirements.get(&id)).into_iter().flatten() {
                if r.dlc {
                    let pl = r.name.to_lowercase().contains("phantom liberty");
                    deps.push(Dependency {
                        name: r.name.clone(),
                        state: if pl && has.phantom_liberty { DepState::Present } else if pl { DepState::Missing } else { DepState::Unknown },
                        installed_id: None,
                        nexus_mod_id: None,
                        framework: None,
                        url: None,
                        notes: None,
                        listed: true,
                        detected: false,
                        dlc: true,
                    });
                    continue;
                }
                let fw = r.mod_id.and_then(framework_for_nexus);
                let (state, installed_id) = match (r.mod_id, fw) {
                    (_, Some(key)) => state_of(framework_mod(key), has.frameworks.contains(key)),
                    (Some(id), None) => state_of(by_nexus.get(&id).copied(), false),
                    (None, None) => (DepState::Unknown, None),
                };
                deps.push(Dependency {
                    name: fw.and_then(framework).map(|f| f.1.to_string()).unwrap_or_else(|| r.name.clone()),
                    state,
                    installed_id,
                    nexus_mod_id: r.mod_id,
                    framework: fw.map(str::to_string),
                    url: r.url.clone(),
                    notes: r.notes.clone(),
                    listed: true,
                    detected: false,
                    dlc: false,
                });
            }
            let mut keys: BTreeSet<String> = detected.get(&m.id).cloned().unwrap_or_default();
            // ArchiveXL, TweakXL and Codeware load through RED4ext.
            if ["archivexl", "tweakxl", "codeware"].iter().any(|k| keys.contains(*k)) {
                keys.insert("red4ext".into());
            }
            for key in keys {
                let Some(&(key, name, nexus, _)) = framework(&key) else { continue };
                // A framework doesn't depend on itself.
                if framework_mod(key).is_some_and(|f| f.id == m.id) || nexus.is_some_and(|n| m.nexus_mod_id == Some(n)) {
                    continue;
                }
                if let Some(d) = deps.iter_mut().find(|d| d.framework.as_deref() == Some(key)) {
                    d.detected = true;
                    continue;
                }
                let (state, installed_id) = state_of(framework_mod(key), has.frameworks.contains(key));
                deps.push(Dependency {
                    name: name.to_string(),
                    state,
                    installed_id,
                    nexus_mod_id: nexus,
                    framework: Some(key.to_string()),
                    url: None,
                    notes: None,
                    listed: false,
                    detected: true,
                    dlc: false,
                });
            }
            ModDependencies { mod_id: m.id, deps, required_by: Vec::new() }
        })
        .collect();

    let mut needed_by: HashMap<i64, Vec<i64>> = HashMap::new();
    for md in &out {
        for d in &md.deps {
            if let Some(id) = d.installed_id {
                needed_by.entry(id).or_default().push(md.mod_id);
            }
        }
    }
    for md in &mut out {
        md.required_by = needed_by.remove(&md.mod_id).unwrap_or_default();
    }
    out
}

/// Framework keys each installed mod's files use, from the compatibility
/// index (mods not indexed yet are left out).
pub fn detected_frameworks(db: &Db, mods: &[ModRow]) -> Result<HashMap<i64, BTreeSet<String>>> {
    let mut out = HashMap::new();
    for m in mods {
        let keys: BTreeSet<String> = db
            .touches(m.id)?
            .into_iter()
            .filter(|t| t.kind == crate::analysis::Kind::Requires)
            .map(|t| t.key)
            .collect();
        out.insert(m.id, keys);
    }
    Ok(out)
}

/// How long a mod's Nexus requirements are reused before asking again.
pub const REQUIREMENTS_TTL_SECS: i64 = 24 * 3600;

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Nexus mod id -> what its page lists.
pub type Requirements = HashMap<i64, Vec<ListedRequirement>>;

/// Stored requirements for `ids`, and the ids that need fetching (never
/// fetched, or older than the TTL).
pub fn cached_requirements(db: &Db, ids: &[i64]) -> Result<(Requirements, Vec<i64>)> {
    let mut have = HashMap::new();
    let mut stale = Vec::new();
    let mut st = db.conn.prepare("SELECT requirements, fetched_at FROM nexus_requirements WHERE nexus_mod_id = ?1")?;
    for &id in ids {
        let row: Option<(String, i64)> = st.query_row([id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        match row {
            Some((json, at)) => {
                if let Ok(list) = serde_json::from_str::<Vec<ListedRequirement>>(&json) {
                    have.insert(id, list);
                }
                if now() - at > REQUIREMENTS_TTL_SECS {
                    stale.push(id);
                }
            }
            None => stale.push(id),
        }
    }
    Ok((have, stale))
}

pub fn store_requirements(db: &Db, id: i64, list: &[ListedRequirement]) -> Result<()> {
    db.conn.execute(
        "INSERT INTO nexus_requirements (nexus_mod_id, requirements, fetched_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(nexus_mod_id) DO UPDATE SET requirements = excluded.requirements, fetched_at = excluded.fetched_at",
        params![id, serde_json::to_string(list)?, now()],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64, name: &str, nexus: Option<i64>, source_ref: Option<&str>, enabled: bool) -> ModRow {
        ModRow {
            id,
            game_id: 1,
            name: name.into(),
            version: None,
            source: if nexus.is_some() { "nexus".into() } else if source_ref.is_some() { "github".into() } else { "manual".into() },
            nexus_mod_id: nexus,
            nexus_file_id: nexus.map(|n| n * 10),
            archive_name: String::new(),
            archive_sha256: String::new(),
            archive_md5: String::new(),
            game_build_id: None,
            game_version: None,
            status: if enabled { "installed".into() } else { "disabled".into() },
            installed_at: String::new(),
            file_count: 1,
            category: None,
            source_ref: source_ref.map(Into::into),
            source_file: None,
        }
    }

    fn req(mod_id: Option<i64>, name: &str) -> ListedRequirement {
        ListedRequirement { mod_id, name: name.into(), notes: None, url: None, dlc: false }
    }

    #[test]
    fn resolves_listed_and_detected_dependencies() {
        let mods = vec![
            row(1, "Some Outfit", Some(5000), None, true),
            row(2, "ArchiveXL", None, Some("psiberx/cp2077-archive-xl"), true),
            row(3, "Appearance Menu Mod", Some(790), None, false),
            row(4, "Hand-made", None, None, true),
        ];
        let mut reqs = HashMap::new();
        reqs.insert(5000, vec![
            req(Some(4198), "ArchiveXL"),
            req(Some(790), "AMM"),
            req(Some(9999), "Some Library"),
            ListedRequirement { mod_id: None, name: "Blender".into(), notes: None, url: Some("https://blender.org".into()), dlc: false },
            ListedRequirement { mod_id: None, name: "Phantom Liberty".into(), notes: None, url: None, dlc: true },
        ]);
        let mut detected = HashMap::new();
        detected.insert(1, BTreeSet::from(["archivexl".to_string(), "tweakxl".to_string()]));
        detected.insert(2, BTreeSet::from(["red4ext".to_string()]));
        detected.insert(4, BTreeSet::from(["cet".to_string(), "redmod".to_string()]));
        let has = GameHas { frameworks: BTreeSet::from(["red4ext".to_string(), "redmod".to_string()]), phantom_liberty: true };

        let r = resolve(&mods, &reqs, &detected, &has);
        let outfit = &r[0].deps;
        let names: Vec<(&str, DepState)> = outfit.iter().map(|d| (d.name.as_str(), d.state)).collect();
        assert_eq!(names, [
            ("ArchiveXL", DepState::Installed),
            ("AMM", DepState::Disabled),
            ("Some Library", DepState::Missing),
            ("Blender", DepState::Unknown),
            ("Phantom Liberty", DepState::Present),
            ("RED4ext", DepState::Present),
            ("TweakXL", DepState::Missing),
        ]);
        let axl = &outfit[0];
        assert!(axl.listed && axl.detected, "listed and seen in its files: one entry");
        assert_eq!(axl.installed_id, Some(2), "matched to the GitHub install");
        assert_eq!(outfit[6].nexus_mod_id, Some(4197));
        assert_eq!(outfit[6].framework.as_deref(), Some("tweakxl"));

        // ArchiveXL itself doesn't need ArchiveXL; it does need RED4ext.
        assert_eq!(r[1].deps.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["RED4ext"]);
        assert_eq!(r[1].required_by, [1]);
        assert_eq!(r[2].required_by, [1]);
        let hand: Vec<(&str, DepState)> = r[3].deps.iter().map(|d| (d.name.as_str(), d.state)).collect();
        assert_eq!(hand, [("Cyber Engine Tweaks", DepState::Missing), ("REDmod", DepState::Present)]);
    }

    #[test]
    fn caches_requirements() {
        let db = Db::open_in_memory().unwrap();
        let (have, stale) = cached_requirements(&db, &[1, 2]).unwrap();
        assert!(have.is_empty());
        assert_eq!(stale, [1, 2]);
        store_requirements(&db, 1, &[req(Some(2380), "RED4ext")]).unwrap();
        store_requirements(&db, 2, &[]).unwrap();
        let (have, stale) = cached_requirements(&db, &[1, 2]).unwrap();
        assert_eq!(have[&1][0].mod_id, Some(2380));
        assert!(have[&2].is_empty());
        assert!(stale.is_empty());
        db.conn.execute("UPDATE nexus_requirements SET fetched_at = 0 WHERE nexus_mod_id = 2", []).unwrap();
        assert_eq!(cached_requirements(&db, &[1, 2]).unwrap().1, [2]);
    }
}
