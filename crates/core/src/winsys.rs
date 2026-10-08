//! Windows-only lookups: where Steam, GOG Galaxy and the Epic launcher keep
//! their installs, and the `nxm://` handler in the registry. Elsewhere every
//! lookup finds nothing, so callers don't need their own `cfg`s.

use std::path::PathBuf;

/// Registry key (under `HKEY_CURRENT_USER`) for our `nxm://` handler.
pub const NXM_CLASS_KEY: &str = r"Software\Classes\nxm";

/// The `shell\open\command` value that starts `exe` for an `nxm://` link.
pub fn nxm_open_command(exe: &str) -> String {
    format!("\"{exe}\" \"%1\"")
}

#[cfg(windows)]
mod imp {
    use std::path::PathBuf;

    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY};

    use super::NXM_CLASS_KEY;
    use crate::{Error, Result};

    /// Cyberpunk 2077's product id on GOG.
    const GOG_GAME_ID: &str = "1423049311";

    fn read(root: winreg::HKEY, path: &str, value: &str) -> Option<String> {
        let key = RegKey::predef(root)
            .open_subkey_with_flags(path, KEY_READ | KEY_WOW64_32KEY)
            .ok()?;
        key.get_value::<String, _>(value)
            .ok()
            .filter(|s| !s.trim().is_empty())
    }

    pub fn steam_roots() -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = [
            read(HKEY_CURRENT_USER, r"Software\Valve\Steam", "SteamPath"),
            read(HKEY_LOCAL_MACHINE, r"SOFTWARE\Valve\Steam", "InstallPath"),
        ]
        .into_iter()
        .flatten()
        .map(PathBuf::from)
        .collect();
        for var in ["ProgramFiles(x86)", "ProgramFiles"] {
            if let Some(p) = std::env::var_os(var) {
                out.push(PathBuf::from(p).join("Steam"));
            }
        }
        out
    }

    pub fn gog_installs() -> Vec<PathBuf> {
        let key = format!(r"SOFTWARE\GOG.com\Games\{GOG_GAME_ID}");
        read(HKEY_LOCAL_MACHINE, &key, "path")
            .map(PathBuf::from)
            .into_iter()
            .collect()
    }

    pub fn epic_manifests_dir() -> Option<PathBuf> {
        let data = std::env::var_os("ProgramData")?;
        Some(PathBuf::from(data).join(r"Epic\EpicGamesLauncher\Data\Manifests"))
    }

    pub fn crash_report_queue() -> Option<PathBuf> {
        Some(dirs::data_local_dir()?.join(r"REDEngine\ReportQueue"))
    }

    /// The command `nxm://` links start for this user, if any.
    pub fn nxm_command() -> Option<String> {
        let key = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey(format!(r"{NXM_CLASS_KEY}\shell\open\command"))
            .ok()?;
        key.get_value::<String, _>("")
            .ok()
            .filter(|s| !s.trim().is_empty())
    }

    /// Point `nxm://` links at `exe` for this user (no admin rights needed).
    pub fn register_nxm(exe: &str) -> Result<()> {
        let err = |e: std::io::Error| Error::Other(format!("could not register nxm:// links: {e}"));
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (class, _) = hkcu.create_subkey(NXM_CLASS_KEY).map_err(err)?;
        class
            .set_value("", &"URL:Nexus Mods download")
            .map_err(err)?;
        class.set_value("URL Protocol", &"").map_err(err)?;
        let (cmd, _) = class.create_subkey(r"shell\open\command").map_err(err)?;
        cmd.set_value("", &super::nxm_open_command(exe))
            .map_err(err)?;
        Ok(())
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::PathBuf;

    use crate::{Error, Result};

    pub fn steam_roots() -> Vec<PathBuf> {
        vec![]
    }

    pub fn gog_installs() -> Vec<PathBuf> {
        vec![]
    }

    pub fn epic_manifests_dir() -> Option<PathBuf> {
        None
    }

    pub fn crash_report_queue() -> Option<PathBuf> {
        None
    }

    pub fn nxm_command() -> Option<String> {
        None
    }

    pub fn register_nxm(_exe: &str) -> Result<()> {
        Err(Error::Other(
            "the Windows registry only exists on Windows".into(),
        ))
    }
}

pub use imp::*;

/// The Epic launcher's per-game manifests (`*.item`), on Windows.
pub fn epic_manifests() -> Vec<PathBuf> {
    let Some(dir) = epic_manifests_dir() else {
        return vec![];
    };
    let Ok(rd) = std::fs::read_dir(dir) else {
        return vec![];
    };
    rd.flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("item"))
        })
        .collect()
}
