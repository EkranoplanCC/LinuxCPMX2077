// Thin Tauri shell over cp2077mm-core. Every command that touches the disk or
// network runs on a blocking thread so the UI stays responsive.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cp2077mm_core::archive::Limits;
use cp2077mm_core::db::{Db, DownloadRow, ModRow, NewMod};
use cp2077mm_core::game::{self, GameInstall};
use cp2077mm_core::install::{InstallOptions, InstallReport, Installer, VerifyReport};
use cp2077mm_core::nexus::{self, NxmLink};
use cp2077mm_core::{Error, Result, paths, secrets, sso};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_deep_link::DeepLinkExt;
use tauri_plugin_opener::OpenerExt;

struct AppState {
    db: Mutex<Db>,
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
    install: Option<InstallReport>,
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

fn nexus_client() -> Result<nexus::Client> {
    let (key, _) = secrets::load_api_key()?.ok_or_else(|| Error::Nexus("no Nexus API key set".into()))?;
    nexus::Client::new(&key)
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
async fn install_archive(app: AppHandle, game_id: i64, path: String, name: Option<String>, overwrite: bool) -> Result<InstallReport> {
    blocking(move || {
        let path = PathBuf::from(path);
        let name = name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| default_mod_name(&path));
        let state = app.state::<AppState>();
        let db = state.db.lock().unwrap();
        let game = db.game(game_id)?;
        with_installer(&db, |i| {
            i.install(
                &game,
                &path,
                InstallOptions { meta: NewMod { name, source: "manual".into(), ..Default::default() }, overwrite },
            )
        })
    })
    .await
}

fn default_mod_name(path: &std::path::Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Unnamed mod".into())
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
        match nexus::Client::new(&key)?.validate() {
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
        let user = nexus::Client::new(&key)?.validate()?;
        let storage = secrets::store_api_key(&key)?;
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
        let opener = app.clone();
        let key = sso::login(
            &slug,
            |url| opener.opener().open_url(url, None::<&str>).map_err(|e| Error::Other(format!("could not open browser: {e}"))),
            cancel,
        );
        state.sso_cancel.lock().unwrap().take();
        let key = key?;
        let user = nexus::Client::new(&key)?.validate()?;
        let storage = secrets::store_api_key(&key)?;
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
    secrets::delete_api_key()
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
async fn nexus_download(
    app: AppHandle,
    mod_id: i64,
    file_id: i64,
    key: Option<String>,
    expires: Option<i64>,
    install_to: Option<i64>,
    overwrite: Option<bool>,
) -> Result<DownloadResult> {
    blocking(move || run_download(&app, mod_id, file_id, key, expires, install_to, overwrite.unwrap_or(false))).await
}

fn run_download(
    app: &AppHandle,
    mod_id: i64,
    file_id: i64,
    key: Option<String>,
    expires: Option<i64>,
    install_to: Option<i64>,
    overwrite: bool,
) -> Result<DownloadResult> {
    let c = nexus_client()?;
    let dl = c.download(mod_id, file_id, key.as_deref(), expires, &paths::downloads_dir()?, |done, total| {
        let _ = app.emit("download-progress", Progress { mod_id, file_id, done, total });
    })?;
    let state = app.state::<AppState>();
    let db = state.db.lock().unwrap();
    db.insert_download(&DownloadRow {
        id: 0,
        nexus_mod_id: Some(mod_id),
        nexus_file_id: Some(file_id),
        file_name: dl.file_name.clone(),
        path: dl.path.to_string_lossy().into_owned(),
        sha256: dl.sha256.clone(),
        md5: dl.md5.clone(),
        size: dl.size as i64,
        verified: dl.verified,
        downloaded_at: String::new(),
    })?;
    let install = match install_to {
        Some(game_id) => {
            let info = c.mod_info(mod_id).ok();
            let file = c.file_info(mod_id, file_id).ok();
            let game = db.game(game_id)?;
            let meta = NewMod {
                name: info.as_ref().and_then(|i| i.name.clone()).unwrap_or_else(|| dl.file_name.clone()),
                version: file.as_ref().and_then(|f| f.version.clone().or(f.mod_version.clone())),
                source: "nexus".into(),
                nexus_mod_id: Some(mod_id),
                nexus_file_id: Some(file_id),
                archive_name: dl.file_name.clone(),
                ..Default::default()
            };
            Some(with_installer(&db, |i| i.install(&game, &dl.path, InstallOptions { meta, overwrite }))?)
        }
        None => None,
    };
    Ok(DownloadResult { download: dl, install })
}

#[tauri::command]
fn parse_nxm(url: String) -> Result<NxmLink> {
    NxmLink::parse(&url)
}

/// Make this app the handler for "Mod Manager Download" links. Opt-in, since
/// it replaces whatever handled nxm:// before.
#[tauri::command]
fn register_nxm_handler(app: AppHandle) -> Result<()> {
    app.deep_link().register_all().map_err(|e| Error::Other(e.to_string()))
}

fn forward_urls(app: &AppHandle, urls: Vec<String>) {
    for u in urls.into_iter().filter(|u| u.starts_with("nxm://")) {
        let _ = app.emit("nxm-link", u);
    }
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.set_focus();
    }
}

fn main() {
    let db = Db::open(&paths::db_path().expect("data dir")).expect("open library database");
    tauri::Builder::default()
        // Must be first: a second launch (e.g. from an nxm:// click) hands its
        // arguments to the running window instead of opening another.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            forward_urls(app, argv);
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(AppState { db: Mutex::new(db), sso_cancel: Mutex::new(None) })
        .setup(|app| {
            let handle = app.handle().clone();
            app.deep_link().on_open_url(move |event| {
                forward_urls(&handle, event.urls().iter().map(|u| u.to_string()).collect());
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            detect_games,
            add_game_path,
            list_mods,
            list_downloads,
            install_archive,
            uninstall_mod,
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
            parse_nxm,
            register_nxm_handler,
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
