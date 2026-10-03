// Thin Tauri shell over cp2077mm-core. Every command that touches the disk or
// network runs on a blocking thread so the UI stays responsive.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cp2077mm_core::archive::Limits;
use cp2077mm_core::db::{Db, DownloadRow, ModRow, NewMod};
use cp2077mm_core::game::{self, GameInstall};
use cp2077mm_core::fomod;
use cp2077mm_core::install::{FomodInfo, InstallOptions, InstallReport, Installer, Prepared, VerifyReport, resolve_ci};
use cp2077mm_core::nexus::{self, NxmLink};
use cp2077mm_core::{Error, Result, paths, secrets, sso};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_deep_link::DeepLinkExt;
use tauri_plugin_opener::OpenerExt;

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
    install: Option<InstallOutcome>,
}

struct PendingInstall {
    game_id: i64,
    prepared: Prepared,
    meta: NewMod,
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
async fn install_archive(app: AppHandle, game_id: i64, path: String, name: Option<String>, overwrite: bool) -> Result<InstallOutcome> {
    blocking(move || {
        let path = PathBuf::from(path);
        let meta = NewMod { name: name.unwrap_or_default(), source: "manual".into(), ..Default::default() };
        begin_install(&app, game_id, &path, meta, overwrite)
    })
    .await
}

/// Extract an archive; install right away unless it has a FOMOD installer.
fn begin_install(app: &AppHandle, game_id: i64, path: &std::path::Path, mut meta: NewMod, overwrite: bool) -> Result<InstallOutcome> {
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
                Ok((InstallOutcome::NeedsChoices { token, name, fomod }, Some(PendingInstall { game_id, prepared, meta })))
            }
            None => {
                let r = i.finish(&game, &prepared, InstallOptions { meta, overwrite, fomod_choices: None });
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
        let r = with_installer(&db, |i| i.finish(&game, &p.prepared, opts));
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
    state.db.lock().unwrap().insert_download(&DownloadRow {
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
            let meta = NewMod {
                name: info.as_ref().and_then(|i| i.name.clone()).unwrap_or_else(|| dl.file_name.clone()),
                version: file.as_ref().and_then(|f| f.version.clone().or(f.mod_version.clone())),
                source: "nexus".into(),
                nexus_mod_id: Some(mod_id),
                nexus_file_id: Some(file_id),
                archive_name: dl.file_name.clone(),
                ..Default::default()
            };
            Some(begin_install(app, game_id, &dl.path, meta, overwrite)?)
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
        .plugin(tauri_plugin_opener::init())
        .manage(AppState { db: Mutex::new(db), pending: Mutex::new(HashMap::new()), sso_cancel: Mutex::new(None) })
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
            fomod_evaluate,
            fomod_image,
            finish_install,
            cancel_install,
            uninstall_mod,
            analyze_game,
            crash_analysis,
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
