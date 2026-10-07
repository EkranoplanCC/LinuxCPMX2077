//! Nexus Mods API v1 client using the user's personal API key.
//!
//! Free accounts can't request download links directly; they get a one-time
//! `key`/`expires` pair by clicking "Mod Manager Download" on the website,
//! which opens an `nxm://` link that this app handles. Premium accounts can
//! download straight from the app.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::nexus_cache::{RequestLog, RequestRecord, ResponseCache, Served, loggable_endpoint, started_ms};
use crate::{APP_NAME, APP_VERSION, Error, NEXUS_GAME_DOMAIN, Result, hash};

pub const API_BASE: &str = "https://api.nexusmods.com/v1";
pub const GRAPHQL_URL: &str = "https://api.nexusmods.com/v2/graphql";

/// Browsing stops when fewer than this many API requests are left, so the
/// remaining quota still covers downloads and checksum verification.
pub const BROWSE_RESERVE: u32 = 25;

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
    pub created_timestamp: Option<i64>,
    pub uploaded_by: Option<String>,
    pub endorsement_count: Option<i64>,
    pub mod_downloads: Option<i64>,
    pub mod_unique_downloads: Option<i64>,
    pub category_id: Option<i64>,
    /// BBCode/HTML from the mod page. Never render as HTML.
    pub description: Option<String>,
    #[serde(default = "yes")]
    pub available: bool,
}

fn yes() -> bool {
    true
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

/// Nexus' API quota as last reported in the `X-RL-*` response headers.
/// Requests are allowed while either the daily or the hourly allowance has
/// requests left.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RateLimit {
    pub hourly_limit: Option<u32>,
    pub hourly_remaining: Option<u32>,
    pub hourly_reset: Option<i64>,
    pub daily_limit: Option<u32>,
    pub daily_remaining: Option<u32>,
    pub daily_reset: Option<i64>,
    /// Unix time before which no request is sent (after a 429 or when both
    /// allowances are used up).
    pub blocked_until: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// Sign-in, download links and checksum checks: allowed until the quota
    /// is actually exhausted.
    Essential,
    /// Search and lists: stop at [`BROWSE_RESERVE`].
    Browse,
}

pub(crate) fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl RateLimit {
    fn header<T: std::str::FromStr>(h: &reqwest::header::HeaderMap, name: &str) -> Option<T> {
        h.get(name)?.to_str().ok()?.trim().parse().ok()
    }

    fn reset(h: &reqwest::header::HeaderMap, name: &str) -> Option<i64> {
        parse_time(h.get(name)?.to_str().ok()?)
    }

    pub fn update(&mut self, h: &reqwest::header::HeaderMap, now: i64) {
        if let Some(v) = Self::header(h, "x-rl-hourly-limit") {
            self.hourly_limit = Some(v);
        }
        if let Some(v) = Self::header(h, "x-rl-hourly-remaining") {
            self.hourly_remaining = Some(v);
        }
        if let Some(v) = Self::reset(h, "x-rl-hourly-reset") {
            self.hourly_reset = Some(v);
        }
        if let Some(v) = Self::header(h, "x-rl-daily-limit") {
            self.daily_limit = Some(v);
        }
        if let Some(v) = Self::header(h, "x-rl-daily-remaining") {
            self.daily_remaining = Some(v);
        }
        if let Some(v) = Self::reset(h, "x-rl-daily-reset") {
            self.daily_reset = Some(v);
        }
        if self.hourly_remaining == Some(0) && self.daily_remaining == Some(0) {
            // The hourly allowance comes back first.
            let until = self.hourly_reset.filter(|t| *t > now).unwrap_or(now + 3600);
            self.blocked_until = Some(until);
        } else if self.blocked_until.is_some_and(|t| t <= now) {
            self.blocked_until = None;
        }
    }

    /// Called on HTTP 429.
    pub fn throttled(&mut self, h: &reqwest::header::HeaderMap, now: i64) {
        self.update(h, now);
        let retry = Self::header::<i64>(h, "retry-after").map(|s| now + s.clamp(1, 86400));
        let until = retry
            .or(self.blocked_until.filter(|t| *t > now))
            .or(self.hourly_reset.filter(|t| *t > now))
            .unwrap_or(now + 60);
        self.blocked_until = Some(until);
    }

    /// Requests left before Nexus starts refusing, if known.
    pub fn remaining(&self) -> Option<u32> {
        match (self.hourly_remaining, self.daily_remaining) {
            (None, None) => None,
            (h, d) => Some(h.unwrap_or(0).max(d.unwrap_or(0))),
        }
    }

    pub fn check(&self, prio: Priority, now: i64) -> Result<()> {
        if let Some(until) = self.blocked_until.filter(|t| *t > now) {
            return Err(Error::Nexus(format!(
                "Nexus API request limit reached; try again in {}",
                fmt_wait(until - now)
            )));
        }
        if prio == Priority::Browse && self.remaining().is_some_and(|r| r <= BROWSE_RESERVE) {
            return Err(Error::Nexus(format!(
                "Only {} Nexus API requests left; browsing is paused to keep them for downloads",
                self.remaining().unwrap_or(0)
            )));
        }
        Ok(())
    }
}

fn fmt_wait(secs: i64) -> String {
    if secs < 90 {
        format!("{secs} s")
    } else if secs < 5400 {
        format!("{} min", (secs + 59) / 60)
    } else {
        format!("{} h", (secs + 1799) / 3600)
    }
}

/// Parse the timestamps Nexus uses: `2024-02-01 13:00:00 +0000` in rate-limit
/// headers and RFC 3339 (`2024-02-01T13:00:00Z`, fractional seconds allowed)
/// in GraphQL. Returns Unix seconds.
pub fn parse_time(s: &str) -> Option<i64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !(b[10] == b'T' || b[10] == b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let mut rest = s[19..].trim_start();
    if let Some(r) = rest.strip_prefix('.') {
        rest = r.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    let rest = rest.trim();
    let offset = match rest {
        "" | "Z" | "z" | "UTC" => 0,
        _ => {
            let r = rest.replace(':', "");
            let (sign, digits) = match r.as_bytes().first()? {
                b'+' => (1, &r[1..]),
                b'-' => (-1, &r[1..]),
                _ => return None,
            };
            if digits.len() != 4 {
                return None;
            }
            let oh: i64 = digits[..2].parse().ok()?;
            let om: i64 = digits[2..].parse().ok()?;
            sign * (oh * 3600 + om * 60)
        }
    };
    // Days from civil date (Howard Hinnant's algorithm).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + h * 3600 + mi * 60 + se - offset)
}

/// State that outlives a single [`Client`]: the quota, the browse response
/// cache (so flipping between lists doesn't spend requests) and the log of
/// recent requests.
#[derive(Default)]
pub struct Shared {
    pub(crate) rate: Mutex<RateLimit>,
    pub(crate) cache: ResponseCache,
    pub(crate) log: RequestLog,
}

impl Shared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn rate(&self) -> RateLimit {
        self.rate.lock().unwrap().clone()
    }

    /// Keep browse responses in `dir` too, so pages open instantly (and
    /// without Nexus) after a restart.
    pub fn set_cache_dir(&self, dir: PathBuf) -> std::io::Result<()> {
        self.cache.set_dir(dir)
    }

    pub fn clear_cache(&self) {
        self.cache.clear();
    }

    /// Saved responses on disk: (count, bytes).
    pub fn cache_usage(&self) -> (usize, u64) {
        self.cache.disk_usage()
    }

    /// API requests made since the request with id `after`, oldest first.
    pub fn requests_since(&self, after: u64) -> Vec<RequestRecord> {
        self.log.since(after)
    }

    pub fn clear_request_log(&self) {
        self.log.clear();
    }
}

pub struct Client {
    pub(crate) http: reqwest::blocking::Client,
    pub(crate) api_key: String,
    base: String,
    pub(crate) graphql: String,
    pub(crate) shared: Arc<Shared>,
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
        Self::with_shared(api_key, Shared::new())
    }

    /// A client that shares quota tracking and cache with other clients.
    pub fn with_shared(api_key: &str, shared: Arc<Shared>) -> Result<Self> {
        Self::with_endpoints(api_key, API_BASE, GRAPHQL_URL, shared)
    }

    pub fn with_base(api_key: &str, base: &str) -> Result<Self> {
        let graphql = format!("{}/v2/graphql", base.trim_end_matches('/').trim_end_matches("/v1"));
        Self::with_endpoints(api_key, base, &graphql, Shared::new())
    }

    pub fn with_endpoints(api_key: &str, base: &str, graphql: &str, shared: Arc<Shared>) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .user_agent(format!("{APP_NAME}/{APP_VERSION} (Linux)"))
            .https_only(base.starts_with("https://"))
            .connect_timeout(Duration::from_secs(20))
            .timeout(None)
            .build()?;
        Ok(Self {
            http,
            api_key: api_key.trim().to_string(),
            base: base.trim_end_matches('/').to_string(),
            graphql: graphql.to_string(),
            shared,
        })
    }

    pub fn rate(&self) -> RateLimit {
        self.shared.rate()
    }

    /// Send an API request, honouring and recording Nexus' rate limits.
    pub(crate) fn send_api(&self, prio: Priority, req: reqwest::blocking::RequestBuilder) -> Result<reqwest::blocking::Response> {
        self.send_api_as(prio, req, None)
    }

    /// [`Self::send_api`], with a name for the request in the debug log.
    pub(crate) fn send_api_as(
        &self,
        prio: Priority,
        req: reqwest::blocking::RequestBuilder,
        label: Option<&str>,
    ) -> Result<reqwest::blocking::Response> {
        let (method, endpoint) = req
            .try_clone()
            .and_then(|r| r.build().ok())
            .map(|r| (r.method().to_string(), loggable_endpoint(r.url())))
            .unwrap_or_default();
        let started = Instant::now();
        let mut record = RequestRecord {
            id: 0,
            at_ms: started_ms(),
            method,
            endpoint,
            label: label.map(String::from),
            priority: if prio == Priority::Browse { "browse" } else { "essential" },
            served: Served::Network,
            status: None,
            duration_ms: 0,
            bytes: None,
            error: None,
            hourly_remaining: None,
            daily_remaining: None,
        };
        let out = self.send_api_inner(prio, req);
        record.duration_ms = started.elapsed().as_millis() as u64;
        let rate = self.rate();
        record.hourly_remaining = rate.hourly_remaining;
        record.daily_remaining = rate.daily_remaining;
        match &out {
            Ok(resp) => {
                record.status = Some(resp.status().as_u16());
                record.bytes = resp.content_length();
            }
            Err(Error::Nexus(msg)) if msg.starts_with("429") => {
                record.status = Some(429);
                record.error = Some(msg.clone());
            }
            Err(e @ Error::Nexus(_)) => {
                record.served = Served::Held;
                record.error = Some(e.to_string());
            }
            Err(e) => record.error = Some(e.to_string()),
        }
        self.shared.log.push(record);
        out
    }

    fn send_api_inner(&self, prio: Priority, req: reqwest::blocking::RequestBuilder) -> Result<reqwest::blocking::Response> {
        self.shared.rate.lock().unwrap().check(prio, now_unix())?;
        let mut req = req
            .header("Application-Name", APP_NAME)
            .header("Application-Version", APP_VERSION)
            .header("Accept", "application/json")
            .timeout(Duration::from_secs(30));
        if !self.api_key.is_empty() {
            req = req.header("apikey", &self.api_key);
        }
        // Nexus closes idle keep-alive connections; a request sent on one
        // fails before it reaches the server, so it is safe to send again.
        let retry = req.try_clone();
        let resp = match (req.send(), retry) {
            (Err(e), Some(again)) if e.is_request() && !e.is_timeout() => {
                log::warn!("Nexus request failed ({e}); retrying once");
                again.send()?
            }
            (r, _) => r?,
        };
        let mut rate = self.shared.rate.lock().unwrap();
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            rate.throttled(resp.headers(), now_unix());
            let wait = rate.blocked_until.map(|t| fmt_wait(t - now_unix())).unwrap_or_default();
            return Err(Error::Nexus(format!("429: Nexus API request limit reached; try again in {wait}")));
        }
        rate.update(resp.headers(), now_unix());
        Ok(resp)
    }

    fn get<T: for<'de> Deserialize<'de>>(&self, path: &str, query: &[(&str, String)]) -> Result<T> {
        self.get_with(Priority::Essential, path, query)
    }

    pub(crate) fn get_with<T: for<'de> Deserialize<'de>>(&self, prio: Priority, path: &str, query: &[(&str, String)]) -> Result<T> {
        let resp = self.send_api(prio, self.http.get(format!("{}{}", self.base, path)).query(query))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            let msg = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("message").or_else(|| v.get("error")).and_then(|m| m.as_str()).map(String::from))
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
    /// Nexus' file hosts, or the API server itself (a test server when the
    /// client was built with another base).
    fn allowed_download(&self, u: &str) -> bool {
        if is_allowed_download_url(u) {
            return true;
        }
        match (url::Url::parse(u), url::Url::parse(&self.base)) {
            (Ok(u), Ok(b)) => u.scheme() == b.scheme() && u.host_str() == b.host_str() && u.port_or_known_default() == b.port_or_known_default(),
            _ => false,
        }
    }

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
            .find(|l| self.allowed_download(&l.uri))
            .ok_or_else(|| Error::Nexus("no download link on an allowed Nexus host".into()))?;

        let file_name = safe_file_name(&info.file_name);
        std::fs::create_dir_all(dest_dir)?;
        let final_path = dest_dir.join(format!("{mod_id}-{file_id}-{file_name}"));
        let part_path = final_path.with_extension("part");

        let mut resp = self.http.get(&link.uri).send()?;
        // Redirects must stay on Nexus' hosts too.
        if !self.allowed_download(resp.url().as_str()) {
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
    use crate::testutil::serve;

    #[test]
    fn free_download_with_nxm_key_is_fetched_and_verified() {
        // md5("hello world")
        let md5 = "5eb63bbbe01eeed093cb22bb8f5acdc3";
        let (base, seen) = serve(vec![
            ("/download_link.json", 200, vec![], r#"[{"name":"CDN","short_name":"cdn","URI":"{addr}/cdn/f.zip"},{"name":"x","short_name":"x","URI":"https://evil.example/f.zip"}]"#.into()),
            ("/games/cyberpunk2077/mods/7/files/9.json", 200, vec![], r#"{"file_id":9,"name":"Main","file_name":"f.zip","size_in_bytes":11}"#.into()),
            ("/cdn/f.zip", 200, vec![], "hello world".into()),
            ("/md5_search/", 200, vec![], format!(r#"[{{"mod":{{"mod_id":7,"name":"M"}},"file_details":{{"file_id":9,"md5":"{md5}"}}}}]"#)),
        ]);
        let c = Client::with_base("k", &base).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let d = c.download(7, 9, Some("abc"), Some(1_900_000_000), dir.path(), |_, _| {}).unwrap();
        assert!(d.verified);
        assert_eq!(d.md5, md5);
        assert_eq!(std::fs::read_to_string(&d.path).unwrap(), "hello world");
        let seen = seen.lock().unwrap();
        let link = seen.iter().find(|s| s.line.contains("download_link.json")).unwrap();
        assert!(link.line.contains("key=abc") && link.line.contains("expires=1900000000"), "{}", link.line);
        assert!(!is_allowed_download_url(&format!("{base}/cdn/f.zip")), "only the client's own server is let through");
    }

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
