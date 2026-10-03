//! Storing the Nexus API key in the desktop keyring (Secret Service: GNOME
//! Keyring, KWallet, KeePassXC). There is deliberately no plaintext fallback:
//! without a keyring the user is told to install one.

use crate::{APP_NAME, Error, Result};

const ACCOUNT: &str = "nexus-api-key";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Keyring,
}

fn entry() -> Result<keyring::Entry> {
    keyring::Entry::new(APP_NAME, ACCOUNT).map_err(|e| Error::Secret(e.to_string()))
}

pub fn store_api_key(key: &str) -> Result<Backend> {
    let key = key.trim();
    if key.is_empty() {
        return Err(Error::Secret("empty API key".into()));
    }
    entry()?.set_password(key).map_err(|e| {
        Error::Secret(format!(
            "could not save to the system keyring ({e}). Install and unlock a Secret Service \
             provider such as GNOME Keyring, KWallet or KeePassXC."
        ))
    })?;
    Ok(Backend::Keyring)
}

pub fn load_api_key() -> Result<Option<(String, Backend)>> {
    match entry()?.get_password() {
        Ok(k) if !k.trim().is_empty() => Ok(Some((k, Backend::Keyring))),
        Ok(_) | Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(Error::Secret(format!("could not read the system keyring: {e}"))),
    }
}

pub fn delete_api_key() -> Result<()> {
    match entry()?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(Error::Secret(e.to_string())),
    }
}
