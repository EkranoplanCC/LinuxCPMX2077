//! A running log of what the app does on this machine (downloads, archive
//! extractions, files copied into the game, moved or deleted, setup fixes),
//! for the debug terminal. Nexus API requests have their own log
//! ([`crate::nexus_cache::RequestLog`]); the terminal shows both together.
//!
//! It is process-wide, kept in memory only, and bounded. Entries never hold
//! an API key or the signed part of a download link: pass URLs through
//! [`safe_url`] first.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{LazyLock, Mutex};

use serde::Serialize;

/// How many entries are kept; the oldest go first.
const MAX: usize = 5000;

/// What kind of operation an entry is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A request to a web API other than Nexus' (Nexus has its own log).
    Api,
    /// A file fetched from the internet.
    Download,
    /// A downloaded file checked against what its source published.
    Verify,
    /// An archive unpacked.
    Extract,
    /// A file written into the game folder.
    Copy,
    /// An original game file saved before a mod replaced it.
    Backup,
    /// A file put back (an original game file, or another mod's copy).
    Restore,
    /// A file or folder moved or renamed.
    Move,
    /// A file or folder deleted.
    Delete,
    /// A Linux setup fix, or anything else changed outside the game folder.
    Setup,
    /// A summary line (install finished, mod disabled...).
    Info,
    /// Something failed.
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub id: u64,
    /// Unix milliseconds.
    pub at_ms: i64,
    pub kind: Kind,
    pub message: String,
    /// The file or folder it is about, when there is one.
    pub path: Option<String>,
}

#[derive(Default)]
struct Log {
    entries: VecDeque<Entry>,
    next: u64,
}

static LOG: LazyLock<Mutex<Log>> = LazyLock::new(Mutex::default);

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Record an operation.
pub fn record(kind: Kind, message: impl Into<String>) {
    push(kind, message.into(), None);
}

/// Record an operation on `path`.
pub fn record_path(kind: Kind, message: impl Into<String>, path: &Path) {
    push(kind, message.into(), Some(path.to_string_lossy().into_owned()));
}

fn push(kind: Kind, message: String, path: Option<String>) {
    log::debug!(target: "cpmx::activity", "{kind:?} {message} {}", path.as_deref().unwrap_or(""));
    let mut log = LOG.lock().unwrap_or_else(|e| e.into_inner());
    log.next += 1;
    let id = log.next;
    if log.entries.len() >= MAX {
        log.entries.pop_front();
    }
    log.entries.push_back(Entry { id, at_ms: now_ms(), kind, message, path });
}

/// Entries newer than the one with id `after`, oldest first.
pub fn since(after: u64) -> Vec<Entry> {
    let log = LOG.lock().unwrap_or_else(|e| e.into_inner());
    log.entries.iter().filter(|e| e.id > after).cloned().collect()
}

pub fn clear() {
    LOG.lock().unwrap_or_else(|e| e.into_inner()).entries.clear();
}

/// A URL without its query or fragment, so signed download links and keys
/// never reach the log.
pub fn safe_url(u: &str) -> String {
    match url::Url::parse(u) {
        Ok(mut url) => {
            let signed = url.query().is_some();
            url.set_query(None);
            url.set_fragment(None);
            let _ = url.set_username("");
            let _ = url.set_password(None);
            if signed { format!("{url}?…") } else { url.to_string() }
        }
        Err(_) => "(unreadable URL)".into(),
    }
}

/// Bytes as a short size for log lines.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[unit]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_in_order_and_hides_signed_links() {
        // The log is shared by every test in the process, so look only at
        // this test's entries.
        let tag = format!("activity-test-{}", uuid::Uuid::new_v4());
        let start = since(0).last().map_or(0, |e| e.id);
        record(Kind::Extract, format!("{tag} one"));
        record_path(Kind::Copy, format!("{tag} two"), Path::new("/game/archive/pc/mod/x.archive"));
        let mine: Vec<Entry> = since(start).into_iter().filter(|e| e.message.starts_with(&tag)).collect();
        assert_eq!(mine.len(), 2);
        assert!(mine[0].id < mine[1].id);
        assert_eq!(mine[1].kind, Kind::Copy);
        assert_eq!(mine[1].path.as_deref(), Some("/game/archive/pc/mod/x.archive"));

        let shown = safe_url("https://user:pw@cf-files.nexusmods.com/cdn/3333/1/x.zip?md5=SECRET&expires=1#f");
        assert_eq!(shown, "https://cf-files.nexusmods.com/cdn/3333/1/x.zip?…");
        assert_eq!(safe_url("https://api.github.com/repos/a/b"), "https://api.github.com/repos/a/b");
        assert_eq!(size(512), "512 B");
        assert_eq!(size(3 * 1024 * 1024), "3.0 MB");
    }
}
