//! ReShade: the post-processing injector from reshade.me, installed as a
//! tracked mod so Disable, Uninstall, Verify and the File map work on it like
//! on any other mod.
//!
//! The setup program from reshade.me carries ReShade's DLLs in a zip archive
//! appended to the executable. Only `ReShade64.dll` is taken out of it, and
//! only after checking it's a 64-bit Windows DLL whose version resource names
//! ReShade and matches the version in the setup's file name. reshade.me
//! publishes no checksums, so that check plus HTTPS from reshade.me itself is
//! the verification; the SHA-256 of the setup and of the DLL are recorded.
//!
//! Cyberpunk 2077 renders with DirectX 12, so the DLL goes next to the game
//! exe as `dxgi.dll` (or `d3d12.dll` when another program already uses that
//! name). `ReShade.ini` is written once if the folder has none, and removed
//! again with ReShade; presets the user made stay.

use std::cmp::Ordering;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use sha2::Digest;

use crate::activity::{self, Kind};
use crate::db::{Db, GameRow, ModRow, NewMod};
use crate::install::{InstallOptions, InstallReport, Installer, Prepared, resolve_ci};
use crate::{APP_NAME, APP_VERSION, Error, Result};

pub const SITE: &str = "https://reshade.me";
/// `mods.source` of ReShade itself.
pub const SOURCE: &str = "reshade";
pub const MOD_NAME: &str = "ReShade";
/// Folder of the game exe, where ReShade and its files live.
pub const BIN: &str = "bin/x64";
/// Names ReShade loads under in a DirectX 12 game, in order of preference.
pub const DLL_NAMES: &[&str] = &["dxgi.dll", "d3d12.dll"];
pub const INI: &str = "ReShade.ini";
/// Files ReShade writes next to itself at runtime (besides presets).
const RUNTIME_FILES: &[&str] = &["ReShade.ini", "ReShade.log", "ReShade.log1", "dxgi.log", "d3d12.log"];
const MAX_PAGE: u64 = 2 << 20;
const MAX_SETUP: u64 = 64 << 20;
const MAX_DLL: u64 = 32 << 20;
/// Settings key (per game) recording that the app created `ReShade.ini`.
const INI_CREATED: &str = "reshade_ini_created";

/// The `ReShade.ini` the app writes when the game has none: effects and
/// textures from `reshade-shaders`, where shader packs and Nexus presets put
/// them.
pub const DEFAULT_INI: &str = "[GENERAL]\r\n\
EffectSearchPaths=.\\reshade-shaders\\Shaders\\**\r\n\
TextureSearchPaths=.\\reshade-shaders\\Textures\\**\r\n\
PresetPath=.\\ReShadePreset.ini\r\n";

/// Compare dotted version numbers ("6.3.10" > "6.3.9").
pub fn cmp_versions(a: &str, b: &str) -> Ordering {
    let parts = |s: &str| s.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    let (mut pa, mut pb) = (parts(a), parts(b));
    let n = pa.len().max(pb.len());
    pa.resize(n, 0);
    pb.resize(n, 0);
    pa.cmp(&pb)
}

fn is_version(v: &str) -> bool {
    !v.is_empty() && v.len() <= 20 && v.split('.').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// The version in a setup file name: `ReShade_Setup_6.3.0.exe` → `6.3.0`.
/// The add-on build (`…_Addon.exe`) is recognised too.
pub fn version_from_file_name(name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    let rest = lower.strip_prefix("reshade_setup_")?;
    let v = rest.strip_suffix(".exe")?;
    let v = v.strip_suffix("_addon").unwrap_or(v);
    is_version(v).then(|| v.to_string())
}

/// The newest standard (non add-on) setup linked from reshade.me's page.
pub fn latest_in_page(html: &str) -> Option<String> {
    const MARK: &str = "/downloads/ReShade_Setup_";
    let mut best: Option<String> = None;
    let mut rest = html;
    while let Some(i) = rest.find(MARK) {
        rest = &rest[i + MARK.len()..];
        let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len());
        let (v, after) = (&rest[..end], &rest[end..]);
        // "6.3.0.exe": the digits run includes the dot before "exe".
        let Some(v) = v.strip_suffix('.') else { continue };
        if after.starts_with("exe") && is_version(v) && best.as_deref().is_none_or(|b| cmp_versions(v, b) == Ordering::Greater) {
            best = Some(v.to_string());
        }
    }
    best
}

pub fn setup_file_name(version: &str) -> String {
    format!("ReShade_Setup_{version}.exe")
}

/// What a DLL's PE headers and version resource say.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DllInfo {
    /// File version, e.g. "6.3.0.1999".
    pub version: Option<String>,
    pub product: Option<String>,
}

/// Check `bytes` is a 64-bit Windows DLL whose version resource names
/// ReShade.
pub fn dll_info(bytes: &[u8]) -> Result<DllInfo> {
    use pelite::pe64::{Pe, PeFile};
    let bad = |why: &str| Error::Integrity(format!("ReShade64.dll {why}"));
    let file = PeFile::from_bytes(bytes).map_err(|_| bad("is not a 64-bit Windows DLL"))?;
    let fh = file.file_header();
    if fh.Machine != 0x8664 || fh.Characteristics & 0x2000 == 0 {
        return Err(bad("is not a 64-bit Windows DLL"));
    }
    let res = file.resources().map_err(|_| bad("has no version information"))?;
    let vi = res.version_info().map_err(|_| bad("has no version information"))?;
    let version = vi.fixed().map(|f| {
        let v = f.dwFileVersion;
        format!("{}.{}.{}.{}", v.Major, v.Minor, v.Patch, v.Build)
    });
    let lang = vi.translation().first().copied();
    let value = |key: &str| lang.and_then(|l| vi.value(l, key)).map(|s| s.trim().to_string());
    let product = value("ProductName");
    let names_reshade = [product.clone(), value("FileDescription")].iter().flatten().any(|s| s.to_ascii_lowercase().contains("reshade"));
    if !names_reshade {
        return Err(bad("does not identify as ReShade in its version information"));
    }
    Ok(DllInfo { version, product })
}

/// Whether the file at `path` is a ReShade DLL.
pub fn is_reshade_dll(path: &Path) -> bool {
    let Ok(f) = std::fs::File::open(path) else { return false };
    let mut bytes = Vec::new();
    f.take(MAX_DLL).read_to_end(&mut bytes).is_ok() && dll_info(&bytes).is_ok()
}

/// A checked ReShade setup and the DLL taken out of it.
#[derive(Debug, Clone, Serialize)]
pub struct Setup {
    pub file_name: String,
    /// Version from the file name, else from the DLL ("6.3.0").
    pub version: String,
    pub sha256: String,
    pub md5: String,
    pub dll_sha256: String,
    pub dll_version: Option<String>,
    #[serde(skip)]
    pub dll: Vec<u8>,
}

/// Read and check a ReShade setup program from reshade.me.
pub fn read_setup(path: &Path) -> Result<Setup> {
    let size = std::fs::metadata(path)?.len();
    if size > MAX_SETUP {
        return Err(Error::Integrity(format!("{} is larger than a ReShade setup ({})", path.display(), activity::size(size))));
    }
    let bytes = std::fs::read(path)?;
    let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !bytes.starts_with(b"MZ") {
        return Err(Error::Integrity(format!("{file_name} is not a Windows program; expected ReShade_Setup_<version>.exe from reshade.me")));
    }
    let not_setup = || Error::Integrity(format!("{file_name} contains no ReShade64.dll; expected ReShade_Setup_<version>.exe from reshade.me"));
    let mut zip = zip::ZipArchive::new(Cursor::new(&bytes[..])).map_err(|_| not_setup())?;
    let index = (0..zip.len())
        .find(|&i| zip.name_for_index(i).is_some_and(|n| n.rsplit(['/', '\\']).next().is_some_and(|f| f.eq_ignore_ascii_case("ReShade64.dll"))))
        .ok_or_else(not_setup)?;
    let entry = zip.by_index(index)?;
    if entry.size() > MAX_DLL {
        return Err(Error::Integrity("ReShade64.dll inside the setup is unexpectedly large".into()));
    }
    let mut dll = Vec::with_capacity(entry.size() as usize);
    entry.take(MAX_DLL + 1).read_to_end(&mut dll)?;
    if dll.len() as u64 > MAX_DLL {
        return Err(Error::Integrity("ReShade64.dll inside the setup is unexpectedly large".into()));
    }
    let info = dll_info(&dll)?;
    let named = version_from_file_name(&file_name);
    let version = match (&named, &info.version) {
        (Some(n), Some(d)) if !d.starts_with(&format!("{n}.")) && d != n => {
            return Err(Error::Integrity(format!("{file_name} contains ReShade {d}, not {n}")));
        }
        (Some(n), _) => n.clone(),
        (None, Some(d)) => d.split('.').take(3).collect::<Vec<_>>().join("."),
        (None, None) => return Err(Error::Integrity("ReShade64.dll has no version number".into())),
    };
    let sha256 = hex::encode(sha2::Sha256::digest(&bytes));
    let md5 = hex::encode(md5::Md5::digest(&bytes));
    let dll_sha256 = hex::encode(sha2::Sha256::digest(&dll));
    activity::record_path(
        Kind::Verify,
        format!(
            "ReShade setup {version}: SHA-256 {sha256}; ReShade64.dll {} is a 64-bit ReShade DLL, SHA-256 {dll_sha256} (reshade.me publishes no checksums)",
            info.version.as_deref().unwrap_or("?")
        ),
        path,
    );
    Ok(Setup { file_name, version, sha256, md5, dll_sha256, dll_version: info.version, dll })
}

/// reshade.me over HTTPS, without following redirects to other hosts.
pub struct Client {
    http: reqwest::blocking::Client,
    base: String,
}

impl Client {
    pub fn new() -> Result<Self> {
        Self::with_base(SITE)
    }

    pub fn with_base(base: &str) -> Result<Self> {
        let base = base.trim_end_matches('/').to_string();
        let host = url::Url::parse(&base).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_default();
        let policy = reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 5 {
                attempt.error("too many redirects")
            } else if attempt.url().host_str() == Some(host.as_str()) {
                attempt.follow()
            } else {
                let to = attempt.url().to_string();
                attempt.error(format!("redirect to another host ({to})"))
            }
        });
        let http = reqwest::blocking::Client::builder()
            .user_agent(format!("{APP_NAME}/{APP_VERSION}"))
            .https_only(base.starts_with("https://"))
            .redirect(policy)
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(120))
            .build()?;
        Ok(Self { http, base })
    }

    /// The newest ReShade version linked from reshade.me.
    pub fn latest(&self) -> Result<String> {
        let resp = self.http.get(format!("{}/", self.base)).send()?;
        let status = resp.status();
        activity::record(Kind::Api, format!("reshade.me GET / → {}", status.as_u16()));
        if !status.is_success() {
            return Err(Error::Other(format!("reshade.me answered {status}")));
        }
        let mut body = Vec::new();
        resp.take(MAX_PAGE).read_to_end(&mut body)?;
        latest_in_page(&String::from_utf8_lossy(&body))
            .ok_or_else(|| Error::Other("reshade.me's page links no ReShade_Setup_<version>.exe".into()))
    }

    /// Download and check the setup for `version` into `dest_dir`. A copy
    /// already there is checked and reused.
    pub fn download(&self, version: &str, dest_dir: &Path, progress: &mut dyn FnMut(u64, u64)) -> Result<(PathBuf, Setup)> {
        if !is_version(version) {
            return Err(Error::Other(format!("{version:?} is not a ReShade version")));
        }
        std::fs::create_dir_all(dest_dir)?;
        let name = setup_file_name(version);
        let final_path = dest_dir.join(&name);
        if final_path.is_file()
            && let Ok(s) = read_setup(&final_path)
        {
            return Ok((final_path, s));
        }
        let url = format!("{}/downloads/{name}", self.base);
        activity::record_path(Kind::Download, format!("Downloading {}", activity::safe_url(&url)), &final_path);
        let resp = self.http.get(&url).send()?;
        if !resp.status().is_success() {
            return Err(Error::Other(format!("reshade.me answered {} for {name}", resp.status())));
        }
        let total = resp.content_length().unwrap_or(0);
        if total > MAX_SETUP {
            return Err(Error::Integrity(format!("{name} is larger than a ReShade setup ({})", activity::size(total))));
        }
        let part = dest_dir.join(format!("{name}.part"));
        let mut out = std::fs::File::create(&part)?;
        let mut reader = resp.take(MAX_SETUP + 1);
        let mut buf = vec![0u8; 1 << 16];
        let mut done = 0u64;
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            done += n as u64;
            if done > MAX_SETUP {
                drop(out);
                let _ = std::fs::remove_file(&part);
                return Err(Error::Integrity(format!("{name} is larger than a ReShade setup; discarded")));
            }
            out.write_all(&buf[..n])?;
            progress(done, total.max(done));
        }
        out.sync_all()?;
        drop(out);
        if total > 0 && done != total {
            let _ = std::fs::remove_file(&part);
            return Err(Error::Integrity(format!("{name}: got {done} of {total} bytes; discarded")));
        }
        std::fs::rename(&part, &final_path)?;
        match read_setup(&final_path) {
            Ok(s) => Ok((final_path, s)),
            Err(e) => {
                let _ = std::fs::remove_file(&final_path);
                activity::record_path(Kind::Error, format!("Discarded {name}: {e}"), &final_path);
                Err(e)
            }
        }
    }
}

fn bin_file(game_dir: &Path, name: &str) -> PathBuf {
    resolve_ci(game_dir, &format!("{BIN}/{name}"))
}

/// ReShade installed by the app (the newest, if several).
pub fn installed_mod(db: &Db, game_id: i64) -> Result<Option<ModRow>> {
    Ok(db.mods(game_id)?.into_iter().filter(|m| m.source == SOURCE).max_by_key(|m| m.id))
}

/// Which of [`DLL_NAMES`] a mod installed ReShade as.
fn mod_dll(db: &Db, mod_id: i64) -> Result<Option<String>> {
    Ok(db.mod_files(mod_id)?.into_iter().find_map(|f| {
        let name = f.rel_path.rsplit('/').next().unwrap_or_default().to_ascii_lowercase();
        DLL_NAMES.contains(&name.as_str()).then_some(name)
    }))
}

/// A ReShade DLL in the game folder, by name (`dxgi.dll`), installed by the
/// app or by hand.
pub fn installed_dll(game_dir: &Path) -> Option<&'static str> {
    DLL_NAMES.iter().copied().find(|n| is_reshade_dll(&bin_file(game_dir, n)))
}

/// The name to install ReShade under: `wanted` if given, else the name the
/// installed copy uses, else the first free name. A name is usable when no
/// file has it or the file there is ReShade (a copy installed by hand is
/// backed up and put back on uninstall).
pub fn pick_dll(db: &Db, game: &GameRow, wanted: Option<&str>) -> Result<String> {
    let game_dir = Path::new(&game.path);
    let usable = |name: &str| -> Result<()> {
        let path = bin_file(game_dir, name);
        if !path.exists() || is_reshade_dll(&path) {
            return Ok(());
        }
        let rel = format!("{BIN}/{name}");
        let owner = db.owners_of(game.id, &rel, -1)?.first().map(|f| f.mod_id);
        Err(Error::Conflict(match owner {
            Some(id) => format!("{rel} belongs to {}", db.get_mod(id)?.name),
            None => format!("{rel} is already there and is not ReShade (another injector or upscaler)"),
        }))
    };
    if let Some(w) = wanted {
        let w = w.to_ascii_lowercase();
        if !DLL_NAMES.contains(&w.as_str()) {
            return Err(Error::Other(format!("ReShade can't load as {w} in a DirectX 12 game; use {}", DLL_NAMES.join(" or "))));
        }
        usable(&w)?;
        return Ok(w);
    }
    if let Some(m) = installed_mod(db, game.id)?
        && let Some(name) = mod_dll(db, m.id)?
    {
        return Ok(name);
    }
    let mut taken = Vec::new();
    for name in DLL_NAMES {
        match usable(name) {
            Ok(()) => return Ok(name.to_string()),
            Err(Error::Conflict(why)) => taken.push(why),
            Err(e) => return Err(e),
        }
    }
    Err(Error::Conflict(taken.join("; ")))
}

/// Put the DLL in a staging folder laid out like the game folder.
fn stage(inst: &Installer, setup: &Setup, dll_name: &str) -> Result<Prepared> {
    std::fs::create_dir_all(&inst.staging_root)?;
    let dir = tempfile::Builder::new().prefix("incoming-").tempdir_in(&inst.staging_root)?.keep();
    let id = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let rel = format!("{BIN}/{dll_name}");
    let path = dir.join("files").join(&rel);
    std::fs::create_dir_all(path.parent().unwrap_or(&dir))?;
    std::fs::write(&path, &setup.dll)?;
    Ok(Prepared {
        id,
        archive_name: setup.file_name.clone(),
        archive_sha256: setup.sha256.clone(),
        archive_md5: setup.md5.clone(),
        file_count: 1,
        fomod: None,
        dir,
        files: vec![rel],
    })
}

fn ini_key(game_id: i64) -> String {
    format!("{INI_CREATED}:{game_id}")
}

/// Install (or update to) the ReShade in `setup_path` as `dll` (or the name
/// [`pick_dll`] chooses). The previous ReShade, if any, is replaced.
pub fn install(inst: &Installer, game: &GameRow, setup_path: &Path, dll: Option<&str>) -> Result<InstallReport> {
    let setup = read_setup(setup_path)?;
    let dll_name = pick_dll(inst.db, game, dll)?;
    let p = stage(inst, &setup, &dll_name)?;
    let meta = NewMod {
        name: MOD_NAME.into(),
        version: Some(setup.version.clone()),
        source: SOURCE.into(),
        archive_name: setup.file_name.clone(),
        category: Some("ReShade".into()),
        source_ref: Some("reshade.me".into()),
        source_file: Some(dll_name.clone()),
        ..Default::default()
    };
    let previous = installed_mod(inst.db, game.id)?;
    let opts = InstallOptions { meta, overwrite: false, fomod_choices: None };
    let result = match &previous {
        Some(old) => inst.finish_replacing(game, &p, opts, old.id),
        None => inst.finish(game, &p, opts),
    };
    let report = match result {
        Ok(r) => r,
        Err(e) => {
            inst.discard(&p);
            return Err(e);
        }
    };
    let ini = bin_file(Path::new(&game.path), INI);
    if !ini.exists() {
        std::fs::write(&ini, DEFAULT_INI)?;
        inst.db.set_setting(&ini_key(game.id), "1")?;
        activity::record_path(Kind::Copy, "Wrote ReShade's settings (search paths for reshade-shaders)", &ini);
    }
    Ok(report)
}

/// After ReShade was uninstalled: remove the settings file the app created
/// and ReShade's logs. Presets stay. Does nothing while another ReShade mod
/// is still installed.
pub fn after_uninstall(db: &Db, game: &GameRow) -> Result<Vec<String>> {
    if installed_mod(db, game.id)?.is_some() || db.get_setting(&ini_key(game.id))?.is_none() {
        return Ok(vec![]);
    }
    let game_dir = Path::new(&game.path);
    let mut removed = Vec::new();
    for name in RUNTIME_FILES {
        let rel = format!("{BIN}/{name}");
        let path = bin_file(game_dir, name);
        if path.is_file() && db.owners_of(game.id, &rel, -1)?.is_empty() {
            std::fs::remove_file(&path)?;
            activity::record_path(Kind::Delete, "Removed ReShade's file", &path);
            removed.push(rel);
        }
    }
    db.conn.execute("DELETE FROM settings WHERE key = ?1", [ini_key(game.id)])?;
    Ok(removed)
}

/// ReShade in one game, for the ReShade card.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    /// Installed by the app.
    pub installed: Option<Installed>,
    /// A ReShade DLL that wasn't installed by the app, by name.
    pub unmanaged: Option<String>,
    /// The name a new install would use, or why none is free.
    pub dll_choice: std::result::Result<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Installed {
    pub mod_id: i64,
    pub version: Option<String>,
    pub dll: Option<String>,
    pub enabled: bool,
    pub setup_sha256: String,
}

pub fn status(db: &Db, game: &GameRow) -> Result<Status> {
    let installed = match installed_mod(db, game.id)? {
        Some(m) => Some(Installed {
            mod_id: m.id,
            version: m.version.clone(),
            dll: mod_dll(db, m.id)?,
            enabled: m.enabled(),
            setup_sha256: m.archive_sha256.clone(),
        }),
        None => None,
    };
    let game_dir = Path::new(&game.path);
    let unmanaged = DLL_NAMES
        .iter()
        .find(|n| {
            let rel = format!("{BIN}/{n}");
            is_reshade_dll(&bin_file(game_dir, n)) && db.owners_of(game.id, &rel, -1).is_ok_and(|o| o.is_empty())
        })
        .map(|n| n.to_string());
    let dll_choice = pick_dll(db, game, None).map_err(|e| match e {
        Error::Conflict(why) => why,
        e => e.to_string(),
    });
    Ok(Status { installed, unmanaged, dll_choice })
}

// ---- Shader packs ------------------------------------------------------------

/// `mods.source` of a shader pack; `source_ref` is its `owner/repo` and
/// `source_file` the commit it was installed from.
pub const PACK_SOURCE: &str = "reshade-shaders";
/// Each pack gets its own folder under `Shaders` and `Textures`, so two packs
/// shipping the same header (`ReShade.fxh`) don't collide.
pub const SHADERS_DIR: &str = "bin/x64/reshade-shaders/Shaders";
pub const TEXTURES_DIR: &str = "bin/x64/reshade-shaders/Textures";
const SHADER_EXTS: &[&str] = &["fx", "fxh", "h", "hlsl"];
const TEXTURE_EXTS: &[&str] = &["png", "jpg", "jpeg", "dds", "bmp", "tga"];

/// A shader collection on GitHub, installed from its own repository.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Pack {
    /// Folder name under `Shaders` and `Textures`.
    pub id: &'static str,
    /// `owner/repo`.
    pub repo: &'static str,
    /// `None`: the repository's default branch.
    pub branch: Option<&'static str>,
    pub name: &'static str,
    pub what: &'static str,
}

/// The shader repositories offered, all from their authors' own GitHub.
pub const PACKS: &[Pack] = &[
    Pack {
        id: "Standard",
        repo: "crosire/reshade-shaders",
        branch: Some("slim"),
        name: "Standard effects",
        what: "The effects ReShade's own installer offers first (SMAA, LumaSharpen, Vibrance, Deband, …).",
    },
    Pack { id: "SweetFX", repo: "CeeJayDK/SweetFX", branch: None, name: "SweetFX", what: "Colour, tonemapping, sharpening and film effects." },
    Pack { id: "qUINT", repo: "martymcmodding/qUINT", branch: None, name: "qUINT", what: "MXAO ambient occlusion, Lightroom colour grading, depth of field." },
    Pack { id: "iMMERSE", repo: "martymcmodding/iMMERSE", branch: None, name: "iMMERSE", what: "Marty McFly's free effects: sharpening, anti-aliasing, MXAO." },
    Pack { id: "prod80", repo: "prod80/prod80-ReShade-Repository", branch: None, name: "prod80", what: "Colour correction and grading." },
    Pack { id: "AstrayFX", repo: "BlueSkyDefender/AstrayFX", branch: None, name: "AstrayFX", what: "Clarity, smart sharpening, bloom and other image effects." },
    Pack { id: "FXShaders", repo: "luluco250/FXShaders", branch: None, name: "FXShaders", what: "Bloom, motion blur and stylised effects." },
];

pub fn pack(id: &str) -> Result<&'static Pack> {
    PACKS.iter().find(|p| p.id.eq_ignore_ascii_case(id)).ok_or_else(|| Error::Other(format!("{id} is not a listed shader pack")))
}

fn ext_of(name: &str) -> String {
    name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default()
}

/// Where each file of a pack's source archive goes: shader files from its
/// `Shaders` folder and images from its `Textures` folder, into the pack's
/// own folders. Everything else (readmes, scripts, images outside
/// `Textures`) is left out. Returns (archive path, game path) pairs.
pub fn pack_layout(files: &[String], id: &str) -> Vec<(String, String)> {
    // The shallowest `Shaders` folder; source archives wrap everything in
    // an `owner-repo-sha/` folder.
    let prefix = files
        .iter()
        .filter_map(|f| {
            let parts: Vec<&str> = f.split('/').collect();
            let i = parts[..parts.len().saturating_sub(1)].iter().position(|p| p.eq_ignore_ascii_case("Shaders"))?;
            Some((i, parts[..i].join("/")))
        })
        .min_by_key(|(i, _)| *i)
        .map(|(_, p)| if p.is_empty() { p } else { format!("{p}/") });
    let Some(prefix) = prefix else { return Vec::new() };
    let mut out = Vec::new();
    for f in files {
        let Some(rest) = f.strip_prefix(&prefix) else { continue };
        let Some((top, inner)) = rest.split_once('/') else { continue };
        let ext = ext_of(inner);
        if top.eq_ignore_ascii_case("Shaders") && SHADER_EXTS.contains(&ext.as_str()) {
            out.push((f.clone(), format!("{SHADERS_DIR}/{id}/{inner}")));
        } else if top.eq_ignore_ascii_case("Textures") && TEXTURE_EXTS.contains(&ext.as_str()) {
            out.push((f.clone(), format!("{TEXTURES_DIR}/{id}/{inner}")));
        }
    }
    out
}

/// The installed copy of a pack (the newest, if several).
pub fn pack_mod(db: &Db, game_id: i64, pack: &Pack) -> Result<Option<ModRow>> {
    Ok(db
        .mods(game_id)?
        .into_iter()
        .filter(|m| m.source == PACK_SOURCE && m.source_ref.as_deref().is_some_and(|r| r.eq_ignore_ascii_case(pack.repo)))
        .max_by_key(|m| m.id))
}

/// Unix time as `YYYY-MM-DD` (UTC).
fn ymd(unix: i64) -> String {
    // Days to civil date (Howard Hinnant's algorithm).
    let z = unix.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// "2026-09-12 abc1234": the commit date and short id.
pub fn pack_version(commit: &crate::sources::github::Commit) -> String {
    let date = commit.date.map(ymd).unwrap_or_default();
    format!("{date} {}", &commit.sha[..7]).trim().to_string()
}

/// Install (or update) `pack` from the source archive of `commit`.
pub fn install_pack(
    inst: &Installer,
    game: &GameRow,
    pack: &Pack,
    archive: &Path,
    commit: &crate::sources::github::Commit,
) -> Result<InstallReport> {
    let mut p = inst.prepare(game, archive)?;
    p.fomod = None;
    let layout = pack_layout(&p.files, pack.id);
    if !layout.iter().any(|(_, t)| ext_of(t) == "fx") {
        inst.discard(&p);
        return Err(Error::Other(format!("{} has no .fx effects in a Shaders folder", pack.repo)));
    }
    // Lay the staged files out like the game folder.
    let relaid = p.dir.join("relaid");
    let moved = (|| -> Result<()> {
        for (from, to) in &layout {
            let dst = relaid.join(to);
            std::fs::create_dir_all(dst.parent().unwrap_or(&relaid))?;
            std::fs::rename(p.dir.join("files").join(from), dst)?;
        }
        std::fs::remove_dir_all(p.dir.join("files"))?;
        std::fs::rename(&relaid, p.dir.join("files"))?;
        Ok(())
    })();
    if let Err(e) = moved {
        inst.discard(&p);
        return Err(e);
    }
    p.files = layout.into_iter().map(|(_, t)| t).collect();
    p.file_count = p.files.len();
    let meta = NewMod {
        name: format!("{} (ReShade shaders)", pack.name),
        version: Some(pack_version(commit)),
        source: PACK_SOURCE.into(),
        archive_name: p.archive_name.clone(),
        category: Some("ReShade".into()),
        source_ref: Some(pack.repo.into()),
        source_file: Some(commit.sha.clone()),
        ..Default::default()
    };
    let opts = InstallOptions { meta, overwrite: false, fomod_choices: None };
    let result = match pack_mod(inst.db, game.id, pack)? {
        Some(old) => inst.finish_replacing(game, &p, opts, old.id),
        None => inst.finish(game, &p, opts),
    };
    if result.is_err() {
        inst.discard(&p);
    }
    result
}

#[derive(Debug, Clone, Serialize)]
pub struct PackState {
    #[serde(flatten)]
    pub pack: Pack,
    pub mod_id: Option<i64>,
    pub version: Option<String>,
    /// Commit the installed copy came from.
    pub commit: Option<String>,
    pub enabled: bool,
}

pub fn packs(db: &Db, game_id: i64) -> Result<Vec<PackState>> {
    PACKS
        .iter()
        .map(|p| {
            let m = pack_mod(db, game_id, p)?;
            Ok(PackState {
                pack: *p,
                mod_id: m.as_ref().map(|m| m.id),
                version: m.as_ref().and_then(|m| m.version.clone()),
                commit: m.as_ref().and_then(|m| m.source_file.clone()),
                enabled: m.as_ref().is_some_and(|m| m.enabled()),
            })
        })
        .collect()
}

// ---- Presets ---------------------------------------------------------------

/// A ReShade preset in the game folder (`bin/x64/*.ini` with a
/// `Techniques=` line).
#[derive(Debug, Clone, Serialize)]
pub struct Preset {
    pub file: String,
    pub techniques: usize,
    /// Effect files the preset uses that aren't under `reshade-shaders/Shaders`.
    pub missing: Vec<String>,
    pub active: bool,
    /// The mod that installed it, if any.
    pub mod_name: Option<String>,
}

const MAX_PRESET: u64 = 1 << 20;

/// `PresetPath` from `ReShade.ini`'s `[GENERAL]` section.
fn ini_value(ini: &str, section: &str, key: &str) -> Option<String> {
    let mut cur = String::new();
    for line in ini.lines() {
        let t = line.trim();
        if t.starts_with('[') && t.ends_with(']') {
            cur = t[1..t.len() - 1].to_string();
        } else if cur.eq_ignore_ascii_case(section)
            && let Some((k, v)) = t.split_once('=')
            && k.trim().eq_ignore_ascii_case(key)
        {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// Effect files (`Name@File.fx`) a preset's `Techniques=` line uses, and how
/// many techniques it lists; `None` if the file isn't a preset.
fn preset_techniques(text: &str) -> Option<(usize, Vec<String>)> {
    let line = text.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        k.trim().eq_ignore_ascii_case("Techniques").then(|| v.trim().to_string())
    })?;
    let entries: Vec<&str> = line.split(',').map(str::trim).filter(|e| !e.is_empty()).collect();
    let mut files: Vec<String> = entries.iter().filter_map(|e| e.split_once('@').map(|(_, f)| f.trim().to_string())).collect();
    files.sort_by_key(|f| f.to_ascii_lowercase());
    files.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    Some((entries.len(), files))
}

fn file_name_of(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

pub fn presets(db: &Db, game: &GameRow) -> Result<Vec<Preset>> {
    let game_dir = Path::new(&game.path);
    let bin = resolve_ci(game_dir, BIN);
    let effects: std::collections::HashSet<String> = walkdir::WalkDir::new(resolve_ci(game_dir, SHADERS_DIR))
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .map(|e| e.file_name().to_string_lossy().to_ascii_lowercase())
        .collect();
    let active = std::fs::read_to_string(bin.join(INI))
        .ok()
        .and_then(|ini| ini_value(&ini, "GENERAL", "PresetPath"))
        .map(|p| file_name_of(&p).to_ascii_lowercase());
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&bin) else { return Ok(out) };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if ext_of(&name) != "ini" || name.eq_ignore_ascii_case(INI) || !e.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        if e.metadata().map(|m| m.len()).unwrap_or(u64::MAX) > MAX_PRESET {
            continue;
        }
        let Ok(bytes) = std::fs::read(e.path()) else { continue };
        let Some((techniques, files)) = preset_techniques(&String::from_utf8_lossy(&bytes)) else { continue };
        let missing = files.into_iter().filter(|f| !effects.contains(&file_name_of(f).to_ascii_lowercase())).collect();
        let owner = db.owners_of(game.id, &format!("{BIN}/{name}"), -1)?.first().map(|f| f.mod_id);
        out.push(Preset {
            active: active.as_deref() == Some(name.to_ascii_lowercase().as_str()),
            mod_name: owner.map(|id| db.get_mod(id).map(|m| m.name)).transpose()?,
            file: name,
            techniques,
            missing,
        });
    }
    out.sort_by_key(|p| p.file.to_ascii_lowercase());
    Ok(out)
}

/// `ini` with `key=value` set in `[section]` (added if missing).
fn set_ini_value(ini: &str, section: &str, key: &str, value: &str) -> String {
    let nl = if ini.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = ini.lines().map(String::from).collect();
    let mut cur = String::new();
    let mut section_at = None;
    for (i, line) in lines.iter_mut().enumerate() {
        let t = line.trim();
        if t.starts_with('[') && t.ends_with(']') {
            cur = t[1..t.len() - 1].to_string();
            if cur.eq_ignore_ascii_case(section) {
                section_at = Some(i);
            }
        } else if cur.eq_ignore_ascii_case(section) && t.split_once('=').is_some_and(|(k, _)| k.trim().eq_ignore_ascii_case(key)) {
            *line = format!("{key}={value}");
            return lines.join(nl) + nl;
        }
    }
    match section_at {
        Some(i) => lines.insert(i + 1, format!("{key}={value}")),
        None => {
            lines.insert(0, format!("[{section}]"));
            lines.insert(1, format!("{key}={value}"));
        }
    }
    lines.join(nl) + nl
}

/// Make `file` (a preset listed by [`presets`]) the one ReShade loads at the
/// next start, by setting `PresetPath` in `ReShade.ini`.
pub fn set_active_preset(db: &Db, game: &GameRow, file: &str) -> Result<()> {
    if !presets(db, game)?.iter().any(|p| p.file == file) {
        return Err(Error::Other(format!("{file} is not a ReShade preset in bin/x64")));
    }
    let ini_path = bin_file(Path::new(&game.path), INI);
    let ini = std::fs::read_to_string(&ini_path).unwrap_or_else(|_| DEFAULT_INI.to_string());
    let new = set_ini_value(&ini, "GENERAL", "PresetPath", &format!(".\\{file}"));
    let tmp = ini_path.with_extension("ini.cpmx-tmp");
    std::fs::write(&tmp, new)?;
    std::fs::rename(&tmp, &ini_path)?;
    activity::record_path(Kind::Copy, format!("Set ReShade's preset to {file}"), &ini_path);
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::archive::Limits;
    use crate::game::{GameInstall, Store};

    pub const STAND_IN: &[u8] = include_bytes!("../tests/fixtures/reshade64-stand-in.dll");

    /// A setup program like reshade.me's: an executable with a zip appended.
    pub fn fake_setup(dir: &Path, name: &str, dll: &[u8]) -> PathBuf {
        let mut zip_bytes = Vec::new();
        {
            let mut z = zip::ZipWriter::new(Cursor::new(&mut zip_bytes));
            let o = zip::write::SimpleFileOptions::default();
            z.start_file("ReShade32.dll", o).unwrap();
            z.write_all(b"32-bit").unwrap();
            z.start_file("ReShade64.dll", o).unwrap();
            z.write_all(dll).unwrap();
            z.finish().unwrap();
        }
        let mut exe = b"MZ".to_vec();
        exe.extend(std::iter::repeat_n(0u8, 4094));
        exe.extend(zip_bytes);
        let p = dir.join(name);
        std::fs::write(&p, exe).unwrap();
        p
    }

    pub fn setup_game() -> (tempfile::TempDir, Db, GameRow) {
        let dir = tempfile::tempdir().unwrap();
        let gdir = dir.path().join("game");
        std::fs::create_dir_all(gdir.join("bin/x64")).unwrap();
        let db = Db::open_in_memory().unwrap();
        let gi = GameInstall {
            path: gdir.clone(),
            store: Store::Manual,
            proton_prefix: None,
            build_id: None,
            exe_file_version: None,
            exe_product_version: None,
            frameworks: vec![],
            launch_options: None,
            warnings: vec![],
        };
        let id = db.upsert_game(&gi).unwrap();
        let game = db.game(id).unwrap();
        (dir, db, game)
    }

    pub fn installer<'a>(db: &'a Db, root: &Path) -> Installer<'a> {
        Installer { db, staging_root: root.join("staging"), backups_root: root.join("backups"), limits: Limits::default() }
    }

    #[test]
    fn finds_latest_standard_setup_on_page() {
        let html = r#"<a href="/downloads/ReShade_Setup_6.2.0.exe">old</a>
            <a class="btn" href="/downloads/ReShade_Setup_6.3.10_Addon.exe">add-on</a>
            <a class="btn" href="/downloads/ReShade_Setup_6.3.9.exe">6.3.9</a>
            <a href="/downloads/ReShade_Setup_6.3.10.exe">6.3.10</a>"#;
        assert_eq!(latest_in_page(html).as_deref(), Some("6.3.10"));
        assert_eq!(latest_in_page("<html>nothing</html>"), None);
        assert_eq!(latest_in_page("/downloads/ReShade_Setup_../../x.exe"), None);
    }

    #[test]
    fn reads_versions_from_names() {
        assert_eq!(version_from_file_name("ReShade_Setup_6.3.0.exe").as_deref(), Some("6.3.0"));
        assert_eq!(version_from_file_name("reshade_setup_6.3.0_Addon.exe").as_deref(), Some("6.3.0"));
        assert_eq!(version_from_file_name("ReShade_Setup_6.3.0 (1).exe"), None);
        assert_eq!(cmp_versions("6.3.10", "6.3.9"), Ordering::Greater);
        assert_eq!(cmp_versions("6.3", "6.3.0"), Ordering::Equal);
    }

    #[test]
    fn checks_the_dll_inside_the_setup() {
        let d = tempfile::tempdir().unwrap();
        let s = read_setup(&fake_setup(d.path(), "ReShade_Setup_6.3.0.exe", STAND_IN)).unwrap();
        assert_eq!(s.version, "6.3.0");
        assert_eq!(s.dll_version.as_deref(), Some("6.3.0.1999"));
        assert_eq!(s.dll, STAND_IN);

        let e = read_setup(&fake_setup(d.path(), "ReShade_Setup_6.4.0.exe", STAND_IN)).unwrap_err();
        assert!(e.to_string().contains("contains ReShade 6.3.0.1999, not 6.4.0"), "{e}");
        let e = read_setup(&fake_setup(d.path(), "ReShade_Setup_6.3.0.exe", b"MZ not a dll")).unwrap_err();
        assert!(e.to_string().contains("not a 64-bit Windows DLL"), "{e}");
        let plain = d.path().join("ReShade_Setup_6.3.0.exe");
        std::fs::write(&plain, b"MZ no zip here").unwrap();
        assert!(read_setup(&plain).unwrap_err().to_string().contains("contains no ReShade64.dll"));
        std::fs::write(&plain, b"PK not an exe").unwrap();
        assert!(read_setup(&plain).unwrap_err().to_string().contains("not a Windows program"));
        // Renamed by the user: the version comes from the DLL.
        let s = read_setup(&fake_setup(d.path(), "reshade.exe", STAND_IN)).unwrap();
        assert_eq!(s.version, "6.3.0");
    }

    #[test]
    fn installs_disables_and_uninstalls_without_leftovers() {
        let (dir, db, game) = setup_game();
        let gdir = PathBuf::from(&game.path);
        let inst = installer(&db, dir.path());
        let setup = fake_setup(dir.path(), "ReShade_Setup_6.3.0.exe", STAND_IN);
        std::fs::write(gdir.join("bin/x64/Cyberpunk2077.exe"), b"exe").unwrap();

        let r = install(&inst, &game, &setup, None).unwrap();
        assert_eq!(r.files_installed, 1);
        assert_eq!(std::fs::read(gdir.join("bin/x64/dxgi.dll")).unwrap(), STAND_IN);
        assert_eq!(std::fs::read_to_string(gdir.join("bin/x64/ReShade.ini")).unwrap(), DEFAULT_INI);
        assert_eq!(installed_dll(&gdir), Some("dxgi.dll"));
        let st = status(&db, &game).unwrap();
        let i = st.installed.unwrap();
        assert_eq!((i.version.as_deref(), i.dll.as_deref(), i.enabled), (Some("6.3.0"), Some("dxgi.dll"), true));
        assert_eq!(st.unmanaged, None);

        // ReShade rewrites its ini and logs while the game runs.
        std::fs::write(gdir.join("bin/x64/ReShade.ini"), "[GENERAL]\nchanged").unwrap();
        std::fs::write(gdir.join("bin/x64/ReShade.log"), "log").unwrap();
        std::fs::write(gdir.join("bin/x64/MyLook.ini"), "Techniques=SMAA@SMAA.fx").unwrap();

        inst.disable(r.mod_id).unwrap();
        assert!(!gdir.join("bin/x64/dxgi.dll").exists());
        inst.enable(r.mod_id, false).unwrap();
        assert!(gdir.join("bin/x64/dxgi.dll").exists());

        // An update replaces it and keeps the user's settings.
        let setup2 = fake_setup(dir.path(), "ReShade_Setup_6.3.0.exe", STAND_IN);
        let r2 = install(&inst, &game, &setup2, None).unwrap();
        assert_eq!(r2.replaced.as_deref(), Some("ReShade 6.3.0"));
        assert_eq!(std::fs::read_to_string(gdir.join("bin/x64/ReShade.ini")).unwrap(), "[GENERAL]\nchanged");

        inst.uninstall(r2.mod_id).unwrap();
        let removed = after_uninstall(&db, &game).unwrap();
        assert_eq!(removed, ["bin/x64/ReShade.ini", "bin/x64/ReShade.log"]);
        let mut left: Vec<String> =
            std::fs::read_dir(gdir.join("bin/x64")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["Cyberpunk2077.exe", "MyLook.ini"], "presets stay, everything else is gone");
    }

    #[test]
    fn keeps_a_settings_file_that_was_there_before() {
        let (dir, db, game) = setup_game();
        let gdir = PathBuf::from(&game.path);
        let inst = installer(&db, dir.path());
        std::fs::write(gdir.join("bin/x64/ReShade.ini"), "mine").unwrap();
        let r = install(&inst, &game, &fake_setup(dir.path(), "ReShade_Setup_6.3.0.exe", STAND_IN), None).unwrap();
        inst.uninstall(r.mod_id).unwrap();
        assert!(after_uninstall(&db, &game).unwrap().is_empty());
        assert_eq!(std::fs::read_to_string(gdir.join("bin/x64/ReShade.ini")).unwrap(), "mine");
    }

    #[test]
    fn picks_a_free_dll_name_and_restores_a_hand_installed_copy() {
        let (dir, db, game) = setup_game();
        let gdir = PathBuf::from(&game.path);
        let inst = installer(&db, dir.path());
        let setup = fake_setup(dir.path(), "ReShade_Setup_6.3.0.exe", STAND_IN);

        // Another injector owns dxgi.dll: ReShade goes in as d3d12.dll.
        std::fs::write(gdir.join("bin/x64/dxgi.dll"), b"MZ something else").unwrap();
        assert_eq!(pick_dll(&db, &game, None).unwrap(), "d3d12.dll");
        assert!(matches!(pick_dll(&db, &game, Some("dxgi.dll")), Err(Error::Conflict(_))));
        assert!(pick_dll(&db, &game, Some("version.dll")).is_err());
        std::fs::write(gdir.join("bin/x64/d3d12.dll"), b"MZ also taken").unwrap();
        let e = pick_dll(&db, &game, None).unwrap_err().to_string();
        assert!(e.contains("dxgi.dll is already there") && e.contains("d3d12.dll is already there"), "{e}");
        std::fs::remove_file(gdir.join("bin/x64/d3d12.dll")).unwrap();
        std::fs::remove_file(gdir.join("bin/x64/dxgi.dll")).unwrap();

        // A ReShade copied in by hand is replaced, and comes back on uninstall.
        let mut hand = STAND_IN.to_vec();
        hand.extend_from_slice(b"hand");
        std::fs::write(gdir.join("bin/x64/dxgi.dll"), &hand).unwrap();
        assert_eq!(status(&db, &game).unwrap().unmanaged.as_deref(), Some("dxgi.dll"));
        let r = install(&inst, &game, &setup, None).unwrap();
        assert_eq!(r.backed_up_game_files, ["bin/x64/dxgi.dll"]);
        inst.uninstall(r.mod_id).unwrap();
        assert_eq!(std::fs::read(gdir.join("bin/x64/dxgi.dll")).unwrap(), hand);
    }

    #[test]
    fn lays_out_a_shader_repository() {
        let files: Vec<String> = [
            "SweetFX-abc123/Shaders/SweetFX/Vibrance.fx",
            "SweetFX-abc123/Shaders/ReShade.fxh",
            "SweetFX-abc123/Shaders/install.bat",
            "SweetFX-abc123/Textures/LUT.png",
            "SweetFX-abc123/Textures/notes.txt",
            "SweetFX-abc123/README.md",
            "SweetFX-abc123/Docs/Shaders/old.fx",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let l = pack_layout(&files, "SweetFX");
        let targets: Vec<&str> = l.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(
            targets,
            [
                "bin/x64/reshade-shaders/Shaders/SweetFX/SweetFX/Vibrance.fx",
                "bin/x64/reshade-shaders/Shaders/SweetFX/ReShade.fxh",
                "bin/x64/reshade-shaders/Textures/SweetFX/LUT.png",
            ]
        );
        assert!(pack_layout(&["x/readme.md".to_string()], "X").is_empty());
        assert_eq!(ymd(1_757_635_200), "2025-09-12");
        assert_eq!(ymd(0), "1970-01-01");
    }

    #[test]
    fn installs_and_updates_a_pack_from_a_source_archive() {
        let (dir, db, game) = setup_game();
        let gdir = PathBuf::from(&game.path);
        let inst = installer(&db, dir.path());
        let zip_at = |name: &str, files: &[(&str, &str)]| {
            let p = dir.path().join(name);
            let mut z = zip::ZipWriter::new(std::fs::File::create(&p).unwrap());
            for (n, body) in files {
                z.start_file(*n, zip::write::SimpleFileOptions::default()).unwrap();
                z.write_all(body.as_bytes()).unwrap();
            }
            z.finish().unwrap();
            p
        };
        let pk = pack("sweetfx").unwrap();
        let c1 = crate::sources::github::Commit { sha: "a".repeat(40), date: Some(1_757_635_200) };
        let a = zip_at("a.zip", &[("r-aaa/Shaders/Vibrance.fx", "v1"), ("r-aaa/Shaders/ReShade.fxh", "h"), ("r-aaa/run.exe", "MZ")]);
        let r = install_pack(&inst, &game, pk, &a, &c1).unwrap();
        assert_eq!(r.files_installed, 2);
        assert!(gdir.join("bin/x64/reshade-shaders/Shaders/SweetFX/Vibrance.fx").is_file());
        assert!(!gdir.join("run.exe").exists() && !gdir.join("bin/x64/reshade-shaders/Shaders/SweetFX/run.exe").exists());
        let st = packs(&db, game.id).unwrap().into_iter().find(|p| p.pack.id == "SweetFX").unwrap();
        assert_eq!(st.version.as_deref(), Some("2025-09-12 aaaaaaa"));

        // A preset using it, and one using an effect nobody installed.
        std::fs::write(gdir.join("bin/x64/Night.ini"), "Techniques=Vibrance@Vibrance.fx,MXAO@qUINT_mxao.fx\n").unwrap();
        std::fs::write(gdir.join("bin/x64/Day.ini"), "Techniques=Vibrance@Vibrance.fx\n").unwrap();
        std::fs::write(gdir.join("bin/x64/ReShade.ini"), "[GENERAL]\r\nPresetPath=.\\Day.ini\r\n").unwrap();
        std::fs::write(gdir.join("bin/x64/other.ini"), "[Settings]\nx=1\n").unwrap();
        let ps = presets(&db, &game).unwrap();
        assert_eq!(ps.iter().map(|p| p.file.as_str()).collect::<Vec<_>>(), ["Day.ini", "Night.ini"]);
        assert!(ps[0].active && ps[0].missing.is_empty());
        assert_eq!(ps[1].missing, ["qUINT_mxao.fx"]);
        set_active_preset(&db, &game, "Night.ini").unwrap();
        assert_eq!(std::fs::read_to_string(gdir.join("bin/x64/ReShade.ini")).unwrap(), "[GENERAL]\r\nPresetPath=.\\Night.ini\r\n");
        assert!(set_active_preset(&db, &game, "../x.ini").is_err());
        assert!(set_active_preset(&db, &game, "other.ini").is_err());

        // An update replaces the files of the old commit.
        let c2 = crate::sources::github::Commit { sha: "b".repeat(40), date: None };
        let b = zip_at("b.zip", &[("r-bbb/Shaders/Vibrance.fx", "v2")]);
        let r2 = install_pack(&inst, &game, pk, &b, &c2).unwrap();
        assert!(r2.replaced.is_some());
        assert!(!gdir.join("bin/x64/reshade-shaders/Shaders/SweetFX/ReShade.fxh").exists());
        inst.uninstall(r2.mod_id).unwrap();
        assert!(!gdir.join("bin/x64/reshade-shaders").exists());

        let none = zip_at("c.zip", &[("r/readme.md", "x")]);
        assert!(install_pack(&inst, &game, pk, &none, &c2).unwrap_err().to_string().contains("no .fx effects"));
    }

    #[test]
    fn edits_ini_values() {
        assert_eq!(set_ini_value("[GENERAL]\nA=1\n[INPUT]\nK=2\n", "INPUT", "K", "3"), "[GENERAL]\nA=1\n[INPUT]\nK=3\n");
        assert_eq!(set_ini_value("[GENERAL]\nA=1\n", "GENERAL", "PresetPath", ".\\x.ini"), "[GENERAL]\nPresetPath=.\\x.ini\nA=1\n");
        assert_eq!(set_ini_value("", "GENERAL", "PresetPath", "p"), "[GENERAL]\nPresetPath=p\n");
        assert_eq!(ini_value("[OTHER]\nPresetPath=no\n[GENERAL]\n PresetPath = .\\a.ini\n", "GENERAL", "PresetPath").as_deref(), Some(".\\a.ini"));
    }

    #[test]
    fn downloads_only_from_the_site_and_checks_the_file() {
        let d = tempfile::tempdir().unwrap();
        let exe = std::fs::read(fake_setup(d.path(), "src.exe", STAND_IN)).unwrap();
        let page = r#"<a href="/downloads/ReShade_Setup_6.3.0.exe">Download</a>"#.as_bytes().to_vec();
        let (base, seen) = crate::testutil::serve_bytes(vec![
            ("/downloads/ReShade_Setup_6.3.0.exe", 200, exe),
            ("/downloads/ReShade_Setup_6.2.0.exe", 200, b"MZ broken".to_vec()),
            ("/", 200, page),
        ]);
        let c = Client::with_base(&base).unwrap();
        assert_eq!(c.latest().unwrap(), "6.3.0");
        let dest = d.path().join("cache");
        let (path, s) = c.download("6.3.0", &dest, &mut |_, _| {}).unwrap();
        assert_eq!(path, dest.join("ReShade_Setup_6.3.0.exe"));
        assert_eq!(s.dll, STAND_IN);
        // The second time the checked copy is reused.
        c.download("6.3.0", &dest, &mut |_, _| {}).unwrap();
        assert_eq!(seen.lock().unwrap().iter().filter(|l| l.contains("6.3.0.exe")).count(), 1);
        // A file that fails the check is not kept.
        assert!(c.download("6.2.0", &dest, &mut |_, _| {}).is_err());
        assert!(!dest.join("ReShade_Setup_6.2.0.exe").exists());
        assert!(c.download("../x", &dest, &mut |_, _| {}).is_err());
    }
}
