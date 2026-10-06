// Thin Tauri shell over cp2077mm-core. Every command that touches the disk or
// network runs on a blocking thread so the UI stays responsive.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use cp2077mm_core::archive::Limits;
use cp2077mm_core::db::{Db, DownloadRow, ModRow, NewMod};
use cp2077mm_core::game::{self, GameInstall};
use cp2077mm_core::fomod;
use cp2077mm_core::install::{EnableReport, FomodInfo, InstallOptions, InstallReport, Installer, Prepared, VerifyReport, resolve_ci};
use cp2077mm_core::nexus::{self, NxmLink};
use cp2077mm_core::nexus_browse::{self, Category, Collection, List, ModDetails, NexusRef, Page, Search};
use cp2077mm_core::sources::{self, Details, ListingPage, SourceInfo, SourceQuery};
use cp2077mm_core::linux_setup::{self, Check};
use cp2077mm_core::{Error, Result, desktop, paths, secrets, sso, updates};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_deep_link::DeepLinkExt;

struct AppState {
    db: Mutex<Db>,
    /// Archives extracted and waiting for the user's FOMOD choices.
    pending: Mutex<HashMap<String, PendingInstall>>,
    sso_cancel: Mutex<Option<Arc<AtomicBool>>>,
}

#[derive(Serialize)]
struct DetectedGame {
    id: i64,
    install: GameInstall,
}

#[derive(Serialize, Clone)]
struct Progress {
    mod_id: i64,
    file_id: i64,
    done: u64,
    total: u64,
}

#[derive(Serialize)]
struct NexusStatus {
    user: Option<nexus::User>,
    storage: Option<secrets::Backend>,
    error: Option<String>,
}

#[derive(Serialize)]
struct NexusMod {
    info: nexus::ModInfo,
    files: Vec<nexus::FileInfo>,
}

#[derive(Serialize)]
struct DownloadResult {
    download: nexus::Downloaded,
    /// The row in the Downloads tab, for installing it later.
    download_id: i64,
    install: Option<InstallOutcome>,
}

struct PendingInstall {
    game_id: i64,
    prepared: Prepared,
    meta: NewMod,
    /// An update: the installed mod this replaces.
    replaces: Option<i64>,
}

#[derive(Serialize)]
struct SourceDownloadResult {
    download: sources::Downloaded,
    download_id: i64,
    install: Option<InstallOutcome>,
}

/// Where the download queue is, for the Nexus window's title and menu.
#[derive(serde::Deserialize)]
struct QueueStep {
    position: u32,
    total: u32,
    name: String,
}

/// Either the mod is installed, or it has a FOMOD installer and the UI must
/// collect choices and call `finish_install`.
#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum InstallOutcome {
    Installed { report: InstallReport },
    NeedsChoices { token: String, name: String, fomod: FomodInfo },
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| Error::Other(format!("task failed: {e}")))?
}

fn with_installer<T>(db: &Db, f: impl FnOnce(&Installer) -> Result<T>) -> Result<T> {
    let inst = Installer {
        db,
        staging_root: paths::staging_dir()?,
        backups_root: paths::backups_dir()?,
        limits: Limits::default(),
    };
    f(&inst)
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| Error::Other("HOME is not set".into()))
}

/// Quota tracking and the browse cache live as long as the app.
static NEXUS: LazyLock<Arc<nexus::Shared>> = LazyLock::new(nexus::Shared::new);
/// GitHub and any other non-Nexus sources (with their caches).
static SOURCES: LazyLock<sources::Registry> = LazyLock::new(|| sources::Registry::new().expect("HTTP client"));

/// The game version to record with a download.
fn game_version(db: &Db, game_id: Option<i64>) -> Option<String> {
    let g = match game_id {
        Some(id) => db.game(id).ok()?,
        None => db.games().ok()?.into_iter().next()?,
    };
    g.exe_product_version.or(g.exe_file_version)
}

/// Test servers for a debug build (`CPMX_NEXUS_API`, `CPMX_NEXUS_WEB`);
/// release builds always talk to Nexus.
fn debug_override(var: &str) -> Option<String> {
    if cfg!(debug_assertions) { std::env::var(var).ok() } else { None }
}

fn nexus_client() -> Result<nexus::Client> {
    let (key, _) = secrets::load_api_key()?.ok_or_else(|| Error::Nexus("no Nexus API key set".into()))?;
    nexus_client_for(&key)
}

fn nexus_client_for(key: &str) -> Result<nexus::Client> {
    match debug_override("CPMX_NEXUS_API") {
        Some(base) => nexus::Client::with_endpoints(key, &format!("{base}/v1"), &format!("{base}/v2/graphql"), NEXUS.clone()),
        None => nexus::Client::with_shared(key, NEXUS.clone()),
    }
}

#[tauri::command]
async fn detect_games(app: AppHandle) -> Result<Vec<DetectedGame>> {
    blocking(move || {
        let found = game::detect(&home()?);
        let state = app.state::<AppState>();
        let db = state.db.lock().unwrap();
        let mut out = Vec::new();
        for g in found {
            out.push(DetectedGame { id: db.upsert_game(&g)?, install: g });
        }
        // Also report manually added installs that still exist.
        for row in db.games()? {
            if out.iter().all(|d| d.id != row.id)
                && let Ok(g) = game::from_manual_path(&PathBuf::from(&row.path)) {
                    out.push(DetectedGame { id: row.id, install: g });
                }
        }
        Ok(out)
    })
    .await
}

#[tauri::command]
async fn add_game_path(app: AppHandle, path: String) -> Result<DetectedGame> {
    blocking(move || {
        let g = game::from_manual_path(&PathBuf::from(path))?;
        let state = app.state::<AppState>();
        let id = state.db.lock().unwrap().upsert_game(&g)?;
        Ok(DetectedGame { id, install: g })
    })
    .await
}

#[tauri::command]
fn list_mods(state: State<'_, AppState>, game_id: i64) -> Result<Vec<ModRow>> {
    state.db.lock().unwrap().mods(game_id)
}

#[tauri::command]
fn list_downloads(state: State<'_, AppState>) -> Result<Vec<DownloadRow>> {
    state.db.lock().unwrap().downloads()
}

#[tauri::command]
async fn install_archive(app: AppHandle, game_id: i64, path: String, name: Option<String>, overwrite: bool) -> Result<InstallOutcome> {
    blocking(move || {
        let path = PathBuf::from(path);
        let meta = NewMod { name: name.unwrap_or_default(), source: "manual".into(), ..Default::default() };
        begin_install(&app, game_id, &path, meta, overwrite, None)
    })
    .await
}

/// Install a file from the Downloads tab, keeping where it came from.
#[tauri::command]
async fn install_download(app: AppHandle, download_id: i64, game_id: i64, overwrite: bool, replaces: Option<i64>) -> Result<InstallOutcome> {
    blocking(move || {
        let d = {
            let state = app.state::<AppState>();
            let db = state.db.lock().unwrap();
            db.downloads()?.into_iter().find(|d| d.id == download_id).ok_or_else(|| Error::Other("download not found".into()))?
        };
        let source = if d.source.is_empty() { "nexus".to_string() } else { d.source.clone() };
        let meta = NewMod {
            name: d.mod_name.clone().unwrap_or_default(),
            version: d.version.clone(),
            source: if source == "nexus" && d.nexus_mod_id.is_none() { "manual".into() } else { source },
            nexus_mod_id: d.nexus_mod_id,
            nexus_file_id: d.nexus_file_id,
            archive_name: d.file_name.clone(),
            source_ref: d.source_ref.clone(),
            source_file: d.source_file.clone(),
            category: d.category.clone(),
            ..Default::default()
        };
        begin_install(&app, game_id, &PathBuf::from(&d.path), meta, overwrite, replaces)
    })
    .await
}

/// Extract an archive; install right away unless it has a FOMOD installer.
fn begin_install(
    app: &AppHandle,
    game_id: i64,
    path: &std::path::Path,
    mut meta: NewMod,
    overwrite: bool,
    replaces: Option<i64>,
) -> Result<InstallOutcome> {
    let state = app.state::<AppState>();
    let db = state.db.lock().unwrap();
    let game = db.game(game_id)?;
    // Locks are always taken pending → db, so the pending entry is added
    // only after the db lock is released.
    let (outcome, pending) = with_installer(&db, |i| {
        let prepared = i.prepare(&game, path)?;
        if meta.name.trim().is_empty() {
            meta.name = prepared
                .fomod
                .as_ref()
                .map(|f| f.installer.module_name.clone())
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| default_mod_name(path));
        }
        match prepared.fomod.clone() {
            Some(fomod) => {
                let token = prepared.id.clone();
                let name = meta.name.clone();
                Ok((InstallOutcome::NeedsChoices { token, name, fomod }, Some(PendingInstall { game_id, prepared, meta, replaces })))
            }
            None => {
                let opts = InstallOptions { meta, overwrite, fomod_choices: None };
                let r = match replaces {
                    Some(old) => i.finish_replacing(&game, &prepared, opts, old),
                    None => i.finish_auto(&game, &prepared, opts),
                };
                if r.is_err() {
                    i.discard(&prepared);
                }
                Ok((InstallOutcome::Installed { report: r? }, None))
            }
        }
    })?;
    drop(db);
    if let Some(p) = pending {
        state.pending.lock().unwrap().insert(p.prepared.id.clone(), p);
    }
    Ok(outcome)
}

/// Which steps are visible and which options are usable for these choices.
#[tauri::command]
fn fomod_evaluate(state: State<'_, AppState>, token: String, selections: fomod::Selections) -> Result<fomod::Evaluation> {
    let pending = state.pending.lock().unwrap();
    let p = pending.get(&token).ok_or_else(|| Error::Other("this install is no longer pending".into()))?;
    let fm = p.prepared.fomod.as_ref().ok_or_else(|| Error::Other("not a FOMOD install".into()))?;
    let game = state.db.lock().unwrap().game(p.game_id)?;
    let files = cp2077mm_core::install::game_files(std::path::Path::new(&game.path));
    Ok(fm.installer.evaluate(&selections, &files).0)
}

/// An installer image as a data: URL (the webview can't read staging files).
#[tauri::command]
fn fomod_image(state: State<'_, AppState>, token: String, path: String) -> Result<Option<String>> {
    use base64::Engine;
    let pending = state.pending.lock().unwrap();
    let p = pending.get(&token).ok_or_else(|| Error::Other("this install is no longer pending".into()))?;
    let Some(fm) = &p.prepared.fomod else { return Ok(None) };
    let rel = cp2077mm_core::archive::sanitize_entry_name(&path)?;
    let Some(rel) = rel else { return Ok(None) };
    let base = p.prepared.dir.join("files");
    let file = resolve_ci(&base, &format!("{}{}", fm.root, rel.to_string_lossy()));
    let mime = match file.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        _ => return Ok(None),
    };
    let Ok(meta) = std::fs::symlink_metadata(&file) else { return Ok(None) };
    if !meta.is_file() || meta.len() > 8 * 1024 * 1024 || !file.starts_with(&base) {
        return Ok(None);
    }
    let bytes = std::fs::read(&file)?;
    Ok(Some(format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))))
}

/// Install a pending FOMOD with the user's choices. A conflict keeps it
/// pending so the user can retry with overwriting allowed.
#[tauri::command]
async fn finish_install(app: AppHandle, token: String, selections: fomod::Selections, overwrite: bool) -> Result<InstallReport> {
    blocking(move || {
        let state = app.state::<AppState>();
        let mut pending = state.pending.lock().unwrap();
        let p = pending.get(&token).ok_or_else(|| Error::Other("this install is no longer pending".into()))?;
        let db = state.db.lock().unwrap();
        let game = db.game(p.game_id)?;
        let opts = InstallOptions { meta: p.meta.clone(), overwrite, fomod_choices: Some(selections) };
        let r = with_installer(&db, |i| match p.replaces {
            Some(old) => i.finish_replacing(&game, &p.prepared, opts, old),
            None => i.finish_auto(&game, &p.prepared, opts),
        });
        match &r {
            Err(Error::Conflict(_)) => {}
            Err(_) if p.prepared.dir.exists() => {} // e.g. invalid choices: let the user fix them
            _ => {
                pending.remove(&token);
            }
        }
        r
    })
    .await
}

#[tauri::command]
fn cancel_install(state: State<'_, AppState>, token: String) -> Result<()> {
    if let Some(p) = state.pending.lock().unwrap().remove(&token) {
        let db = state.db.lock().unwrap();
        with_installer(&db, |i| {
            i.discard(&p.prepared);
            Ok(())
        })?;
    }
    Ok(())
}

fn default_mod_name(path: &std::path::Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Unnamed mod".into())
}

/// Cross-check every installed mod for conflicts and missing frameworks.
#[tauri::command]
async fn analyze_game(app: AppHandle, game_id: i64) -> Result<cp2077mm_core::analysis::Report> {
    blocking(move || {
        let state = app.state::<AppState>();
        let db = state.db.lock().unwrap();
        let game = db.game(game_id)?;
        cp2077mm_core::analysis::report_for_game(&db, &paths::staging_dir()?, &game)
    })
    .await
}

/// Run `f` with the setup context for one game: its fresh install details,
/// what's running, and the folder fix records live in.
fn with_setup<T>(app: &AppHandle, game_id: i64, f: impl FnOnce(&linux_setup::Ctx) -> Result<T>) -> Result<T> {
    let row = app.state::<AppState>().db.lock().unwrap().game(game_id)?;
    let home = home()?;
    let path = PathBuf::from(&row.path);
    let install = game::detect(&home)
        .into_iter()
        .find(|g| g.path == path)
        .map_or_else(|| game::from_manual_path(&path), Ok)?;
    let ctx = linux_setup::Ctx {
        home: &home,
        game: &install,
        state_dir: paths::data_dir()?.join("setup").join(game_id.to_string()),
        probe: linux_setup::Probe::system(),
    };
    f(&ctx)
}

/// Linux setup the game needs for mods (runtime, DLL overrides, -modded,
/// folder spellings), each with a fix and an undo.
#[tauri::command]
async fn setup_checks(app: AppHandle, game_id: i64) -> Result<Vec<Check>> {
    blocking(move || with_setup(&app, game_id, |ctx| Ok(linux_setup::checks(ctx)))).await
}

#[tauri::command]
async fn setup_fix(app: AppHandle, game_id: i64, id: String) -> Result<String> {
    blocking(move || with_setup(&app, game_id, |ctx| linux_setup::apply(ctx, &id))).await
}

#[tauri::command]
async fn setup_undo(app: AppHandle, game_id: i64, id: String) -> Result<String> {
    blocking(move || with_setup(&app, game_id, |ctx| linux_setup::undo(ctx, &id))).await
}

/// The end of one log from the Crashes & logs list, by its display name.
#[tauri::command]
async fn read_log(app: AppHandle, game_id: i64, name: String) -> Result<String> {
    blocking(move || {
        let game = app.state::<AppState>().db.lock().unwrap().game(game_id)?;
        let log = cp2077mm_core::crash::find_log(std::path::Path::new(&game.path), &name)?;
        cp2077mm_core::crash::tail_lines(&log.path, 5000)
    })
    .await
}

/// Show the folder a listed log is in.
#[tauri::command]
async fn open_log_folder(app: AppHandle, game_id: i64, name: String) -> Result<()> {
    blocking(move || {
        let game = app.state::<AppState>().db.lock().unwrap().game(game_id)?;
        let log = cp2077mm_core::crash::find_log(std::path::Path::new(&game.path), &name)?;
        let dir = log.path.parent().ok_or_else(|| Error::Other("log has no folder".into()))?;
        desktop::open_folder(dir)
    })
    .await
}

/// Errors from crash reports and framework logs, matched to mods.
#[tauri::command]
async fn crash_analysis(app: AppHandle, game_id: i64) -> Result<cp2077mm_core::crash::CrashReport> {
    blocking(move || {
        let state = app.state::<AppState>();
        let db = state.db.lock().unwrap();
        let game = db.game(game_id)?;
        cp2077mm_core::crash::analyze(&db, &game)
    })
    .await
}

#[tauri::command]
async fn uninstall_mod(app: AppHandle, mod_id: i64) -> Result<()> {
    blocking(move || {
        let state = app.state::<AppState>();
        let db = state.db.lock().unwrap();
        with_installer(&db, |i| i.uninstall(mod_id))
    })
    .await
}

/// Returns files left in the game because they changed after install.
#[tauri::command]
async fn disable_mod(app: AppHandle, mod_id: i64) -> Result<Vec<String>> {
    blocking(move || {
        let state = app.state::<AppState>();
        let db = state.db.lock().unwrap();
        with_installer(&db, |i| i.disable(mod_id))
    })
    .await
}

#[tauri::command]
async fn enable_mod(app: AppHandle, mod_id: i64, overwrite: bool) -> Result<EnableReport> {
    blocking(move || {
        let state = app.state::<AppState>();
        let db = state.db.lock().unwrap();
        with_installer(&db, |i| i.enable(mod_id, overwrite))
    })
    .await
}

#[tauri::command]
async fn verify_mod(app: AppHandle, mod_id: i64) -> Result<VerifyReport> {
    blocking(move || {
        let state = app.state::<AppState>();
        let db = state.db.lock().unwrap();
        with_installer(&db, |i| i.verify(mod_id))
    })
    .await
}

#[tauri::command]
async fn nexus_status() -> Result<NexusStatus> {
    blocking(|| {
        let Some((key, storage)) = secrets::load_api_key()? else {
            return Ok(NexusStatus { user: None, storage: None, error: None });
        };
        match nexus_client_for(&key)?.validate() {
            Ok(u) => Ok(NexusStatus { user: Some(u), storage: Some(storage), error: None }),
            Err(e) => Ok(NexusStatus { user: None, storage: Some(storage), error: Some(e.to_string()) }),
        }
    })
    .await
}

/// Validate the key with Nexus before storing it.
#[tauri::command]
async fn nexus_set_key(key: String) -> Result<NexusStatus> {
    blocking(move || {
        let user = nexus_client_for(&key)?.validate()?;
        let storage = secrets::store_api_key(&key)?;
        NEXUS.clear_cache();
        Ok(NexusStatus { user: Some(user), storage: Some(storage), error: None })
    })
    .await
}

const SSO_SLUG_SETTING: &str = "nexus_sso_slug";

/// The SSO application slug: user setting first, then the one built in.
fn sso_slug(db: &Db) -> Result<Option<String>> {
    let custom = db.get_setting(SSO_SLUG_SETTING)?.filter(|s| !s.trim().is_empty());
    Ok(custom.or(sso::BUILT_IN_SLUG.map(String::from)))
}

#[tauri::command]
fn nexus_sso_slug(state: State<'_, AppState>) -> Result<Option<String>> {
    sso_slug(&state.db.lock().unwrap())
}

#[tauri::command]
fn set_nexus_sso_slug(state: State<'_, AppState>, slug: String) -> Result<()> {
    state.db.lock().unwrap().set_setting(SSO_SLUG_SETTING, slug.trim())
}

/// Sign in through the browser; the resulting API key goes to the keyring.
#[tauri::command]
async fn nexus_sso_login(app: AppHandle) -> Result<NexusStatus> {
    blocking(move || {
        let state = app.state::<AppState>();
        let slug = sso_slug(&state.db.lock().unwrap())?
            .ok_or_else(|| Error::Nexus("Browser sign-in isn't configured: set the Nexus application slug in Settings, or paste an API key instead.".into()))?;
        let cancel = Arc::new(AtomicBool::new(false));
        if let Some(old) = state.sso_cancel.lock().unwrap().replace(cancel.clone()) {
            old.store(true, Ordering::Relaxed);
        }
        let key = sso::login(&slug, desktop::open_url, cancel);
        state.sso_cancel.lock().unwrap().take();
        let key = key?;
        let user = nexus_client_for(&key)?.validate()?;
        let storage = secrets::store_api_key(&key)?;
        NEXUS.clear_cache();
        Ok(NexusStatus { user: Some(user), storage: Some(storage), error: None })
    })
    .await
}

#[tauri::command]
fn nexus_sso_cancel(state: State<'_, AppState>) {
    if let Some(c) = state.sso_cancel.lock().unwrap().take() {
        c.store(true, Ordering::Relaxed);
    }
}

#[tauri::command]
fn nexus_clear_key() -> Result<()> {
    NEXUS.clear_cache();
    secrets::delete_api_key()
}

const SHOW_ADULT_SETTING: &str = "nexus_show_adult";

fn show_adult(app: &AppHandle) -> bool {
    let state = app.state::<AppState>();
    let db = state.db.lock().unwrap();
    db.get_setting(SHOW_ADULT_SETTING).ok().flatten().as_deref() == Some("1")
}

#[tauri::command]
fn nexus_show_adult(app: AppHandle) -> bool {
    show_adult(&app)
}

#[tauri::command]
fn set_nexus_show_adult(state: State<'_, AppState>, show: bool) -> Result<()> {
    state.db.lock().unwrap().set_setting(SHOW_ADULT_SETTING, if show { "1" } else { "0" })
}

/// Nexus' curated lists: trending, latest added, latest updated.
#[tauri::command]
async fn nexus_browse_list(app: AppHandle, list: List) -> Result<Page> {
    blocking(move || nexus_client()?.browse_list(list, show_adult(&app))).await
}

/// Nexus' mod categories, for the category filter.
#[tauri::command]
async fn nexus_categories() -> Result<Vec<Category>> {
    blocking(move || nexus_client()?.categories()).await
}

/// Name search and sorted lists (endorsements, downloads, dates), paged.
#[tauri::command]
async fn nexus_search(app: AppHandle, query: Search) -> Result<Page> {
    blocking(move || nexus_client()?.search(&query, show_adult(&app))).await
}

#[tauri::command]
async fn nexus_mod_details(mod_id: i64) -> Result<ModDetails> {
    blocking(move || nexus_client()?.mod_details(mod_id)).await
}

/// What a pasted Nexus link, id or collection nxm:// link points at.
#[tauri::command]
fn nexus_resolve(input: String) -> Option<NexusRef> {
    nexus_browse::parse_nexus_ref(&input)
}

/// A collection's mods, to queue them for install.
#[tauri::command]
async fn nexus_collection(slug: String, revision: Option<u32>) -> Result<Collection> {
    blocking(move || nexus_client()?.collection(&slug, revision)).await
}

/// A collection's page in the browser (built here, like `nexus_open_page`).
#[tauri::command]
async fn nexus_open_collection(slug: String) -> Result<()> {
    if !nexus_browse::is_collection_slug(&slug) {
        return Err(Error::Other(format!("`{slug}` is not a collection id")));
    }
    blocking(move || desktop::open_url(&nexus_browse::collection_page_url(&slug))).await
}

/// The API quota as Nexus last reported it.
#[tauri::command]
fn nexus_rate() -> nexus::RateLimit {
    NEXUS.rate()
}

/// The page for a mod, or the "Mod Manager Download" page for one file.
/// Built here so the UI can't open arbitrary links.
fn nexus_page(mod_id: i64, file_id: Option<i64>) -> Result<String> {
    if mod_id <= 0 || file_id.is_some_and(|f| f <= 0) {
        return Err(Error::Nexus("bad mod or file id".into()));
    }
    let url = match file_id {
        Some(f) => desktop::nexus_download_page(mod_id, f),
        None => nexus_browse::mod_page_url(mod_id, None),
    };
    Ok(match debug_override("CPMX_NEXUS_WEB") {
        Some(web) => url.replacen("https://www.nexusmods.com", &web, 1),
        None => url,
    })
}

/// Open a Nexus page in the user's browser. For a file, the download then
/// reaches the app through the system nxm:// handler.
#[tauri::command]
async fn nexus_open_page(mod_id: i64, file_id: Option<i64>) -> Result<()> {
    let url = nexus_page(mod_id, file_id)?;
    blocking(move || desktop::open_url(&url)).await
}

const NEXUS_WINDOW: &str = "nexus";

/// Show a Nexus page in a window of the app. The user signs in and starts
/// the download there like in a browser; the nxm:// link the page opens is
/// caught here and handed to the main window, so no system handler is
/// needed. The page runs untouched (no capability, so no access to the
/// app's commands); only nxm:// and non-web navigations are intercepted.
///
/// With `queue`, the window steps through the download queue: its title says
/// which mod is up, and its menu has Skip and Cancel (sent to the main window
/// as `queue-control`).
#[tauri::command]
async fn nexus_open_in_app(app: AppHandle, mod_id: i64, file_id: Option<i64>, queue: Option<QueueStep>) -> Result<()> {
    let url: tauri::Url = nexus_page(mod_id, file_id)?.parse().map_err(|e| Error::Other(format!("bad url: {e}")))?;
    if let Some(w) = app.get_webview_window(NEXUS_WINDOW) {
        w.navigate(url).map_err(|e| Error::Other(e.to_string()))?;
        queue_chrome(&app, &w, queue.as_ref())?;
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return Ok(());
    }
    let nav_app = app.clone();
    let popup_app = app.clone();
    let window = tauri::WebviewWindowBuilder::new(&app, NEXUS_WINDOW, tauri::WebviewUrl::External(url))
        .title(NEXUS_TITLE)
        .inner_size(1180.0, 860.0)
        .min_inner_size(640.0, 480.0)
        .on_navigation(move |u| match u.scheme() {
            "nxm" => {
                let app = nav_app.clone();
                let link = u.to_string();
                std::thread::spawn(move || forward_urls(&app, vec![link]));
                false
            }
            "https" | "http" | "about" | "blob" | "data" => true,
            _ => false,
        })
        .on_new_window(move |u, _| {
            let link = u.to_string();
            if u.scheme() == "nxm" {
                let app = popup_app.clone();
                std::thread::spawn(move || forward_urls(&app, vec![link]));
            } else if u.scheme() == "https" && is_nexus_host(u.host_str().unwrap_or_default()) {
                // Keep Nexus (including its sign-in) in this window.
                if let Some(w) = popup_app.get_webview_window(NEXUS_WINDOW) {
                    let _ = w.navigate(u);
                }
            } else {
                std::thread::spawn(move || {
                    if let Err(e) = desktop::open_link(&link) {
                        log::warn!("{e}");
                    }
                });
            }
            tauri::webview::NewWindowResponse::Deny
        })
        .build()
        .map_err(|e| Error::Other(format!("could not open the Nexus window: {e}")))?;
    let closed_app = app.clone();
    window.on_window_event(move |e| {
        if let tauri::WindowEvent::Destroyed = e {
            let _ = closed_app.emit_to("main", "nexus-window-closed", ());
        }
    });
    let menu_app = app.clone();
    window.on_menu_event(move |_, e| {
        let action = match e.id().as_ref() {
            "queue-skip" => "skip",
            "queue-cancel" => "cancel",
            _ => return,
        };
        let _ = menu_app.emit_to("main", "queue-control", action);
    });
    queue_chrome(&app, &window, queue.as_ref())
}

const NEXUS_TITLE: &str = "Nexus Mods · sign in, then start the download (CPMX2077 picks it up)";

fn queue_chrome(app: &AppHandle, w: &tauri::WebviewWindow, queue: Option<&QueueStep>) -> Result<()> {
    use tauri::menu::{Menu, MenuItem, Submenu};
    let err = |e: tauri::Error| Error::Other(e.to_string());
    let Some(q) = queue else {
        let _ = w.set_title(NEXUS_TITLE);
        let _ = w.remove_menu();
        return Ok(());
    };
    let step = format!("Mod {} of {}: {}", q.position, q.total, q.name);
    let _ = w.set_title(&format!("CPMX2077 queue · {step} · click “Slow download”"));
    // A dropdown: a click on a bare menu-bar item only highlights it in GTK.
    let queue_menu = Submenu::with_items(
        app,
        format!("Download queue · {step} ▾"),
        true,
        &[
            &MenuItem::with_id(app, "queue-skip", "Skip this mod", true, None::<&str>).map_err(err)?,
            &MenuItem::with_id(app, "queue-cancel", "Cancel the queue", true, None::<&str>).map_err(err)?,
        ],
    )
    .map_err(err)?;
    let menu = Menu::with_items(app, &[&queue_menu]).map_err(err)?;
    w.set_menu(menu).map_err(err)?;
    Ok(())
}

/// Close the Nexus window (the queue finished or was cancelled).
#[tauri::command]
fn nexus_window_close(app: AppHandle) {
    if let Some(w) = app.get_webview_window(NEXUS_WINDOW) {
        let _ = w.close();
    }
}

fn is_nexus_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == "nexusmods.com" || host.ends_with(".nexusmods.com")
}

/// Open a mod's page from another source in the browser (github.com only).
#[tauri::command]
async fn open_url(url: String) -> Result<()> {
    blocking(move || desktop::open_url(&url)).await
}

#[tauri::command]
async fn nexus_mod(mod_id: i64) -> Result<NexusMod> {
    blocking(move || {
        let c = nexus_client()?;
        Ok(NexusMod { info: c.mod_info(mod_id)?, files: c.mod_files(mod_id)? })
    })
    .await
}

/// Download a Nexus file (premium, or with key/expires from an nxm link) and
/// optionally install it into `game_id`.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn nexus_download(
    app: AppHandle,
    mod_id: i64,
    file_id: i64,
    key: Option<String>,
    expires: Option<i64>,
    install_to: Option<i64>,
    overwrite: Option<bool>,
    replaces: Option<i64>,
) -> Result<DownloadResult> {
    blocking(move || run_download(&app, mod_id, file_id, key, expires, install_to, overwrite.unwrap_or(false), replaces)).await
}

#[allow(clippy::too_many_arguments)]
fn run_download(
    app: &AppHandle,
    mod_id: i64,
    file_id: i64,
    key: Option<String>,
    expires: Option<i64>,
    install_to: Option<i64>,
    overwrite: bool,
    replaces: Option<i64>,
) -> Result<DownloadResult> {
    let c = nexus_client()?;
    let dl = c.download(mod_id, file_id, key.as_deref(), expires, &paths::downloads_dir()?, |done, total| {
        let _ = app.emit("download-progress", Progress { mod_id, file_id, done, total });
    })?;
    // Shown in the Downloads tab and kept for installing later. Best effort:
    // the download itself already succeeded.
    let info = c.mod_info(mod_id).ok();
    let file = c.file_info(mod_id, file_id).ok();
    let category = info.as_ref().and_then(|i| i.category_id).and_then(|id| {
        c.categories().ok()?.into_iter().find(|cat| cat.category_id == id).map(|cat| cat.name)
    });
    let name = info.as_ref().and_then(|i| i.name.clone());
    let version = file.as_ref().and_then(|f| f.version.clone().or(f.mod_version.clone()));
    let state = app.state::<AppState>();
    let download_id = {
        let db = state.db.lock().unwrap();
        db.insert_download(&DownloadRow {
            nexus_mod_id: Some(mod_id),
            nexus_file_id: Some(file_id),
            file_name: dl.file_name.clone(),
            path: dl.path.to_string_lossy().into_owned(),
            sha256: dl.sha256.clone(),
            md5: dl.md5.clone(),
            size: dl.size as i64,
            verified: dl.verified,
            source: "nexus".into(),
            mod_name: name.clone(),
            version: version.clone(),
            category: category.clone(),
            game_version: game_version(&db, install_to),
            checked: Some(if dl.verified { "MD5 matches Nexus" } else { "unverified: Nexus checksum lookup failed" }.into()),
            ..Default::default()
        })?
    };
    let install = match install_to {
        Some(game_id) => {
            let meta = NewMod {
                name: name.unwrap_or_else(|| dl.file_name.clone()),
                version,
                source: "nexus".into(),
                nexus_mod_id: Some(mod_id),
                nexus_file_id: Some(file_id),
                archive_name: dl.file_name.clone(),
                category,
                ..Default::default()
            };
            Some(begin_install(app, game_id, &dl.path, meta, overwrite, replaces)?)
        }
        None => None,
    };
    Ok(DownloadResult { download: dl, download_id, install })
}

#[derive(Serialize, Clone)]
struct SourceProgress {
    source: String,
    id: String,
    file_id: String,
    done: u64,
    total: u64,
}

#[derive(Serialize)]
struct SourceRef {
    source: &'static str,
    id: String,
}

#[tauri::command]
fn source_list() -> Vec<SourceInfo> {
    SOURCES.list()
}

#[tauri::command]
async fn source_featured(source: String) -> Result<Vec<sources::Listing>> {
    blocking(move || SOURCES.get(&source)?.featured()).await
}

#[derive(Serialize)]
struct PlannedFramework {
    source: &'static str,
    id: &'static str,
    name: &'static str,
}

/// GitHub frameworks to queue for `wanted`, each after what it needs; skips
/// requirements the game already has. An empty `wanted` means every featured
/// framework the game doesn't have yet.
#[tauri::command]
async fn framework_plan(app: AppHandle, game_id: i64, wanted: Vec<String>) -> Result<Vec<PlannedFramework>> {
    blocking(move || {
        let state = app.state::<AppState>();
        let row = state.db.lock().unwrap().game(game_id)?;
        let present: Vec<String> = game::detect_frameworks(std::path::Path::new(&row.path))
            .into_iter()
            .filter(|f| f.installed)
            .map(|f| f.id)
            .collect();
        let wanted = if wanted.is_empty() {
            let missing = sources::github::FEATURED.iter().filter(|f| !present.iter().any(|p| p == f.key));
            missing.map(|f| f.key.to_string()).collect()
        } else {
            wanted
        };
        Ok(sources::github::install_plan(&wanted, &present)?
            .into_iter()
            .map(|f| PlannedFramework { source: "github", id: f.repo, name: f.name })
            .collect())
    })
    .await
}

#[tauri::command]
async fn source_search(source: String, query: SourceQuery) -> Result<ListingPage> {
    blocking(move || SOURCES.get(&source)?.search(&query)).await
}

/// Which source a pasted link or id belongs to.
#[tauri::command]
fn source_resolve(input: String) -> Option<SourceRef> {
    SOURCES.list().into_iter().find_map(|info| {
        let id = SOURCES.get(info.id).ok()?.parse_ref(&input)?;
        Some(SourceRef { source: info.id, id })
    })
}

#[tauri::command]
async fn source_details(source: String, id: String) -> Result<Details> {
    blocking(move || SOURCES.get(&source)?.details(&id)).await
}

/// Download a file from a source, verified as far as it allows, and
/// optionally install it (replacing `replaces` for an update).
#[tauri::command]
async fn source_download(
    app: AppHandle,
    source: String,
    id: String,
    file_id: String,
    install_to: Option<i64>,
    overwrite: Option<bool>,
    replaces: Option<i64>,
) -> Result<SourceDownloadResult> {
    blocking(move || {
        let src = SOURCES.get(&source)?;
        let details = src.details(&id)?;
        let file = details
            .files
            .iter()
            .find(|f| f.id == file_id)
            .cloned()
            .ok_or_else(|| Error::Other("that file is no longer listed".into()))?;
        let mut progress = |done, total| {
            let _ = app.emit(
                "source-progress",
                SourceProgress { source: source.clone(), id: id.clone(), file_id: file_id.clone(), done, total },
            );
        };
        let dl = src.download(&id, &file_id, &paths::downloads_dir()?, &mut progress)?;
        let version = file.version.clone().or(details.listing.version.clone());
        let download_id = {
            let state = app.state::<AppState>();
            let db = state.db.lock().unwrap();
            db.insert_download(&DownloadRow {
                file_name: dl.file_name.clone(),
                path: dl.path.to_string_lossy().into_owned(),
                sha256: dl.sha256.clone(),
                md5: dl.md5.clone(),
                size: dl.size as i64,
                verified: dl.verified,
                source: source.clone(),
                source_ref: Some(id.clone()),
                source_file: Some(file_id.clone()),
                mod_name: Some(details.listing.name.clone()),
                version: version.clone(),
                category: details.listing.category.clone(),
                game_version: game_version(&db, install_to),
                checked: Some(dl.check.clone()),
                ..Default::default()
            })?
        };
        let install = match install_to {
            Some(game_id) => {
                let meta = NewMod {
                    name: details.listing.name.clone(),
                    version,
                    source: source.clone(),
                    archive_name: file.file_name.clone(),
                    category: details.listing.category.clone(),
                    source_ref: Some(id.clone()),
                    source_file: Some(file_id.clone()),
                    ..Default::default()
                };
                Some(begin_install(&app, game_id, &dl.path, meta, overwrite.unwrap_or(false), replaces)?)
            }
            None => None,
        };
        Ok(SourceDownloadResult { download: dl, download_id, install })
    })
    .await
}

/// Look for newer versions of the installed mods on Nexus and the other
/// sources. Nexus is skipped without an API key.
#[tauri::command]
async fn check_updates(app: AppHandle, game_id: i64) -> Result<updates::Report> {
    blocking(move || {
        let mods = app.state::<AppState>().db.lock().unwrap().mods(game_id)?;
        let nexus = nexus_client().ok();
        Ok(updates::check(&mods, nexus.as_ref(), &SOURCES))
    })
    .await
}

#[tauri::command]
fn parse_nxm(url: String) -> Result<NxmLink> {
    NxmLink::parse(&url)
}

/// Whether "Mod Manager Download" links in the browser reach this app.
#[tauri::command]
async fn nxm_status() -> Result<desktop::NxmStatus> {
    blocking(|| Ok(desktop::nxm_status())).await
}

/// Make this app the handler for "Mod Manager Download" links. Opt-in, since
/// it replaces whatever handled nxm:// before.
#[tauri::command]
async fn register_nxm_handler() -> Result<desktop::NxmStatus> {
    blocking(|| desktop::register_nxm(&home()?)).await
}

fn forward_urls(app: &AppHandle, urls: Vec<String>) {
    for u in urls.into_iter().filter(|u| u.starts_with("nxm://")) {
        let _ = app.emit("nxm-link", u);
    }
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.set_focus();
    }
}

/// How to launch the read-only agent server for this install.
#[tauri::command]
fn mcp_command() -> String {
    let exe = std::env::var("APPIMAGE")
        .ok()
        .or_else(|| std::env::current_exe().ok().map(|p| p.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "cp2077-modmanager".into());
    format!("claude mcp add cp2077-mods -- \"{exe}\" --mcp")
}

fn main() {
    // `--mcp`: serve the read-only agent API on stdin/stdout, no window.
    if std::env::args().skip(1).any(|a| a == "--mcp") {
        let result = cp2077mm_core::mcp::Server::open_default()
            .and_then(|s| s.serve(std::io::stdin().lock(), std::io::stdout().lock()));
        if let Err(e) = result {
            eprintln!("cp2077-modmanager --mcp: {e}");
            std::process::exit(1);
        }
        return;
    }
    let db = Db::open(&paths::db_path().expect("data dir")).expect("open library database");
    // Drop half-finished installs from a previous run.
    let _ = with_installer(&db, |i| {
        i.cleanup_incoming();
        Ok(())
    });
    tauri::Builder::default()
        // Must be first: a second launch (e.g. from an nxm:// click) hands its
        // arguments to the running window instead of opening another.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            forward_urls(app, argv);
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState { db: Mutex::new(db), pending: Mutex::new(HashMap::new()), sso_cancel: Mutex::new(None) })
        .setup(|app| {
            // An updated AppImage lives at a new path: keep nxm links working.
            if let Ok(h) = home() {
                let _ = desktop::refresh_nxm_entry(&h);
            }
            let handle = app.handle().clone();
            app.deep_link().on_open_url(move |event| {
                forward_urls(&handle, event.urls().iter().map(|u| u.to_string()).collect());
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            detect_games,
            add_game_path,
            setup_checks,
            setup_fix,
            setup_undo,
            list_mods,
            list_downloads,
            install_archive,
            install_download,
            fomod_evaluate,
            fomod_image,
            finish_install,
            cancel_install,
            uninstall_mod,
            disable_mod,
            enable_mod,
            analyze_game,
            crash_analysis,
            read_log,
            open_log_folder,
            mcp_command,
            verify_mod,
            nexus_status,
            nexus_set_key,
            nexus_clear_key,
            nexus_sso_slug,
            set_nexus_sso_slug,
            nexus_sso_login,
            nexus_sso_cancel,
            nexus_mod,
            nexus_download,
            nexus_show_adult,
            set_nexus_show_adult,
            nexus_browse_list,
            nexus_search,
            nexus_categories,
            nexus_mod_details,
            nexus_resolve,
            nexus_collection,
            nexus_open_collection,
            nexus_rate,
            nexus_open_page,
            nexus_open_in_app,
            nexus_window_close,
            open_url,
            parse_nxm,
            nxm_status,
            register_nxm_handler,
            source_list,
            source_featured,
            framework_plan,
            source_search,
            source_resolve,
            source_details,
            source_download,
            check_updates,
            startup_links,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// nxm:// links passed on the command line of the first launch.
#[tauri::command]
fn startup_links(app: AppHandle) -> Vec<String> {
    let mut v: Vec<String> = std::env::args().skip(1).filter(|a| a.starts_with("nxm://")).collect();
    if let Ok(Some(urls)) = app.deep_link().get_current() {
        v.extend(urls.into_iter().map(|u| u.to_string()));
    }
    v.dedup();
    v
}
