//! Locating a Cyberpunk 2077 install (Steam/Proton first, GOG via Heroic, on
//! Windows also GOG Galaxy and Epic, or a path the user picks) and reading
//! what's in it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::vdf;
use crate::{Error, Result, STEAM_APP_ID};

pub const GAME_EXE: &str = "bin/x64/Cyberpunk2077.exe";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Store {
    Steam,
    Gog,
    Epic,
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameInstall {
    pub path: PathBuf,
    pub store: Store,
    /// Proton prefix (`compatdata/1091500/pfx`), or the Wine prefix Heroic
    /// uses for a GOG install; where saves and logs live.
    pub proton_prefix: Option<PathBuf>,
    /// Steam build id from the app manifest; changes with every patch.
    pub build_id: Option<String>,
    /// Version strings from the executable's PE resources.
    pub exe_file_version: Option<String>,
    pub exe_product_version: Option<String>,
    pub frameworks: Vec<Framework>,
    /// Steam launch options for the game, if we could read them.
    pub launch_options: Option<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Framework {
    pub id: String,
    pub name: String,
    pub installed: bool,
}

/// Steam roots in the places distros and Flatpak/Snap put them, and on
/// Windows where the registry says Steam is.
pub fn steam_roots(home: &Path) -> Vec<PathBuf> {
    let candidates = [
        ".steam/steam",
        ".steam/root",
        ".local/share/Steam",
        ".var/app/com.valvesoftware.Steam/.local/share/Steam",
        ".var/app/com.valvesoftware.Steam/data/Steam",
        "snap/steam/common/.local/share/Steam",
    ];
    let mut out: Vec<PathBuf> = Vec::new();
    for p in candidates.iter().map(|c| home.join(c)).chain(crate::winsys::steam_roots()) {
        if p.join("steamapps").is_dir() {
            let canon = p.canonicalize().unwrap_or(p);
            if !out.contains(&canon) {
                out.push(canon);
            }
        }
    }
    out
}

/// All Steam library folders listed in `libraryfolders.vdf` for every root.
pub fn steam_libraries(home: &Path) -> Vec<PathBuf> {
    let mut libs: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        let canon = p.canonicalize().unwrap_or(p);
        if canon.join("steamapps").is_dir() && !libs.contains(&canon) {
            libs.push(canon);
        }
    };
    for root in steam_roots(home) {
        push(root.clone());
        let vdf_path = root.join("steamapps/libraryfolders.vdf");
        let Ok(src) = std::fs::read_to_string(&vdf_path) else { continue };
        let parsed = vdf::parse(&src);
        if let Some(folders) = parsed.get("libraryfolders").and_then(|v| v.as_obj()) {
            for entry in folders.values() {
                if let Some(path) = entry.get("path").and_then(|v| v.as_str()) {
                    push(PathBuf::from(path));
                }
            }
        }
    }
    libs
}

/// Find every Cyberpunk 2077 install we can see.
pub fn detect(home: &Path) -> Vec<GameInstall> {
    let mut found = Vec::new();
    for lib in steam_libraries(home) {
        let manifest = lib.join(format!("steamapps/appmanifest_{STEAM_APP_ID}.acf"));
        let Ok(src) = std::fs::read_to_string(&manifest) else { continue };
        let parsed = vdf::parse(&src);
        let state = parsed.get("AppState");
        let installdir = state
            .and_then(|s| s.get("installdir"))
            .and_then(|v| v.as_str())
            .unwrap_or("Cyberpunk 2077");
        let path = lib.join("steamapps/common").join(installdir);
        if !path.join(GAME_EXE).is_file() {
            continue;
        }
        let build_id = state.and_then(|s| s.get("buildid")).and_then(|v| v.as_str()).map(String::from);
        let prefix = lib.join(format!("steamapps/compatdata/{STEAM_APP_ID}/pfx"));
        let mut g = inspect(&path, Store::Steam);
        g.build_id = build_id;
        g.proton_prefix = prefix.is_dir().then_some(prefix);
        g.launch_options = steam_launch_options(home);
        g.warnings = warnings(&g);
        found.push(g);
    }
    found.extend(detect_heroic_gog(home));
    for g in detect_windows_launchers() {
        if !found.iter().any(|f| same_dir(&f.path, &g.path)) {
            found.push(g);
        }
    }
    found
}

fn same_dir(a: &Path, b: &Path) -> bool {
    a == b || a.canonicalize().ok().is_some_and(|a| b.canonicalize().ok().is_some_and(|b| a == b))
}

/// GOG Galaxy and Epic installs on Windows (none elsewhere).
fn detect_windows_launchers() -> Vec<GameInstall> {
    let gog = crate::winsys::gog_installs().into_iter().map(|p| (p, Store::Gog));
    let epic = crate::winsys::epic_manifests().into_iter().filter_map(|m| epic_install(&m)).map(|p| (p, Store::Epic));
    gog.chain(epic)
        .filter(|(p, _)| p.join(GAME_EXE).is_file())
        .map(|(p, store)| {
            let mut g = inspect(&p, store);
            g.warnings = warnings(&g);
            g
        })
        .collect()
}

/// The install folder an Epic launcher manifest points at, if it's this game.
pub fn epic_install(manifest: &Path) -> Option<PathBuf> {
    let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(manifest).ok()?).ok()?;
    let name = json.get("DisplayName").and_then(|v| v.as_str()).unwrap_or_default();
    if !name.to_ascii_lowercase().contains("cyberpunk 2077") {
        return None;
    }
    json.get("InstallLocation").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(PathBuf::from)
}

const HEROIC_CONFIG_DIRS: &[&str] = &[".config/heroic", ".var/app/com.heroicgameslauncher.hgl/config/heroic"];

/// GOG installs managed by Heroic Games Launcher.
fn detect_heroic_gog(home: &Path) -> Vec<GameInstall> {
    let mut out = Vec::new();
    for (path, h) in heroic_installs(home) {
        let mut g = inspect(&path, Store::Gog);
        g.proton_prefix = h.prefix;
        g.warnings = warnings(&g);
        out.push(g);
    }
    out
}

/// How Heroic runs one game: its Wine prefix and the Wine (or Proton) build.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HeroicGame {
    pub app_name: String,
    /// The prefix itself (the folder holding `drive_c`).
    pub prefix: Option<PathBuf>,
    /// A `wine` binary that runs in that prefix.
    pub wine: Option<PathBuf>,
}

fn heroic_installs(home: &Path) -> Vec<(PathBuf, HeroicGame)> {
    let mut out = Vec::new();
    for dir in HEROIC_CONFIG_DIRS {
        let dir = home.join(dir);
        let Ok(src) = std::fs::read_to_string(dir.join("gog_store/installed.json")) else { continue };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&src) else { continue };
        let Some(list) = json.get("installed").and_then(|v| v.as_array()) else { continue };
        for item in list {
            let Some(p) = item.get("install_path").and_then(|v| v.as_str()) else { continue };
            let path = PathBuf::from(p);
            if !path.join(GAME_EXE).is_file() {
                continue;
            }
            let app_name = item.get("appName").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let h = heroic_game_config(&dir, &app_name).unwrap_or(HeroicGame { app_name, ..Default::default() });
            out.push((path, h));
        }
    }
    out
}

/// Heroic's per-game settings (`GamesConfig/<appName>.json`).
fn heroic_game_config(dir: &Path, app_name: &str) -> Option<HeroicGame> {
    if app_name.is_empty() || app_name.contains(['/', '\\']) {
        return None;
    }
    let src = std::fs::read_to_string(dir.join("GamesConfig").join(format!("{app_name}.json"))).ok()?;
    let json: serde_json::Value = serde_json::from_str(&src).ok()?;
    let cfg = json.get(app_name)?;
    let base = cfg.get("winePrefix").and_then(|v| v.as_str()).map(PathBuf::from);
    // A Proton build keeps the real prefix in `pfx/`, like Steam's compatdata.
    let prefix = base.and_then(|b| [b.join("pfx"), b].into_iter().find(|p| p.join("drive_c").is_dir()));
    let wv = cfg.get("wineVersion");
    let bin = wv.and_then(|w| w.get("bin")).and_then(|v| v.as_str()).map(PathBuf::from);
    let kind = wv.and_then(|w| w.get("type")).and_then(|v| v.as_str()).unwrap_or("wine");
    let wine = bin.and_then(|b| if kind == "proton" { proton_wine(b.parent()?) } else { Some(b) });
    Some(HeroicGame { app_name: app_name.to_string(), prefix, wine })
}

/// Heroic's view of the GOG install at `game`, if Heroic manages it.
pub fn heroic_game(home: &Path, game: &Path) -> Option<HeroicGame> {
    heroic_installs(home).into_iter().find(|(p, _)| p == game).map(|(_, h)| h)
}

/// The `wine` binary inside a Proton build (`files/` since Proton 5.13,
/// `dist/` before).
pub fn proton_wine(proton_dir: &Path) -> Option<PathBuf> {
    ["files/bin/wine", "dist/bin/wine"].iter().map(|r| proton_dir.join(r)).find(|p| p.is_file())
}

/// Validate and inspect a path the user picked by hand.
pub fn from_manual_path(path: &Path) -> Result<GameInstall> {
    if !path.join(GAME_EXE).is_file() {
        return Err(Error::GameNotFound(format!(
            "{} does not contain {GAME_EXE}",
            path.display()
        )));
    }
    let mut g = inspect(path, Store::Manual);
    g.warnings = warnings(&g);
    Ok(g)
}

fn inspect(path: &Path, store: Store) -> GameInstall {
    let (file_v, product_v) = exe_versions(&path.join(GAME_EXE)).unwrap_or((None, None));
    GameInstall {
        path: path.to_path_buf(),
        store,
        proton_prefix: None,
        build_id: None,
        exe_file_version: file_v,
        exe_product_version: product_v,
        frameworks: detect_frameworks(path),
        launch_options: None,
        warnings: Vec::new(),
    }
}

/// Read FileVersion / ProductVersion from the PE version resource.
pub fn exe_versions(exe: &Path) -> Result<(Option<String>, Option<String>)> {
    use pelite::pe64::{Pe, PeFile};
    let map = pelite::FileMap::open(exe)?;
    let file = PeFile::from_bytes(&map).map_err(|e| Error::Other(format!("PE parse: {e}")))?;
    let res = file.resources().map_err(|e| Error::Other(format!("PE resources: {e}")))?;
    let vi = res.version_info().map_err(|e| Error::Other(format!("PE version: {e}")))?;
    let fixed = vi.fixed().map(|f| {
        let v = f.dwFileVersion;
        format!("{}.{}.{}.{}", v.Major, v.Minor, v.Patch, v.Build)
    });
    let product = vi
        .translation()
        .first()
        .and_then(|lang| vi.value(*lang, "ProductVersion"))
        .map(|s| s.trim().to_string());
    Ok((fixed, product))
}

/// Modding frameworks most Cyberpunk mods depend on, detected by marker files.
pub fn detect_frameworks(game: &Path) -> Vec<Framework> {
    let table: &[(&str, &str, &[&str])] = &[
        ("cet", "Cyber Engine Tweaks", &["bin/x64/plugins/cyber_engine_tweaks.asi"]),
        ("red4ext", "RED4ext", &["red4ext/RED4ext.dll"]),
        ("redscript", "redscript", &["engine/tools/scc.exe", "engine/tools/scc_lib.dll"]),
        ("archivexl", "ArchiveXL", &["red4ext/plugins/ArchiveXL/ArchiveXL.dll"]),
        ("tweakxl", "TweakXL", &["red4ext/plugins/TweakXL/TweakXL.dll"]),
        ("codeware", "Codeware", &["red4ext/plugins/Codeware/Codeware.dll"]),
        ("redmod", "REDmod", &["tools/redmod/bin/redMod.exe"]),
    ];
    table
        .iter()
        .map(|(id, name, markers)| Framework {
            id: id.to_string(),
            name: name.to_string(),
            installed: markers.iter().any(|m| exists_ci(game, m)),
        })
        .collect()
}

/// Path lookup that tolerates case differences, since Windows mods don't care
/// about case and Linux filesystems do.
pub fn exists_ci(base: &Path, rel: &str) -> bool {
    if base.join(rel).exists() {
        return true;
    }
    let mut cur = base.to_path_buf();
    for part in rel.split('/') {
        let Ok(rd) = std::fs::read_dir(&cur) else { return false };
        let hit = rd
            .flatten()
            .find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(part));
        match hit {
            Some(e) => cur = e.path(),
            None => return false,
        }
    }
    true
}

/// Launch options from Steam's per-user `localconfig.vdf`.
pub fn steam_launch_options(home: &Path) -> Option<String> {
    for root in steam_roots(home) {
        let Ok(users) = std::fs::read_dir(root.join("userdata")) else { continue };
        for user in users.flatten() {
            let cfg = user.path().join("config/localconfig.vdf");
            let Ok(src) = std::fs::read_to_string(&cfg) else { continue };
            let v = vdf::parse(&src);
            let opts = v
                .get("UserLocalConfigStore")
                .and_then(|v| v.get("Software"))
                .and_then(|v| v.get("Valve"))
                .and_then(|v| v.get("Steam"))
                .and_then(|v| v.get("apps"))
                .and_then(|v| v.get(STEAM_APP_ID))
                .and_then(|v| v.get("LaunchOptions"))
                .and_then(|v| v.as_str());
            if let Some(o) = opts {
                return Some(o.to_string());
            }
        }
    }
    None
}

/// Any `mods/<name>/info.json` in the game folder.
pub fn has_redmods(game: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(game.join("mods")) else { return false };
    rd.flatten().any(|e| e.path().join("info.json").is_file())
}

/// Problems with the installed frameworks themselves. Prefix and launch
/// setup (runtimes, DLL overrides, -modded) are checked, and fixed, by
/// [`crate::linux_setup`].
fn warnings(g: &GameInstall) -> Vec<String> {
    let mut w = Vec::new();
    let has = |id: &str| g.frameworks.iter().any(|f| f.id == id && f.installed);
    for (dep, needs) in [("archivexl", "red4ext"), ("tweakxl", "red4ext"), ("codeware", "red4ext")] {
        if has(dep) && !has(needs) {
            w.push(format!("{dep} is installed but {needs} is missing; it will not load."));
        }
    }
    w
}

/// CET or RED4ext is installed, so the game needs the `winmm`/`version`
/// DLL overrides and a current Visual C++ runtime.
pub fn needs_overrides(g: &GameInstall) -> bool {
    g.frameworks.iter().any(|f| (f.id == "cet" || f.id == "red4ext") && f.installed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"x").unwrap();
    }

    #[test]
    fn detects_steam_install_in_secondary_library() {
        let home = tempfile::tempdir().unwrap();
        let lib2 = tempfile::tempdir().unwrap();
        let root = home.path().join(".local/share/Steam");
        std::fs::create_dir_all(root.join("steamapps")).unwrap();
        std::fs::write(
            root.join("steamapps/libraryfolders.vdf"),
            format!(
                "\"libraryfolders\" {{ \"0\" {{ \"path\" \"{}\" }} \"1\" {{ \"path\" \"{}\" }} }}",
                root.display(),
                lib2.path().display()
            ),
        )
        .unwrap();
        let steamapps = lib2.path().join("steamapps");
        std::fs::create_dir_all(steamapps.join("compatdata/1091500/pfx")).unwrap();
        std::fs::write(
            steamapps.join("appmanifest_1091500.acf"),
            "\"AppState\" { \"appid\" \"1091500\" \"installdir\" \"Cyberpunk 2077\" \"buildid\" \"19212345\" }",
        )
        .unwrap();
        let game = steamapps.join("common/Cyberpunk 2077");
        touch(&game.join(GAME_EXE));
        touch(&game.join("bin/x64/plugins/Cyber_Engine_Tweaks.asi"));

        let found = detect(home.path());
        assert_eq!(found.len(), 1);
        let g = &found[0];
        assert_eq!(g.store, Store::Steam);
        assert_eq!(g.build_id.as_deref(), Some("19212345"));
        assert!(g.proton_prefix.is_some());
        assert!(g.frameworks.iter().any(|f| f.id == "cet" && f.installed), "case-insensitive marker");
        assert!(needs_overrides(g));
    }

    #[test]
    fn reads_heroic_prefix_and_proton_wine() {
        let home = tempfile::tempdir().unwrap();
        let game = home.path().join("Games/Cyberpunk 2077");
        touch(&game.join(GAME_EXE));
        let cfg = home.path().join(".config/heroic");
        let pfx_base = home.path().join("Games/Heroic/Prefixes/Cyberpunk 2077");
        std::fs::create_dir_all(pfx_base.join("pfx/drive_c")).unwrap();
        let proton = home.path().join("Proton-GE");
        touch(&proton.join("files/bin/wine"));
        std::fs::create_dir_all(cfg.join("gog_store")).unwrap();
        std::fs::write(
            cfg.join("gog_store/installed.json"),
            serde_json::json!({"installed": [{"appName": "1423049311", "install_path": game}]}).to_string(),
        )
        .unwrap();
        std::fs::create_dir_all(cfg.join("GamesConfig")).unwrap();
        std::fs::write(
            cfg.join("GamesConfig/1423049311.json"),
            serde_json::json!({"1423049311": {"winePrefix": pfx_base, "wineVersion": {"bin": proton.join("proton"), "type": "proton"}}}).to_string(),
        )
        .unwrap();

        let found = detect(home.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].store, Store::Gog);
        assert_eq!(found[0].proton_prefix.as_deref(), Some(pfx_base.join("pfx").as_path()));
        let h = heroic_game(home.path(), &game).unwrap();
        assert_eq!(h.wine, Some(proton.join("files/bin/wine")));
    }

    #[test]
    fn reads_epic_manifests() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, json: serde_json::Value| {
            let p = dir.path().join(name);
            std::fs::write(&p, json.to_string()).unwrap();
            p
        };
        let game = write("a.item", serde_json::json!({"DisplayName": "Cyberpunk 2077", "InstallLocation": "C:\\Games\\Cyberpunk 2077"}));
        let other = write("b.item", serde_json::json!({"DisplayName": "Fortnite", "InstallLocation": "C:\\Games\\Fortnite"}));
        assert_eq!(epic_install(&game), Some(PathBuf::from("C:\\Games\\Cyberpunk 2077")));
        assert_eq!(epic_install(&other), None);
        assert_eq!(epic_install(&dir.path().join("missing.item")), None);
    }
}
