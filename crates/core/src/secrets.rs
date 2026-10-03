//! Storing the Nexus API key. The desktop keyring (Secret Service: GNOME
//! Keyring, KWallet) is used when available; otherwise the key goes in a
//! file only the user can read, and callers are told so.

use std::path::PathBuf;

use crate::{APP_NAME, Error, Result};

const ACCOUNT: &str = "nexus-api-key";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Keyring,
    File,
}

fn fallback_path() -> Result<PathBuf> {
    let dir = dirs::config_dir().ok_or_else(|| Error::Secret("no config dir".into()))?.join(APP_NAME);
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("nexus-api-key"))
}

pub fn store_api_key(key: &str) -> Result<Backend> {
    let key = key.trim();
    if key.is_empty() {
        return Err(Error::Secret("empty API key".into()));
    }
    if let Ok(entry) = keyring::Entry::new(APP_NAME, ACCOUNT)
        && entry.set_password(key).is_ok() {
            // Don't leave an older plaintext copy around.
            if let Ok(p) = fallback_path() {
                let _ = std::fs::remove_file(p);
            }
            return Ok(Backend::Keyring);
        }
    write_private(&fallback_path()?, key)?;
    Ok(Backend::File)
}

pub fn load_api_key() -> Result<Option<(String, Backend)>> {
    if let Ok(entry) = keyring::Entry::new(APP_NAME, ACCOUNT)
        && let Ok(k) = entry.get_password() {
            return Ok(Some((k, Backend::Keyring)));
        }
    match std::fs::read_to_string(fallback_path()?) {
        Ok(k) if !k.trim().is_empty() => Ok(Some((k.trim().to_string(), Backend::File))),
        _ => Ok(None),
    }
}

pub fn delete_api_key() -> Result<()> {
    if let Ok(entry) = keyring::Entry::new(APP_NAME, ACCOUNT) {
        let _ = entry.delete_credential();
    }
    let _ = std::fs::remove_file(fallback_path()?);
    Ok(())
}

fn write_private(path: &std::path::Path, contents: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    // mode() only applies on creation; tighten an existing file too.
    f.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    f.write_all(contents.as_bytes())?;
    Ok(())
}
