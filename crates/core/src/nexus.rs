//! Nexus Mods API v1 client using the user's personal API key.
//!
//! Free accounts can't request download links directly; they get a one-time
//! `key`/`expires` pair by clicking "Mod Manager Download" on the website,
//! which opens an `nxm://` link that this app handles. Premium accounts can
//! download straight from the app.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{APP_NAME, APP_VERSION, Error, NEXUS_GAME_DOMAIN, Result, hash};

pub const API_BASE: &str = "https://api.nexusmods.com/v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub user_id: i64,
    pub name: String,
    #[serde(default)]
    pub is_premium: bool,
    #[serde(default)]
    pub is_supporter: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModInfo {
    pub mod_id: i64,
    pub name: Option<String>,
    pub summary: Option<String>,
    pub version: Option<String>,
    pub author: Option<String>,
    pub picture_url: Option<String>,
    #[serde(default)]
    pub contains_adult_content: bool,
    pub status: Option<String>,
    pub updated_timestamp: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInfo {
    pub file_id: i64,
    pub name: Option<String>,
    pub version: Option<String>,
    pub category_name: Option<String>,
    #[serde(default)]
    pub is_primary: bool,
    pub file_name: String,
    pub size_in_bytes: Option<u64>,
    pub uploaded_timestamp: Option<i64>,
    pub mod_version: Option<String>,
    pub external_virus_scan_url: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FilesResponse {
    files: Vec<FileInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadLink {
    pub name: String,
    pub short_name: String,
    #[serde(rename = "URI")]
    pub uri: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Md5Hit {
    #[serde(rename = "mod")]
    pub mod_: Md5Mod,
    pub file_details: Md5File,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Md5Mod {
    pub mod_id: i64,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Md5File {
    pub file_id: i64,
    pub md5: String,
    pub size_in_bytes: Option<u64>,
}

/// A parsed `nxm://` link from the website's "Mod Manager Download" button.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NxmLink {
    pub game: String,
    pub mod_id: i64,
    pub file_id: i64,
    pub key: Option<String>,
    pub expires: Option<i64>,
    pub user_id: Option<i64>,
}

impl NxmLink {
    pub fn parse(s: &str) -> Result<Self> {
        let url = url::Url::parse(s.trim()).map_err(|e| Error::Nexus(format!("bad nxm link: {e}")))?;
        if url.scheme() != "nxm" {
            return Err(Error::Nexus("not an nxm:// link".into()));
        }
        let game = url.host_str().unwrap_or_default().to_ascii_lowercase();
        if game != NEXUS_GAME_DOMAIN {
            return Err(Error::Nexus(format!("link is for '{game}', not Cyberpunk 2077")));
        }
        let segs: Vec<&str> = url.path_segments().map(|s| s.collect()).unwrap_or_default();
        let (mod_id, file_id) = match segs.as_slice() {
            ["mods", m, "files", f] => (
                m.parse().map_err(|_| Error::Nexus("bad mod id".into()))?,
                f.parse().map_err(|_| Error::Nexus("bad file id".into()))?,
            ),
            _ => return Err(Error::Nexus("unexpected nxm link shape".into())),
        };
        let mut link = NxmLink { game, mod_id, file_id, key: None, expires: None, user_id: None };
        for (k, v) in url.query_pairs() {
            match k.as_ref() {
                "key" => link.key = Some(v.into_owned()),
                "expires" => link.expires = v.parse().ok(),
                "user_id" => link.user_id = v.parse().ok(),
                _ => {}
            }
        }
        Ok(link)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Downloaded {
    pub path: PathBuf,
    pub file_name: String,
    pub sha256: String,
    pub md5: String,
    pub size: u64,
    /// Nexus' md5 index confirms this exact file belongs to the mod/file id.
    pub verified: bool,
    pub virus_scan_url: Option<String>,
}

pub struct Client {
    http: reqwest::blocking::Client,
    api_key: String,
    base: String,
}

/// Only fetch archives from Nexus' own hosts, over HTTPS.
pub fn is_allowed_download_url(u: &str) -> bool {
    let Ok(url) = url::Url::parse(u) else { return false };
    if url.scheme() != "https" {
        return false;
    }
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    ["nexusmods.com", "nexus-cdn.com"]
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

/// Strip any directory parts and odd characters from a server-supplied name.
pub fn safe_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .map(|c| if c.is_control() || c == ':' { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim_start_matches('.').trim().to_string();
    if cleaned.is_empty() { "download.bin".into() } else { cleaned }
}

impl Client {
    pub fn new(api_key: &str) -> Result<Self> {
        Self::with_base(api_key, API_BASE)
    }

    pub fn with_base(api_key: &str, base: &str) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .user_agent(format!("{APP_NAME}/{APP_VERSION} (Linux)"))
            .https_only(base.starts_with("https://"))
            .connect_timeout(Duration::from_secs(20))
            .timeout(None)
            .build()?;
        Ok(Self { http, api_key: api_key.trim().to_string(), base: base.trim_end_matches('/').to_string() })
    }

    fn get<T: for<'de> Deserialize<'de>>(&self, path: &str, query: &[(&str, String)]) -> Result<T> {
        let resp = self
            .http
            .get(format!("{}{}", self.base, path))
            .header("apikey", &self.api_key)
            .header("Application-Name", APP_NAME)
            .header("Application-Version", APP_VERSION)
            .header("Accept", "application/json")
            .query(query)
            .timeout(Duration::from_secs(30))
            .send()?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            let msg = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(String::from))
                .unwrap_or(body);
            return Err(Error::Nexus(format!("{status}: {msg}")));
        }
        Ok(resp.json()?)
    }

    pub fn validate(&self) -> Result<User> {
        self.get("/users/validate.json", &[])
    }

    pub fn mod_info(&self, mod_id: i64) -> Result<ModInfo> {
        self.get(&format!("/games/{NEXUS_GAME_DOMAIN}/mods/{mod_id}.json"), &[])
    }

    pub fn mod_files(&self, mod_id: i64) -> Result<Vec<FileInfo>> {
        let r: FilesResponse = self.get(&format!("/games/{NEXUS_GAME_DOMAIN}/mods/{mod_id}/files.json"), &[])?;
        Ok(r.files)
    }

    pub fn file_info(&self, mod_id: i64, file_id: i64) -> Result<FileInfo> {
        self.get(&format!("/games/{NEXUS_GAME_DOMAIN}/mods/{mod_id}/files/{file_id}.json"), &[])
    }

    /// Premium users can call this without `key`/`expires`; free users need
    /// the values from an nxm:// link.
    pub fn download_links(&self, mod_id: i64, file_id: i64, key: Option<&str>, expires: Option<i64>) -> Result<Vec<DownloadLink>> {
        let mut q = Vec::new();
        if let (Some(k), Some(e)) = (key, expires) {
            q.push(("key", k.to_string()));
            q.push(("expires", e.to_string()));
        }
        self.get(&format!("/games/{NEXUS_GAME_DOMAIN}/mods/{mod_id}/files/{file_id}/download_link.json"), &q)
    }

    pub fn md5_search(&self, md5: &str) -> Result<Vec<Md5Hit>> {
        self.get(&format!("/games/{NEXUS_GAME_DOMAIN}/mods/md5_search/{md5}.json"), &[])
    }

    /// Download a mod file into `dest_dir` and verify it against Nexus.
    /// `progress` gets (bytes_done, bytes_total).
    pub fn download(
        &self,
        mod_id: i64,
        file_id: i64,
        key: Option<&str>,
        expires: Option<i64>,
        dest_dir: &Path,
        mut progress: impl FnMut(u64, u64),
    ) -> Result<Downloaded> {
        let info = self.file_info(mod_id, file_id)?;
        let links = self.download_links(mod_id, file_id, key, expires)?;
        let link = links
            .iter()
            .find(|l| is_allowed_download_url(&l.uri))
            .ok_or_else(|| Error::Nexus("no download link on an allowed Nexus host".into()))?;

        let file_name = safe_file_name(&info.file_name);
        std::fs::create_dir_all(dest_dir)?;
        let final_path = dest_dir.join(format!("{mod_id}-{file_id}-{file_name}"));
        let part_path = final_path.with_extension("part");

        let mut resp = self.http.get(&link.uri).send()?;
        // Redirects must stay on Nexus' hosts too.
        if !is_allowed_download_url(resp.url().as_str()) {
            return Err(Error::Nexus(format!("download redirected to untrusted host {}", resp.url())));
        }
        if !resp.status().is_success() {
            return Err(Error::Nexus(format!("download failed: {}", resp.status())));
        }
        let expected = info.size_in_bytes.or(resp.content_length()).unwrap_or(0);
        // Never write more than the advertised size (+1 KiB slack).
        let cap = if expected > 0 { expected + 1024 } else { 64 * 1024 * 1024 * 1024 };
        let mut out = std::fs::File::create(&part_path)?;
        let mut buf = vec![0u8; 1 << 16];
        let mut done = 0u64;
        loop {
            let n = resp.read(&mut buf)?;
            if n == 0 {
                break;
            }
            done += n as u64;
            if done > cap {
                drop(out);
                let _ = std::fs::remove_file(&part_path);
                return Err(Error::Integrity("download is larger than Nexus says it should be".into()));
            }
            out.write_all(&buf[..n])?;
            progress(done, expected);
        }
        out.sync_all()?;
        drop(out);
        if expected > 0 && done != expected {
            let _ = std::fs::remove_file(&part_path);
            return Err(Error::Integrity(format!("size mismatch: got {done} bytes, expected {expected}")));
        }

        let (sha256, md5) = hash::file_digests(&part_path)?;
        let verified = match self.md5_search(&md5) {
            Ok(hits) => {
                let ok = hits.iter().any(|h| h.mod_.mod_id == mod_id && h.file_details.file_id == file_id);
                if !ok {
                    let _ = std::fs::remove_file(&part_path);
                    return Err(Error::Integrity(
                        "Nexus does not recognise this file's MD5 for the requested mod; discarded".into(),
                    ));
                }
                true
            }
            // md5_search returns 404 for unknown hashes but can also be
            // briefly unavailable; keep the file, flagged unverified.
            Err(Error::Nexus(msg)) if msg.starts_with("404") => {
                let _ = std::fs::remove_file(&part_path);
                return Err(Error::Integrity("Nexus has no record of this file's MD5; discarded".into()));
            }
            Err(_) => false,
        };
        std::fs::rename(&part_path, &final_path)?;
        Ok(Downloaded {
            path: final_path,
            file_name,
            sha256,
            md5,
            size: done,
            verified,
            virus_scan_url: info.external_virus_scan_url,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nxm_links() {
        let l = NxmLink::parse("nxm://cyberpunk2077/mods/107/files/94235?key=abc&expires=1700000000&user_id=42").unwrap();
        assert_eq!(l.mod_id, 107);
        assert_eq!(l.file_id, 94235);
        assert_eq!(l.key.as_deref(), Some("abc"));
        assert_eq!(l.expires, Some(1700000000));
        assert!(NxmLink::parse("nxm://skyrimspecialedition/mods/1/files/2").is_err());
        assert!(NxmLink::parse("https://cyberpunk2077/mods/1/files/2").is_err());
    }

    #[test]
    fn restricts_download_hosts() {
        assert!(is_allowed_download_url("https://supporter-files.nexus-cdn.com/3333/107/x.zip?md5=1"));
        assert!(is_allowed_download_url("https://cf-files.nexusmods.com/x.7z"));
        assert!(!is_allowed_download_url("http://cf-files.nexusmods.com/x.7z"));
        assert!(!is_allowed_download_url("https://nexusmods.com.evil.example/x.7z"));
        assert!(!is_allowed_download_url("https://evilnexusmods.com/x.7z"));
    }

    #[test]
    fn sanitizes_file_names() {
        assert_eq!(safe_file_name("../../.bashrc"), "bashrc");
        assert_eq!(safe_file_name("C:\\x\\Mod 1.0.zip"), "Mod 1.0.zip");
        assert_eq!(safe_file_name(""), "download.bin");
    }
}
