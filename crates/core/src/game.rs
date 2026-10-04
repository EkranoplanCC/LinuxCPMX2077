//! Locating a Cyberpunk 2077 install on Linux (Steam/Proton first, GOG via
//! Heroic, or a path the user picks) and reading what's in it.

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
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameInstall {
    pub path: PathBuf,
    pub store: Store,
    /// Proton prefix (`compatdata/1091500/pfx`), where saves and logs live.
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

/// Steam roots in the places distros and Flatpak/Snap put them.
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
    for c in candidates {
        let p = home.join(c);
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
    found
}

/// GOG installs managed by Heroic Games Launcher.
fn detect_heroic_gog(home: &Path) -> Vec<GameInstall> {
    let candidates = [
        ".config/heroic/gog_store/installed.json",
        ".var/app/com.heroicgameslauncher.hgl/config/heroic/gog_store/installed.json",
    ];
    let mut out = Vec::new();
    for c in candidates {
        let Ok(src) = std::fs::read_to_string(home.join(c)) else { continue };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&src) else { continue };
        let Some(list) = json.get("installed").and_then(|v| v.as_array()) else { continue };
        for item in list {
            let Some(p) = item.get("install_path").and_then(|v| v.as_str()) else { continue };
            let path = PathBuf::from(p);
            if path.join(GAME_EXE).is_file() {
                let mut g = inspect(&path, Store::Gog);
                g.warnings = warnings(&g);
                out.push(g);
            }
        }
    }
    out
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

/// Oldest `msvcp140.dll` (major, minor) that current CET and RED4ext builds
/// load with (Visual C++ 2022 17.10 changed std::mutex in a way older
/// runtimes crash on).
const MIN_VC_RUNTIME: (u16, u16) = (14, 40);

/// Version of the Visual C++ runtime installed in a Proton prefix, if any.
pub fn vc_runtime_version(prefix: &Path) -> Option<(u16, u16)> {
    let dll = prefix.join("drive_c/windows/system32/msvcp140.dll");
    let (file_v, _) = exe_versions(&dll).ok()?;
    let mut parts = file_v?.split('.').map(|p| p.parse::<u16>().ok()).collect::<Vec<_>>().into_iter();
    Some((parts.next()??, parts.next()??))
}

/// Proton-specific problems we can spot statically.
fn warnings(g: &GameInstall) -> Vec<String> {
    let mut w = Vec::new();
    let has = |id: &str| g.frameworks.iter().any(|f| f.id == id && f.installed);
    let needs_override = has("cet") || has("red4ext");
    if needs_override && g.store == Store::Steam {
        let opts = g.launch_options.clone().unwrap_or_default();
        let lower = opts.to_ascii_lowercase();
        if !(lower.contains("winedlloverrides") && lower.contains("winmm") && lower.contains("version")) {
            w.push(
                "Cyber Engine Tweaks / RED4ext need the Steam launch option \
                 WINEDLLOVERRIDES=\"winmm,version=n,b\" %command% under Proton."
                    .into(),
            );
        }
    }
    // CET and RED4ext are built with a recent MSVC; an older msvcp140.dll in
    // the prefix makes them fail at startup with error 998 (invalid memory
    // access).
    if needs_override
        && let Some(prefix) = &g.proton_prefix
        && let Some(v) = vc_runtime_version(prefix)
        && v < MIN_VC_RUNTIME
    {
        w.push(format!(
            "The Visual C++ runtime in the Proton prefix is {}.{}, older than CET and RED4ext need (error 998 at startup). \
             Close the game and run: protontricks {STEAM_APP_ID} vcrun2022",
            v.0, v.1
        ));
    }
    // REDmod mods only load (and get deployed) with the -modded flag.
    if g.store == Store::Steam && has_redmods(&g.path) {
        let opts = g.launch_options.clone().unwrap_or_default();
        if !opts.split_whitespace().any(|o| o.eq_ignore_ascii_case("-modded")) {
            w.push("REDmod mods are installed but the Steam launch options don't include -modded, so they won't load.".into());
        }
    }
    for (dep, needs) in [("archivexl", "red4ext"), ("tweakxl", "red4ext"), ("codeware", "red4ext")] {
        if has(dep) && !has(needs) {
            w.push(format!("{dep} is installed but {needs} is missing; it will not load."));
        }
    }
    w
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
        assert!(g.warnings.iter().any(|w| w.contains("WINEDLLOVERRIDES")));
        assert!(!g.warnings.iter().any(|w| w.contains("-modded")));

        assert!(!g.warnings.iter().any(|w| w.contains("vcrun2022")), "no runtime found, no guess");

        touch(&game.join("mods/SomeRedmod/info.json"));
        let g = &detect(home.path())[0];
        assert!(g.warnings.iter().any(|w| w.contains("-modded")));
    }
}
