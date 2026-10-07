//! What the Nexus client remembers between requests: browse responses (in
//! memory and, once a folder is set, on disk so they survive a restart) and a
//! log of recent API requests for the debug view.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Saved responses older than this are deleted.
pub const DISK_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
const DISK_MAX_FILES: usize = 3000;
const MEMORY_MAX: usize = 300;
const LOG_MAX: usize = 300;

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// A cached response and when it was fetched (Unix seconds).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub fetched_at: i64,
    pub value: Value,
}

#[derive(Serialize, Deserialize)]
struct DiskEntry {
    key: String,
    fetched_at: i64,
    value: Value,
}

#[derive(Default)]
pub struct ResponseCache {
    memory: Mutex<HashMap<String, Entry>>,
    dir: Mutex<Option<PathBuf>>,
}

impl ResponseCache {
    /// Keep responses in `dir` as well, and drop the ones there that are too
    /// old to be worth showing.
    pub fn set_dir(&self, dir: PathBuf) -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        prune(&dir, DISK_MAX_AGE, DISK_MAX_FILES);
        *self.dir.lock().unwrap() = Some(dir);
        Ok(())
    }

    fn file(&self, key: &str) -> Option<PathBuf> {
        let dir = self.dir.lock().unwrap().clone()?;
        Some(dir.join(format!("{}.json", hex::encode(Sha256::digest(key.as_bytes())))))
    }

    /// The newest copy of `key`, however old.
    pub fn get(&self, key: &str) -> Option<Entry> {
        if let Some(e) = self.memory.lock().unwrap().get(key) {
            return Some(e.clone());
        }
        let path = self.file(key)?;
        let d: DiskEntry = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
        // A hash collision or a hand-edited file is just a miss.
        if d.key != key {
            return None;
        }
        let e = Entry { fetched_at: d.fetched_at, value: d.value };
        self.memory.lock().unwrap().insert(key.to_string(), e.clone());
        Some(e)
    }

    /// The copy of `key` if it was fetched less than `ttl` ago.
    pub fn fresh(&self, key: &str, ttl: Duration, now: i64) -> Option<Entry> {
        self.get(key).filter(|e| now - e.fetched_at < ttl.as_secs() as i64 && e.fetched_at <= now + 60)
    }

    pub fn store(&self, key: String, value: Value, now: i64) {
        let entry = Entry { fetched_at: now, value };
        if let Some(path) = self.file(&key) {
            let d = DiskEntry { key: key.clone(), fetched_at: now, value: entry.value.clone() };
            if let Err(e) = write_atomic(&path, &serde_json::to_vec(&d).unwrap_or_default()) {
                log::warn!("could not save Nexus response to {}: {e}", path.display());
            }
        }
        let mut m = self.memory.lock().unwrap();
        if m.len() >= MEMORY_MAX {
            // Drop the oldest half; the disk still has them.
            let mut ages: Vec<i64> = m.values().map(|e| e.fetched_at).collect();
            ages.sort_unstable();
            let cut = ages[ages.len() / 2];
            m.retain(|_, e| e.fetched_at > cut);
        }
        m.insert(key, entry);
    }

    pub fn clear(&self) {
        self.memory.lock().unwrap().clear();
        if let Some(dir) = self.dir.lock().unwrap().as_deref() {
            prune(dir, Duration::ZERO, 0);
        }
    }

    /// Number of saved responses and their total size in bytes.
    pub fn disk_usage(&self) -> (usize, u64) {
        let Some(dir) = self.dir.lock().unwrap().clone() else { return (0, 0) };
        let files = cache_files(&dir);
        (files.len(), files.iter().map(|(_, _, len)| len).sum())
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// (path, modified, size) of every saved response in `dir`.
fn cache_files(dir: &Path) -> Vec<(PathBuf, SystemTime, u64)> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    rd.filter_map(|e| {
        let e = e.ok()?;
        let path = e.path();
        let name = path.file_name()?.to_str()?;
        // Only our own files: 64 hex chars + .json, or a leftover temp file.
        let stem = name.split('.').next()?;
        if stem.len() != 64 || !stem.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let meta = e.metadata().ok().filter(|m| m.is_file())?;
        Some((path, meta.modified().unwrap_or(UNIX_EPOCH), meta.len()))
    })
    .collect()
}

fn prune(dir: &Path, max_age: Duration, max_files: usize) {
    let mut files = cache_files(dir);
    let now = SystemTime::now();
    files.sort_by_key(|(_, at, _)| std::cmp::Reverse(*at));
    for (i, (path, at, _)) in files.iter().enumerate() {
        let old = now.duration_since(*at).unwrap_or_default() >= max_age;
        if old || i >= max_files || path.extension().is_some_and(|x| x != "json") {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Where a request's answer came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Served {
    /// Sent to Nexus.
    Network,
    /// Answered from a recent cached copy without asking Nexus.
    Cache,
    /// Nexus couldn't be asked (or failed), so an older saved copy was shown.
    Saved,
    /// Not sent: the app was holding back to protect the quota.
    Held,
}

/// One API call, as the debug view shows it. Never holds the API key or the
/// one-time download key of an nxm link.
#[derive(Debug, Clone, Serialize)]
pub struct RequestRecord {
    pub id: u64,
    /// Unix milliseconds when it started.
    pub at_ms: i64,
    pub method: String,
    /// Path and query on the API host, e.g. `/v1/games/cyberpunk2077.json`.
    pub endpoint: String,
    /// GraphQL operation name, or what the request was for.
    pub label: Option<String>,
    /// `browse` or `essential`.
    pub priority: &'static str,
    pub served: Served,
    pub status: Option<u16>,
    pub duration_ms: u64,
    pub bytes: Option<u64>,
    pub error: Option<String>,
    pub hourly_remaining: Option<u32>,
    pub daily_remaining: Option<u32>,
}

#[derive(Default)]
pub struct RequestLog {
    entries: Mutex<VecDeque<RequestRecord>>,
    next: Mutex<u64>,
}

impl RequestLog {
    pub fn push(&self, mut r: RequestRecord) {
        let mut next = self.next.lock().unwrap();
        *next += 1;
        r.id = *next;
        if r.at_ms == 0 {
            r.at_ms = now_ms();
        }
        let mut e = self.entries.lock().unwrap();
        if e.len() >= LOG_MAX {
            e.pop_front();
        }
        e.push_back(r);
    }

    /// Requests newer than `after` (an id), oldest first.
    pub fn since(&self, after: u64) -> Vec<RequestRecord> {
        self.entries.lock().unwrap().iter().filter(|r| r.id > after).cloned().collect()
    }

    pub fn clear(&self) {
        self.entries.lock().unwrap().clear();
    }
}

pub(crate) fn started_ms() -> i64 {
    now_ms()
}

/// The part of a request URL that is safe to show: path and query on the
/// API host, without any `key`/`expires` from an nxm link.
pub fn loggable_endpoint(url: &url::Url) -> String {
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| {
            let v = if matches!(k.as_ref(), "key" | "expires" | "apikey") { "…".into() } else { v.into_owned() };
            (k.into_owned(), v)
        })
        .collect();
    let mut out = url.path().to_string();
    if !pairs.is_empty() {
        out.push('?');
        out.push_str(&pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn responses_survive_a_restart_and_expire() {
        let dir = tempfile::tempdir().unwrap();
        let a = ResponseCache::default();
        a.set_dir(dir.path().into()).unwrap();
        a.store("v1:/x".into(), json!({"n": 1}), 1000);
        assert_eq!(a.fresh("v1:/x", Duration::from_secs(300), 1100).unwrap().value["n"], 1);

        // A new process reads it back from disk.
        let b = ResponseCache::default();
        b.set_dir(dir.path().into()).unwrap();
        let e = b.get("v1:/x").unwrap();
        assert_eq!((e.fetched_at, e.value["n"].as_i64()), (1000, Some(1)));
        assert!(b.fresh("v1:/x", Duration::from_secs(300), 1400).is_none(), "too old to count as fresh");
        assert!(b.get("v1:/y").is_none());
        assert_eq!(b.disk_usage().0, 1);

        b.clear();
        assert!(b.get("v1:/x").is_none());
        assert_eq!(b.disk_usage(), (0, 0));
        // Other files in the folder are left alone.
        std::fs::write(dir.path().join("notes.txt"), "keep").unwrap();
        b.clear();
        assert!(dir.path().join("notes.txt").exists());
    }

    #[test]
    fn memory_stays_bounded() {
        let c = ResponseCache::default();
        for i in 0..(MEMORY_MAX as i64 + 50) {
            c.store(format!("k{i}"), json!(i), i);
        }
        assert!(c.memory.lock().unwrap().len() <= MEMORY_MAX);
        assert!(c.get(&format!("k{}", MEMORY_MAX + 49)).is_some(), "newest kept");
    }

    #[test]
    fn log_is_bounded_and_hides_keys() {
        let log = RequestLog::default();
        let rec = |e: &str| RequestRecord {
            id: 0,
            at_ms: 0,
            method: "GET".into(),
            endpoint: e.into(),
            label: None,
            priority: "browse",
            served: Served::Network,
            status: Some(200),
            duration_ms: 1,
            bytes: None,
            error: None,
            hourly_remaining: None,
            daily_remaining: None,
        };
        for i in 0..(LOG_MAX + 5) {
            log.push(rec(&format!("/{i}")));
        }
        let all = log.since(0);
        assert_eq!(all.len(), LOG_MAX);
        assert_eq!(all.last().unwrap().id, LOG_MAX as u64 + 5);
        assert_eq!(log.since(all.last().unwrap().id - 2).len(), 2);

        let u = url::Url::parse("https://api.nexusmods.com/v1/games/x/mods/1/files/2/download_link.json?key=SECRET&expires=99&a=b").unwrap();
        let shown = loggable_endpoint(&u);
        assert!(!shown.contains("SECRET") && !shown.contains("99"), "{shown}");
        assert!(shown.ends_with("a=b"), "{shown}");
    }
}
