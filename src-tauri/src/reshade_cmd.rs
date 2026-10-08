// Commands for the ReShade card in the Installed mods tab. The logic lives
// in cp2077mm-core's reshade module.

use std::path::{Path, PathBuf};

use cp2077mm_core::install::InstallReport;
use cp2077mm_core::linux_setup::{self, ReShadeLaunch};
use cp2077mm_core::reshade::{self, Status};
use cp2077mm_core::{Result, paths};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use super::{AppState, blocking, debug_override, with_installer, with_setup};

#[derive(Serialize, Clone)]
struct ReShadeProgress {
    done: u64,
    total: u64,
}

fn client() -> Result<reshade::Client> {
    match debug_override("CPMX_RESHADE_WEB") {
        Some(base) => reshade::Client::with_base(&base),
        None => reshade::Client::new(),
    }
}

/// Setups downloaded from reshade.me are kept here, checked again on reuse.
fn setup_dir() -> Result<PathBuf> {
    Ok(paths::data_dir()?.join("reshade"))
}

/// ReShade in this game: installed by the app, copied in by hand, and the
/// DLL name a new install would use.
#[tauri::command]
pub async fn reshade_status(app: AppHandle, game_id: i64) -> Result<Status> {
    blocking(move || {
        let state = app.state::<AppState>();
        let db = state.db.lock().unwrap();
        let game = db.game(game_id)?;
        reshade::status(&db, &game)
    })
    .await
}

/// What ReShade needs from Proton (launch options, shader compiler); `None`
/// on Windows or while ReShade isn't in the game folder.
#[tauri::command]
pub async fn reshade_launch(app: AppHandle, game_id: i64) -> Result<Option<ReShadeLaunch>> {
    blocking(move || with_setup(&app, game_id, |ctx| Ok(linux_setup::reshade_launch(ctx.game)))).await
}

/// The newest version on reshade.me.
#[tauri::command]
pub async fn reshade_latest() -> Result<String> {
    blocking(|| client()?.latest()).await
}

fn install_from(app: &AppHandle, game_id: i64, setup: &Path, dll: Option<String>) -> Result<InstallReport> {
    let state = app.state::<AppState>();
    let db = state.db.lock().unwrap();
    let game = db.game(game_id)?;
    with_installer(&db, |i| reshade::install(i, &game, setup, dll.as_deref()))
}

/// Download ReShade from reshade.me (the newest version unless one is
/// given), check it and install or update it.
#[tauri::command]
pub async fn reshade_install(app: AppHandle, game_id: i64, version: Option<String>, dll: Option<String>) -> Result<InstallReport> {
    blocking(move || {
        let c = client()?;
        let version = match version {
            Some(v) => v,
            None => c.latest()?,
        };
        let (path, _) = c.download(&version, &setup_dir()?, &mut |done, total| {
            let _ = app.emit("reshade-progress", ReShadeProgress { done, total });
        })?;
        install_from(&app, game_id, &path, dll)
    })
    .await
}

/// Install ReShade from a setup the user downloaded from reshade.me.
#[tauri::command]
pub async fn reshade_install_file(app: AppHandle, game_id: i64, path: String, dll: Option<String>) -> Result<InstallReport> {
    blocking(move || install_from(&app, game_id, Path::new(&path), dll)).await
}
