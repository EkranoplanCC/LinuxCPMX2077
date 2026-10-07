//! The game's version history for the Installed mods tab: one entry per game
//! version the manager has seen (a new one each time the game updates), the
//! mods installed on each, and what changed in the mod list since an older
//! version.

use serde::Serialize;

use crate::Result;
use crate::db::{Db, GameVersionRow, ModRow, SnapshotMod};

#[derive(Debug, Clone, Serialize)]
pub struct VersionEntry {
    /// `game_versions.id`; `None` for a version only known from mods
    /// installed before the manager kept a history.
    pub id: Option<i64>,
    pub version: Option<String>,
    pub build_id: Option<String>,
    pub first_seen: Option<String>,
    pub left_at: Option<String>,
    pub current: bool,
    /// Installed mods that were installed (or last updated) on this version.
    pub mod_ids: Vec<i64>,
    /// Mods installed when the game moved on from this version.
    pub snapshot_count: usize,
}

fn builds_match(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    }
}

/// The version history, oldest first, with each installed mod placed on the
/// version it was installed on.
pub fn history(db: &Db, game_id: i64) -> Result<Vec<VersionEntry>> {
    let rows = db.game_versions(game_id)?;
    let mods = db.mods(game_id)?;
    let mut out = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        out.push(entry(db, r, i + 1 == rows.len())?);
    }
    let mut older: Vec<VersionEntry> = Vec::new();
    for m in &mods {
        // Prefer the entry with the same build, then the newest with the
        // same version.
        let found = out
            .iter_mut()
            .rev()
            .filter(|e| e.version == m.game_version)
            .find(|e| builds_match(e.build_id.as_deref(), m.game_build_id.as_deref()));
        match found {
            Some(e) => e.mod_ids.push(m.id),
            None => match older.iter_mut().find(|e| e.version == m.game_version) {
                Some(e) => e.mod_ids.push(m.id),
                None => older.push(VersionEntry {
                    id: None,
                    version: m.game_version.clone(),
                    build_id: m.game_build_id.clone(),
                    first_seen: Some(m.installed_at.clone()),
                    left_at: None,
                    current: false,
                    mod_ids: vec![m.id],
                    snapshot_count: 0,
                }),
            },
        }
    }
    // `mods` is in install order, so `older` already runs oldest first.
    older.extend(out);
    Ok(older)
}

fn entry(db: &Db, r: &GameVersionRow, current: bool) -> Result<VersionEntry> {
    Ok(VersionEntry {
        id: Some(r.id),
        version: r.version.clone(),
        build_id: r.build_id.clone(),
        first_seen: Some(r.first_seen.clone()),
        left_at: r.left_at.clone(),
        current,
        mod_ids: Vec::new(),
        snapshot_count: db.game_version_mods(r.id)?.len(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    /// Still installed at the same version.
    Same,
    /// Still installed, at another version.
    Updated,
    /// Still installed but turned off now (it was on then).
    Disabled,
    /// Turned on now (it was off then).
    Enabled,
    Removed,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModThen {
    #[serde(flatten)]
    pub then: SnapshotMod,
    pub change: Change,
    /// The installed mod it matches now.
    pub now_id: Option<i64>,
    pub now_version: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Loadout {
    pub version: Option<String>,
    pub left_at: Option<String>,
    pub mods: Vec<ModThen>,
    /// Installed now but not then.
    pub added: Vec<i64>,
}

/// Same mod across a game update: same Nexus mod, same source listing, or
/// (for manual installs) same name.
fn same_mod(s: &SnapshotMod, m: &ModRow) -> bool {
    match (s.nexus_mod_id, s.source_ref.as_deref()) {
        (Some(id), _) => m.nexus_mod_id == Some(id),
        (None, Some(r)) => m.source == s.source && m.source_ref.as_deref() == Some(r),
        _ => m.nexus_mod_id.is_none() && m.source_ref.is_none() && s.name.eq_ignore_ascii_case(&m.name),
    }
}

/// The mods that were installed when the game left version `id`, compared
/// with what's installed now.
pub fn loadout(db: &Db, game_id: i64, id: i64) -> Result<Loadout> {
    let row = db
        .game_versions(game_id)?
        .into_iter()
        .find(|r| r.id == id)
        .ok_or_else(|| crate::Error::Other("that game version isn't in the history".into()))?;
    Ok(compare(row.version, row.left_at, db.game_version_mods(id)?, &db.mods(game_id)?))
}

pub fn compare(version: Option<String>, left_at: Option<String>, then: Vec<SnapshotMod>, now: &[ModRow]) -> Loadout {
    let mut used = vec![false; now.len()];
    let mut mods = Vec::new();
    for s in then {
        // An exact version match first, so two files of one Nexus mod pair up.
        let pick = (0..now.len())
            .find(|&i| !used[i] && same_mod(&s, &now[i]) && now[i].version == s.version && now[i].archive_name == s.archive_name)
            .or_else(|| (0..now.len()).find(|&i| !used[i] && same_mod(&s, &now[i]) && now[i].version == s.version))
            .or_else(|| (0..now.len()).find(|&i| !used[i] && same_mod(&s, &now[i])));
        let (change, now_id, now_version) = match pick {
            None => (Change::Removed, None, None),
            Some(i) => {
                used[i] = true;
                let m = &now[i];
                let change = if m.version != s.version || m.archive_name != s.archive_name {
                    Change::Updated
                } else if s.enabled && !m.enabled() {
                    Change::Disabled
                } else if !s.enabled && m.enabled() {
                    Change::Enabled
                } else {
                    Change::Same
                };
                (change, Some(m.id), m.version.clone())
            }
        };
        mods.push(ModThen { then: s, change, now_id, now_version });
    }
    let added = now.iter().zip(&used).filter(|(_, u)| !**u).map(|(m, _)| m.id).collect();
    Loadout { version, left_at, mods, added }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::NewMod;
    use crate::game::{GameInstall, Store};

    fn install(version: &str, build: &str) -> GameInstall {
        GameInstall {
            path: "/g".into(),
            store: Store::Steam,
            proton_prefix: None,
            build_id: Some(build.into()),
            exe_file_version: None,
            exe_product_version: Some(version.into()),
            frameworks: vec![],
            launch_options: None,
            warnings: vec![],
        }
    }

    fn add(db: &Db, game_id: i64, name: &str, nexus: Option<i64>, version: &str) -> i64 {
        let g = db.game(game_id).unwrap();
        db.insert_mod(
            &g,
            &NewMod { name: name.into(), source: if nexus.is_some() { "nexus" } else { "manual" }.into(), nexus_mod_id: nexus, version: Some(version.into()), archive_name: format!("{name}-{version}.zip"), ..Default::default() },
        )
        .unwrap()
    }

    #[test]
    fn records_updates_and_compares_loadouts() {
        let db = Db::open_in_memory().unwrap();
        let gid = db.upsert_game(&install("3.0.76", "100")).unwrap();
        let cet = add(&db, gid, "CET", Some(107), "1.31");
        let _axl = add(&db, gid, "ArchiveXL", Some(4198), "1.20");
        let lights = add(&db, gid, "Lights", None, "1");
        // Seeing the same version again changes nothing.
        db.upsert_game(&install("3.0.76", "100")).unwrap();
        assert_eq!(db.game_versions(gid).unwrap().len(), 1);

        // The game updates.
        db.upsert_game(&install("3.0.78", "200")).unwrap();
        let versions = db.game_versions(gid).unwrap();
        assert_eq!(versions.len(), 2);
        assert!(versions[0].left_at.is_some());
        assert_eq!(db.game_version_mods(versions[0].id).unwrap().len(), 3);

        // After the update: CET updated, ArchiveXL removed, Lights turned off, a new mod.
        db.delete_mod(cet).unwrap();
        let cet2 = add(&db, gid, "CET", Some(107), "1.32");
        db.delete_mod(_axl).unwrap();
        db.set_mod_status(lights, crate::db::STATUS_DISABLED).unwrap();
        let new = add(&db, gid, "New", Some(5), "1");

        let h = history(&db, gid).unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].mod_ids, vec![lights], "Lights was installed on the old version");
        assert_eq!(h[1].mod_ids, vec![cet2, new]);
        assert!(h[1].current);
        assert_eq!(h[0].snapshot_count, 3);

        let l = loadout(&db, gid, versions[0].id).unwrap();
        let change = |n: &str| l.mods.iter().find(|m| m.then.name == n).unwrap().change;
        assert_eq!(change("CET"), Change::Updated);
        assert_eq!(l.mods.iter().find(|m| m.then.name == "CET").unwrap().now_version.as_deref(), Some("1.32"));
        assert_eq!(change("ArchiveXL"), Change::Removed);
        assert_eq!(change("Lights"), Change::Disabled);
        assert_eq!(l.added, vec![new]);
    }

    #[test]
    fn a_missing_build_id_is_not_an_update() {
        let db = Db::open_in_memory().unwrap();
        let mut gi = install("3.0.76", "100");
        let gid = db.upsert_game(&gi).unwrap();
        gi.build_id = None;
        db.upsert_game(&gi).unwrap();
        assert_eq!(db.game_versions(gid).unwrap().len(), 1);
        // A Steam hotfix: same exe version, new build.
        db.upsert_game(&install("3.0.76", "101")).unwrap();
        assert_eq!(db.game_versions(gid).unwrap().len(), 2);
    }

    #[test]
    fn mods_from_before_the_history_get_their_own_entry() {
        let db = Db::open_in_memory().unwrap();
        let gid = db.upsert_game(&install("3.0.78", "200")).unwrap();
        let id = add(&db, gid, "Old", None, "1");
        db.conn.execute("UPDATE mods SET game_version = '3.0.70', game_build_id = '50' WHERE id = ?1", [id]).unwrap();
        let h = history(&db, gid).unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].id, None);
        assert_eq!(h[0].version.as_deref(), Some("3.0.70"));
        assert_eq!(h[0].mod_ids, vec![id]);
        assert!(h[1].current && h[1].mod_ids.is_empty());
    }
}
