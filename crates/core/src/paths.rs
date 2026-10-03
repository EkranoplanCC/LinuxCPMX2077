use std::path::PathBuf;

use crate::{APP_NAME, Error, Result};

/// `~/.local/share/cp2077-modmanager` (or `$XDG_DATA_HOME/...`).
pub fn data_dir() -> Result<PathBuf> {
    let base = dirs::data_dir().ok_or_else(|| Error::Other("no XDG data dir".into()))?;
    let dir = base.join(APP_NAME);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn db_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("library.sqlite3"))
}

/// Downloaded archives are kept so mods can be reinstalled and re-verified.
pub fn downloads_dir() -> Result<PathBuf> {
    let dir = data_dir()?.join("downloads");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Extracted copies of each mod, one directory per mod id.
pub fn staging_dir() -> Result<PathBuf> {
    let dir = data_dir()?.join("staging");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Original game files that a mod overwrote, restored on uninstall.
pub fn backups_dir() -> Result<PathBuf> {
    let dir = data_dir()?.join("backups");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
