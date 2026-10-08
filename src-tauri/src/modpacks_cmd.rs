// Commands for the Modpacks tab (Nexus collections the user follows, their
// own categories, mod list import/export) and the dependency view in the
// Installed mods tab. The logic lives in cp2077mm-core.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

use cp2077mm_core::analysis;
use cp2077mm_core::db::Db;
use cp2077mm_core::dependencies::{self, ModDependencies};
use cp2077mm_core::modpacks::{self, CollectionModStatus, CustomCategory, ImportReport, RevisionDiff, StatusCounts, TrackedCollection};
use cp2077mm_core::nexus_browse::{Collection, CollectionPage, CollectionSearch};
use cp2077mm_core::{Error, Result, paths};
use serde::Serialize;
use tauri::{AppHandle, Manager};

use super::{AppState, blocking, nexus_client, show_adult};

fn with_db<T>(app: &AppHandle, f: impl FnOnce(&Db) -> Result<T>) -> Result<T> {
    let state = app.state::<AppState>();
    let db = state.db.lock().unwrap();
    f(&db)
}

/// Search or list Nexus collections for the game.
#[tauri::command]
pub async fn nexus_collections(app: AppHandle, query: CollectionSearch) -> Result<CollectionPage> {
    blocking(move || nexus_client()?.search_collections(&query, show_adult(&app))).await
}

/// A followed collection with where each of its mods stands.
#[derive(Serialize)]
pub struct TrackedView {
    #[serde(flatten)]
    collection: TrackedCollection,
    mods: Vec<CollectionModStatus>,
    counts: StatusCounts,
    update_available: bool,
}

fn tracked_views(db: &Db, game_id: i64) -> Result<Vec<TrackedView>> {
    let installed = db.mods(game_id)?;
    Ok(modpacks::tracked(db, game_id)?
        .into_iter()
        .map(|c| {
            let mods = modpacks::status(&c.mods, &installed);
            TrackedView { counts: modpacks::counts(&mods), update_available: c.update_available(), mods, collection: c }
        })
        .collect())
}

#[tauri::command]
pub async fn tracked_collections(app: AppHandle, game_id: i64) -> Result<Vec<TrackedView>> {
    blocking(move || with_db(&app, |db| tracked_views(db, game_id))).await
}

/// Ask Nexus for the newest revision of every followed collection.
#[derive(Serialize)]
pub struct CollectionCheck {
    collections: Vec<TrackedView>,
    errors: Vec<String>,
}

#[tauri::command]
pub async fn check_collection_updates(app: AppHandle, game_id: i64) -> Result<CollectionCheck> {
    blocking(move || {
        let slugs: Vec<(String, String)> =
            with_db(&app, |db| Ok(modpacks::tracked(db, game_id)?.into_iter().map(|c| (c.slug, c.name)).collect()))?;
        let client = nexus_client()?;
        let mut errors = Vec::new();
        for (slug, name) in slugs {
            match client.collection(&slug, None) {
                Ok(c) => with_db(&app, |db| modpacks::set_latest_revision(db, game_id, &slug, c.revision))?,
                Err(e) => errors.push(format!("{name}: {e}")),
            }
        }
        Ok(CollectionCheck { collections: with_db(&app, |db| tracked_views(db, game_id))?, errors })
    })
    .await
}

/// A collection from Nexus with the state of each of its mods here and,
/// when the user follows it at another revision, what changed since.
#[derive(Serialize)]
pub struct CollectionView {
    collection: Collection,
    mods: Vec<CollectionModStatus>,
    counts: StatusCounts,
    /// The revision the user follows, if any.
    tracked_revision: Option<u32>,
    diff: Option<RevisionDiff>,
}

#[tauri::command]
pub async fn collection_view(app: AppHandle, game_id: i64, slug: String, revision: Option<u32>) -> Result<CollectionView> {
    blocking(move || {
        let c = nexus_client()?.collection(&slug, revision)?;
        with_db(&app, |db| {
            let tracked = modpacks::tracked_one(db, game_id, &c.slug)?;
            if let Some(t) = &tracked
                && revision.is_none()
            {
                modpacks::set_latest_revision(db, game_id, &t.slug, c.revision.max(t.latest_revision))?;
            }
            let mods = modpacks::status(&c.mods, &db.mods(game_id)?);
            let diff = tracked.as_ref().filter(|t| t.revision != c.revision).map(|t| modpacks::diff(&t.mods, &c.mods));
            Ok(CollectionView { counts: modpacks::counts(&mods), mods, tracked_revision: tracked.and_then(|t| t.revision), diff, collection: c })
        })
    })
    .await
}

/// Follow a collection at `revision` (the latest when none), or move a
/// followed one to it. Called when the user installs from it.
#[tauri::command]
pub async fn track_collection(app: AppHandle, game_id: i64, slug: String, revision: Option<u32>) -> Result<()> {
    blocking(move || {
        let c = nexus_client()?.collection(&slug, revision)?;
        with_db(&app, |db| modpacks::track(db, game_id, &c))
    })
    .await
}

/// Stop following a collection. Its mods stay installed.
#[tauri::command]
pub async fn untrack_collection(app: AppHandle, game_id: i64, slug: String) -> Result<()> {
    blocking(move || with_db(&app, |db| modpacks::untrack(db, game_id, &slug))).await
}

// ---- the user's own categories ---------------------------------------------

#[derive(Serialize)]
pub struct CustomCategories {
    categories: Vec<CustomCategory>,
    /// Installed mod id -> its category.
    mods: HashMap<i64, String>,
}

fn custom_categories_for(db: &Db, game_id: i64) -> Result<CustomCategories> {
    let by_key = modpacks::mod_categories(db)?;
    let mods = db.mods(game_id)?.into_iter().filter_map(|m| Some((m.id, by_key.get(&modpacks::mod_key(&m))?.clone()))).collect();
    Ok(CustomCategories { categories: modpacks::categories(db)?, mods })
}

#[tauri::command]
pub async fn custom_categories(app: AppHandle, game_id: i64) -> Result<CustomCategories> {
    blocking(move || with_db(&app, |db| custom_categories_for(db, game_id))).await
}

#[tauri::command]
pub async fn add_custom_category(app: AppHandle, name: String, color: Option<String>) -> Result<String> {
    blocking(move || with_db(&app, |db| modpacks::add_category(db, &name, color.as_deref()))).await
}

#[tauri::command]
pub async fn edit_custom_category(app: AppHandle, name: String, new_name: String, color: Option<String>) -> Result<()> {
    blocking(move || with_db(&app, |db| modpacks::edit_category(db, &name, &new_name, color.as_deref()))).await
}

#[tauri::command]
pub async fn delete_custom_category(app: AppHandle, name: String) -> Result<()> {
    blocking(move || with_db(&app, |db| modpacks::delete_category(db, &name))).await
}

#[tauri::command]
pub async fn set_mod_custom_category(app: AppHandle, mod_id: i64, category: Option<String>) -> Result<()> {
    blocking(move || {
        with_db(&app, |db| {
            let m = db.get_mod(mod_id)?;
            modpacks::set_mod_category(db, &modpacks::mod_key(&m), category.as_deref())
        })
    })
    .await
}

// ---- mod lists ---------------------------------------------------------------

/// Write the game's mod list, categories and followed collections to `path`.
#[tauri::command]
pub async fn export_modlist(app: AppHandle, game_id: i64, path: String) -> Result<usize> {
    blocking(move || {
        let list = with_db(&app, |db| modpacks::export(db, game_id))?;
        let mut path = PathBuf::from(path);
        if path.extension().is_none() {
            path.set_extension("json");
        }
        std::fs::write(&path, serde_json::to_vec_pretty(&list)?)?;
        cp2077mm_core::activity::record_path(cp2077mm_core::activity::Kind::Setup, "Exported the mod list to", &path);
        Ok(list.mods.len())
    })
    .await
}

/// Read a mod list from `path` and apply its categories. Mods it lists that
/// aren't installed are returned for the UI to offer, not downloaded.
#[tauri::command]
pub async fn import_modlist(app: AppHandle, game_id: i64, path: String) -> Result<ImportReport> {
    blocking(move || {
        let path = PathBuf::from(path);
        let size = std::fs::metadata(&path)?.len();
        if size > modpacks::MAX_MODLIST_BYTES as u64 {
            return Err(Error::Other("that file is too big to be a CPMX2077 mod list".into()));
        }
        let list = modpacks::parse_modlist(&std::fs::read(&path)?)?;
        with_db(&app, |db| modpacks::import(db, game_id, &list))
    })
    .await
}

// ---- dependencies --------------------------------------------------------------

#[derive(Serialize)]
pub struct DependencyView {
    mods: Vec<ModDependencies>,
    /// Why Nexus requirements may be missing or old, if they are.
    note: Option<String>,
}

/// What each installed mod needs. Nexus requirements are cached for a day;
/// `refresh` asks Nexus again for all of them.
#[tauri::command]
pub async fn mod_dependencies(app: AppHandle, game_id: i64, refresh: bool) -> Result<DependencyView> {
    blocking(move || {
        let staging = paths::staging_dir()?;
        let (mods, game_dir, mut cached, stale) = with_db(&app, |db| {
            let mods = db.mods(game_id)?;
            // Index mods the compatibility check hasn't seen yet, so their
            // framework needs are known.
            for m in &mods {
                if db.index_version(m.id)? != Some(analysis::SCANNER_VERSION) {
                    analysis::index_mod(db, &staging, m.id)?;
                }
            }
            let ids: Vec<i64> = mods.iter().filter_map(|m| m.nexus_mod_id).collect();
            let (cached, stale) = dependencies::cached_requirements(db, &ids)?;
            let stale = if refresh { ids } else { stale };
            Ok((mods, PathBuf::from(db.game(game_id)?.path), cached, stale))
        })?;
        let mut note = None;
        if !stale.is_empty() {
            match nexus_client().and_then(|c| c.requirements(&stale)) {
                Ok(fresh) => with_db(&app, |db| {
                    let answered: BTreeSet<i64> = fresh.iter().map(|(id, _)| *id).collect();
                    for (id, list) in &fresh {
                        dependencies::store_requirements(db, *id, list)?;
                        cached.insert(*id, list.clone());
                    }
                    // Mods Nexus no longer has: remember there's nothing to list.
                    for id in stale.iter().filter(|i| !answered.contains(i)) {
                        dependencies::store_requirements(db, *id, &[])?;
                    }
                    Ok(())
                })?,
                Err(e) => note = Some(format!("Couldn't ask Nexus what mods need ({e}); showing what was saved before.")),
            }
        }
        let detected = with_db(&app, |db| dependencies::detected_frameworks(db, &mods))?;
        let has = dependencies::game_has(&game_dir);
        Ok(DependencyView { mods: dependencies::resolve(&mods, &cached, &detected, &has), note })
    })
    .await
}
