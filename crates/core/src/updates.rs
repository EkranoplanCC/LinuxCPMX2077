//! Finding newer versions of installed mods, on Nexus and the other sources.

use std::collections::HashMap;

use serde::Serialize;

use crate::db::ModRow;
use crate::nexus;
use crate::nexus_browse::newer_file;
use crate::sources::{InstalledRef, Registry, SourceFile};

#[derive(Debug, Clone, Serialize)]
pub struct Update {
    /// The installed mod (`mods.id`).
    pub mod_id: i64,
    pub name: String,
    pub source: String,
    pub current: Option<String>,
    pub latest: String,
    /// Nexus: the file that replaces the installed one.
    pub nexus_mod_id: Option<i64>,
    pub nexus_file_id: Option<i64>,
    /// Other sources: the mod's id there and the replacement file, if one
    /// could be picked automatically.
    pub source_ref: Option<String>,
    pub file: Option<SourceFile>,
    /// Going from an installed pre-release back to the stable release.
    pub to_stable: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub updates: Vec<Update>,
    /// Mods that couldn't be checked, with the reason.
    pub errors: Vec<String>,
    pub checked: usize,
}

/// Check installed mods that came from a known source (the app reads them
/// from the library first so the database isn't locked during network calls).
/// Nexus is skipped when there's no client (no API key).
pub fn check(mods: &[ModRow], nexus: Option<&nexus::Client>, sources: &Registry) -> Report {
    let mut report = Report::default();
    // One file list per Nexus mod, even when several of its files are installed.
    let mut nexus_files = HashMap::new();
    for m in mods {
        let result = match m.source.as_str() {
            "manual" => continue,
            "nexus" => {
                let (Some(client), Some(nexus_mod), Some(file)) = (nexus, m.nexus_mod_id, m.nexus_file_id) else { continue };
                report.checked += 1;
                if let std::collections::hash_map::Entry::Vacant(e) = nexus_files.entry(nexus_mod) {
                    e.insert(client.files_with_updates(nexus_mod));
                }
                match &nexus_files[&nexus_mod] {
                    Ok((files, chain)) => Ok(newer_file(file, files, chain).map(|f| Update {
                        latest: f.version.clone().or(f.mod_version.clone()).unwrap_or_else(|| f.name.clone().unwrap_or_default()),
                        nexus_mod_id: Some(nexus_mod),
                        nexus_file_id: Some(f.file_id),
                        ..base(m)
                    })),
                    Err(e) => Err(e.to_string()),
                }
            }
            other => {
                let (Ok(src), Some(source_ref)) = (sources.get(other), m.source_ref.clone()) else { continue };
                report.checked += 1;
                let installed = InstalledRef {
                    source_ref: source_ref.clone(),
                    file_id: m.source_file.clone(),
                    file_name: Some(m.archive_name.clone()),
                    version: m.version.clone(),
                };
                src.check_update(&installed)
                    .map(|o| o.map(|o| Update { latest: o.version, source_ref: Some(source_ref), file: o.file, to_stable: o.to_stable, ..base(m) }))
                    .map_err(|e| e.to_string())
            }
        };
        match result {
            Ok(Some(u)) => report.updates.push(u),
            Ok(None) => {}
            Err(e) => report.errors.push(format!("{}: {e}", m.name)),
        }
    }
    report
}

fn base(m: &ModRow) -> Update {
    Update {
        mod_id: m.id,
        name: m.name.clone(),
        source: m.source.clone(),
        current: m.version.clone(),
        latest: String::new(),
        nexus_mod_id: None,
        nexus_file_id: None,
        source_ref: None,
        file: None,
        to_stable: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Result;
    use crate::db::{Db, NewMod};
    use crate::game::{GameInstall, Store};
    use crate::sources::{Details, Downloaded, ListingPage, ModSource, Progress, SourceInfo, SourceQuery, UpdateOffer};

    struct Fake;
    impl ModSource for Fake {
        fn info(&self) -> SourceInfo {
            SourceInfo { id: "fake", label: "Fake", description: "", search_hint: "", popularity_label: "" }
        }
        fn search(&self, _: &SourceQuery) -> Result<ListingPage> {
            unimplemented!()
        }
        fn parse_ref(&self, _: &str) -> Option<String> {
            None
        }
        fn details(&self, _: &str) -> Result<Details> {
            unimplemented!()
        }
        fn download(&self, _: &str, _: &str, _: &std::path::Path, _: Progress) -> Result<Downloaded> {
            unimplemented!()
        }
        fn check_update(&self, i: &InstalledRef) -> Result<Option<UpdateOffer>> {
            match i.source_ref.as_str() {
                "old" => Ok(Some(UpdateOffer { version: "2.0".into(), file: None, to_stable: false })),
                "broken" => Err(crate::Error::Other("rate limited".into())),
                _ => Ok(None),
            }
        }
    }

    #[test]
    fn collects_updates_and_errors_per_mod() {
        let db = Db::open_in_memory().unwrap();
        let gi = GameInstall {
            path: "/g".into(),
            store: Store::Manual,
            proton_prefix: None,
            build_id: None,
            exe_file_version: None,
            exe_product_version: None,
            frameworks: vec![],
            launch_options: None,
            warnings: vec![],
        };
        let game = db.game(db.upsert_game(&gi).unwrap()).unwrap();
        for (name, source, r) in [("A", "fake", "old"), ("B", "fake", "current"), ("C", "fake", "broken"), ("D", "manual", ""), ("E", "gone", "x")] {
            db.insert_mod(&game, &NewMod { name: name.into(), source: source.into(), source_ref: Some(r.into()), version: Some("1.0".into()), ..Default::default() })
                .unwrap();
        }
        let reg = Registry::with_sources(vec![Box::new(Fake)]);
        let r = check(&db.mods(game.id).unwrap(), None, &reg);
        assert_eq!(r.checked, 3, "manual mods and unknown sources are skipped");
        assert_eq!(r.updates.len(), 1);
        assert_eq!(r.updates[0].name, "A");
        assert_eq!(r.updates[0].current.as_deref(), Some("1.0"));
        assert_eq!(r.updates[0].latest, "2.0");
        assert_eq!(r.errors, vec!["C: rate limited".to_string()]);
    }
}
