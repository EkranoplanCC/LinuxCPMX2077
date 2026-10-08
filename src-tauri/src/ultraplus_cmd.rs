// Commands for the Ultra+ recommendations card in the Installed mods tab.
// The logic lives in cp2077mm-core's ultraplus module.

use std::path::Path;

use cp2077mm_core::Result;
use cp2077mm_core::ultraplus::{self, Report};
use tauri::{AppHandle, Manager};

use super::{AppState, blocking};

fn build(app: &AppHandle, game_id: i64) -> Result<Report> {
    let state = app.state::<AppState>();
    let db = state.db.lock().unwrap();
    let game = db.game(game_id)?;
    let mods = db.mods(game_id)?;
    Ok(ultraplus::report(Path::new(&game.path), &mods, &ultraplus::saved(&db)))
}

/// Ultra+'s recommendations and where each stands in this game.
#[tauri::command]
pub async fn ultraplus_report(app: AppHandle, game_id: i64) -> Result<Report> {
    blocking(move || build(&app, game_id)).await
}

/// Re-read the Ultra+ page, keep its lists, and report again.
#[tauri::command]
pub async fn ultraplus_refresh(app: AppHandle, game_id: i64) -> Result<Report> {
    blocking(move || {
        let guide = ultraplus::fetch()?;
        ultraplus::save(&app.state::<AppState>().db.lock().unwrap(), &guide)?;
        build(&app, game_id)
    })
    .await
}
