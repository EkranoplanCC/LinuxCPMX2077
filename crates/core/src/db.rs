//! SQLite library of games, installed mods, the files each mod owns and
//! downloaded archives.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::game::GameInstall;

const SCHEMA: &str = r#"
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS games (
    id                  INTEGER PRIMARY KEY,
    path                TEXT NOT NULL UNIQUE,
    store               TEXT NOT NULL,
    build_id            TEXT,
    exe_file_version    TEXT,
    exe_product_version TEXT,
    last_seen           TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE IF NOT EXISTS mods (
    id              INTEGER PRIMARY KEY,
    game_id         INTEGER NOT NULL REFERENCES games(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    version         TEXT,
    source          TEXT NOT NULL,          -- 'manual' | 'nexus'
    nexus_mod_id    INTEGER,
    nexus_file_id   INTEGER,
    archive_name    TEXT NOT NULL,
    archive_sha256  TEXT NOT NULL,
    archive_md5     TEXT NOT NULL,
    game_build_id   TEXT,                   -- game build when installed
    game_version    TEXT,
    status          TEXT NOT NULL DEFAULT 'installed', -- 'installed' (enabled) | 'disabled'
    installed_at    TEXT NOT NULL DEFAULT (datetime('now'))
    -- More columns are added by MIGRATIONS.
);
CREATE TABLE IF NOT EXISTS mod_files (
    mod_id      INTEGER NOT NULL REFERENCES mods(id) ON DELETE CASCADE,
    rel_path    TEXT NOT NULL,              -- path inside the game dir
    staged_path TEXT NOT NULL,              -- path inside the mod's staging dir
    sha256      TEXT NOT NULL,
    size        INTEGER NOT NULL,
    PRIMARY KEY (mod_id, rel_path)
);
CREATE INDEX IF NOT EXISTS mod_files_path ON mod_files (lower(rel_path));
CREATE TABLE IF NOT EXISTS backups (
    game_id     INTEGER NOT NULL REFERENCES games(id) ON DELETE CASCADE,
    rel_path    TEXT NOT NULL,
    backup_path TEXT NOT NULL,
    sha256      TEXT NOT NULL,
    PRIMARY KEY (game_id, rel_path)
);
-- Files a mod shipped that were changed after install (e.g. a config the mod
-- rewrote) and were left in the game dir when the mod was disabled.
CREATE TABLE IF NOT EXISTS kept_files (
    mod_id   INTEGER NOT NULL REFERENCES mods(id) ON DELETE CASCADE,
    rel_path TEXT NOT NULL,
    sha256   TEXT NOT NULL,
    PRIMARY KEY (mod_id, rel_path)
);
-- What each mod touches in the game (see analysis.rs).
CREATE TABLE IF NOT EXISTS touches (
    mod_id  INTEGER NOT NULL REFERENCES mods(id) ON DELETE CASCADE,
    kind    TEXT NOT NULL,
    key     TEXT NOT NULL,
    file    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS touches_mod ON touches (mod_id);
CREATE INDEX IF NOT EXISTS touches_key ON touches (kind, key);
CREATE TABLE IF NOT EXISTS indexed_mods (
    mod_id     INTEGER PRIMARY KEY REFERENCES mods(id) ON DELETE CASCADE,
    version    INTEGER NOT NULL
);
-- Resource hashes in the base game's archives, per game install.
CREATE TABLE IF NOT EXISTS base_resources (
    game_id INTEGER NOT NULL REFERENCES games(id) ON DELETE CASCADE,
    hash    INTEGER NOT NULL,
    PRIMARY KEY (game_id, hash)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS downloads (
    id             INTEGER PRIMARY KEY,
    nexus_mod_id   INTEGER,
    nexus_file_id  INTEGER,
    file_name      TEXT NOT NULL,
    path           TEXT NOT NULL,
    sha256         TEXT NOT NULL,
    md5            TEXT NOT NULL,
    size           INTEGER NOT NULL,
    verified       INTEGER NOT NULL DEFAULT 0, -- checksum matched the source's record
    downloaded_at  TEXT NOT NULL DEFAULT (datetime('now'))
    -- More columns are added by MIGRATIONS.
);
-- Nexus collections the user installed from and follows (see modpacks.rs).
CREATE TABLE IF NOT EXISTS tracked_collections (
    game_id         INTEGER NOT NULL REFERENCES games(id) ON DELETE CASCADE,
    slug            TEXT NOT NULL,
    name            TEXT NOT NULL,
    author          TEXT,
    revision        INTEGER,                -- the revision the user installed
    game_version    TEXT,
    latest_revision INTEGER,                -- newest revision seen on Nexus
    added_at        TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (game_id, slug)
);
-- The mod files of each tracked collection's installed revision.
CREATE TABLE IF NOT EXISTS collection_mods (
    game_id       INTEGER NOT NULL,
    slug          TEXT NOT NULL,
    position      INTEGER NOT NULL,
    nexus_mod_id  INTEGER NOT NULL,
    nexus_file_id INTEGER NOT NULL,
    mod_name      TEXT NOT NULL,
    file_name     TEXT,
    version       TEXT,
    optional      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (game_id, slug, nexus_mod_id, nexus_file_id),
    FOREIGN KEY (game_id, slug) REFERENCES tracked_collections (game_id, slug) ON DELETE CASCADE
);
-- The user's own mod tags, and which mod has which (several per mod). Mods
-- are keyed by where they came from (modpacks::mod_key), so tags survive
-- updates and reinstalls.
CREATE TABLE IF NOT EXISTS tags (
    name     TEXT PRIMARY KEY COLLATE NOCASE,
    color    TEXT,
    position INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS mod_tags (
    mod_key TEXT NOT NULL,
    tag     TEXT NOT NULL REFERENCES tags (name) ON DELETE CASCADE ON UPDATE CASCADE,
    PRIMARY KEY (mod_key, tag)
);
-- What a Nexus mod's page says it needs (JSON list of
-- nexus_browse::Requirement), cached between runs.
CREATE TABLE IF NOT EXISTS nexus_requirements (
    nexus_mod_id INTEGER PRIMARY KEY,
    requirements TEXT NOT NULL,
    fetched_at   INTEGER NOT NULL           -- unix seconds
);
-- Every game version the manager has seen, oldest first. A new row is added
-- when the game updates.
CREATE TABLE IF NOT EXISTS game_versions (
    id          INTEGER PRIMARY KEY,
    game_id     INTEGER NOT NULL REFERENCES games(id) ON DELETE CASCADE,
    version     TEXT,
    build_id    TEXT,
    first_seen  TEXT NOT NULL DEFAULT (datetime('now')),
    left_at     TEXT                    -- when the next version was first seen
);
CREATE INDEX IF NOT EXISTS game_versions_game ON game_versions (game_id, id);
-- The mods installed when the game moved on from a version.
CREATE TABLE IF NOT EXISTS game_version_mods (
    game_version_id INTEGER NOT NULL REFERENCES game_versions(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    version         TEXT,
    source          TEXT NOT NULL,
    nexus_mod_id    INTEGER,
    nexus_file_id   INTEGER,
    source_ref      TEXT,
    archive_name    TEXT NOT NULL,
    enabled         INTEGER NOT NULL
);
"#;

/// Columns added after v0.1, as (table, column, declaration). Applied to
/// existing libraries on open.
const MIGRATIONS: &[(&str, &str, &str)] = &[
    // Category shown and sorted on in the mod list.
    ("mods", "category", "TEXT"),
    // For sources other than Nexus: the source's id for the mod (e.g.
    // `owner/repo` on GitHub) and the file that was installed.
    ("mods", "source_ref", "TEXT"),
    ("mods", "source_file", "TEXT"),
    ("downloads", "source", "TEXT NOT NULL DEFAULT 'nexus'"),
    ("downloads", "source_ref", "TEXT"),
    ("downloads", "source_file", "TEXT"),
    ("downloads", "mod_name", "TEXT"),
    ("downloads", "version", "TEXT"),
    // Game version current when the file was downloaded.
    ("downloads", "game_version", "TEXT"),
    // How the file was verified, e.g. "MD5 matches Nexus".
    ("downloads", "checked", "TEXT"),
    // The mod's category, kept for installing the file later.
    ("downloads", "category", "TEXT"),
    // Release channel: 'stable' | 'beta' | 'nightly' (see downloads.rs).
    ("downloads", "channel", "TEXT"),
];

pub struct Db {
    pub conn: Connection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameRow {
    pub id: i64,
    pub path: String,
    pub store: String,
    pub build_id: Option<String>,
    pub exe_file_version: Option<String>,
    pub exe_product_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModRow {
    pub id: i64,
    pub game_id: i64,
    pub name: String,
    pub version: Option<String>,
    pub source: String,
    pub nexus_mod_id: Option<i64>,
    pub nexus_file_id: Option<i64>,
    pub archive_name: String,
    pub archive_sha256: String,
    pub archive_md5: String,
    pub game_build_id: Option<String>,
    pub game_version: Option<String>,
    pub status: String,
    pub installed_at: String,
    pub file_count: i64,
    pub category: Option<String>,
    pub source_ref: Option<String>,
    pub source_file: Option<String>,
}

pub const STATUS_ENABLED: &str = "installed";
pub const STATUS_DISABLED: &str = "disabled";

impl ModRow {
    /// Whether the mod's files are in the game directory.
    pub fn enabled(&self) -> bool {
        self.status == STATUS_ENABLED
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModFile {
    pub mod_id: i64,
    pub rel_path: String,
    pub staged_path: String,
    pub sha256: String,
    pub size: i64,
}

#[derive(Debug, Clone, Default)]
pub struct NewMod {
    pub name: String,
    pub version: Option<String>,
    pub source: String,
    pub nexus_mod_id: Option<i64>,
    pub nexus_file_id: Option<i64>,
    pub archive_name: String,
    pub archive_sha256: String,
    pub archive_md5: String,
    pub category: Option<String>,
    pub source_ref: Option<String>,
    pub source_file: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DownloadRow {
    pub id: i64,
    pub nexus_mod_id: Option<i64>,
    pub nexus_file_id: Option<i64>,
    pub file_name: String,
    pub path: String,
    pub sha256: String,
    pub md5: String,
    pub size: i64,
    pub verified: bool,
    pub downloaded_at: String,
    pub source: String,
    pub source_ref: Option<String>,
    pub source_file: Option<String>,
    pub mod_name: Option<String>,
    pub version: Option<String>,
    pub game_version: Option<String>,
    pub checked: Option<String>,
    pub category: Option<String>,
    pub channel: Option<String>,
}

/// One game version in the history (see [`Db::game_versions`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameVersionRow {
    pub id: i64,
    pub game_id: i64,
    pub version: Option<String>,
    pub build_id: Option<String>,
    pub first_seen: String,
    pub left_at: Option<String>,
}

/// A mod as it was when the game moved on from a version.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMod {
    pub name: String,
    pub version: Option<String>,
    pub source: String,
    pub nexus_mod_id: Option<i64>,
    pub nexus_file_id: Option<i64>,
    pub source_ref: Option<String>,
    pub archive_name: String,
    pub enabled: bool,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// Open without the ability to change anything (for the agent server).
    pub fn open_read_only(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.pragma_update(None, "query_only", true)?;
        Ok(Self { conn })
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch("PRAGMA journal_mode = WAL;")?;
        conn.execute_batch(SCHEMA)?;
        for (table, column, decl) in MIGRATIONS {
            let exists: bool = conn.query_row(
                &format!("SELECT count(*) > 0 FROM pragma_table_info('{table}') WHERE name = ?1"),
                [column],
                |r| r.get(0),
            )?;
            if !exists {
                conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))?;
            }
        }
        // Custom categories (one per mod) became tags (several per mod).
        let old: bool =
            conn.query_row("SELECT count(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'custom_categories'", [], |r| r.get(0))?;
        if old {
            conn.execute_batch(
                "BEGIN;
                 INSERT OR IGNORE INTO tags (name, color, position) SELECT name, color, position FROM custom_categories;
                 INSERT OR IGNORE INTO mod_tags (mod_key, tag)
                   SELECT c.mod_key, t.name FROM mod_custom_categories c JOIN tags t ON t.name = c.category;
                 DROP TABLE IF EXISTS mod_custom_categories;
                 DROP TABLE custom_categories;
                 COMMIT;",
            )?;
        }
        Ok(Self { conn })
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Insert or refresh a game; returns its id.
    pub fn upsert_game(&self, g: &GameInstall) -> Result<i64> {
        let store = serde_json::to_value(&g.store)?.as_str().unwrap_or("manual").to_string();
        self.conn.execute(
            "INSERT INTO games (path, store, build_id, exe_file_version, exe_product_version)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(path) DO UPDATE SET store = excluded.store, build_id = excluded.build_id,
               exe_file_version = excluded.exe_file_version,
               exe_product_version = excluded.exe_product_version, last_seen = datetime('now')",
            params![
                g.path.to_string_lossy(),
                store,
                g.build_id,
                g.exe_file_version,
                g.exe_product_version
            ],
        )?;
        let id = self.conn.query_row(
            "SELECT id FROM games WHERE path = ?1",
            [g.path.to_string_lossy()],
            |r| r.get(0),
        )?;
        let version = g.exe_product_version.clone().or(g.exe_file_version.clone());
        self.record_game_version(id, version.as_deref(), g.build_id.as_deref())?;
        Ok(id)
    }

    /// Note the game's current version. When it differs from the last one
    /// seen (the game updated), the installed mods are kept as the old
    /// version's snapshot and a new version starts. Returns whether a new
    /// version was added.
    pub fn record_game_version(&self, game_id: i64, version: Option<&str>, build_id: Option<&str>) -> Result<bool> {
        if version.is_none() && build_id.is_none() {
            return Ok(false);
        }
        let last = self.game_versions(game_id)?.pop();
        if let Some(last) = &last {
            let same_version = last.version.as_deref() == version || version.is_none();
            // A build id that's missing on either side (a manual install, a
            // failed read) isn't a change.
            let same_build = match (last.build_id.as_deref(), build_id) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            };
            if same_version && same_build {
                if last.build_id.is_none() && build_id.is_some() {
                    self.conn.execute("UPDATE game_versions SET build_id = ?2 WHERE id = ?1", params![last.id, build_id])?;
                }
                return Ok(false);
            }
        }
        let tx = self.conn.unchecked_transaction()?;
        if let Some(last) = &last {
            tx.execute("DELETE FROM game_version_mods WHERE game_version_id = ?1", [last.id])?;
            tx.execute(
                "INSERT INTO game_version_mods (game_version_id, name, version, source, nexus_mod_id, nexus_file_id,
                   source_ref, archive_name, enabled)
                 SELECT ?1, name, version, source, nexus_mod_id, nexus_file_id, source_ref, archive_name,
                   status = 'installed'
                 FROM mods WHERE game_id = ?2 ORDER BY id",
                params![last.id, game_id],
            )?;
            tx.execute("UPDATE game_versions SET left_at = datetime('now') WHERE id = ?1", [last.id])?;
        }
        tx.execute(
            "INSERT INTO game_versions (game_id, version, build_id) VALUES (?1, ?2, ?3)",
            params![game_id, version, build_id],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// The game's versions, oldest first; the last one is current.
    pub fn game_versions(&self, game_id: i64) -> Result<Vec<GameVersionRow>> {
        let mut st = self.conn.prepare(
            "SELECT id, game_id, version, build_id, first_seen, left_at FROM game_versions WHERE game_id = ?1 ORDER BY id",
        )?;
        let rows = st.query_map([game_id], |r| {
            Ok(GameVersionRow {
                id: r.get(0)?,
                game_id: r.get(1)?,
                version: r.get(2)?,
                build_id: r.get(3)?,
                first_seen: r.get(4)?,
                left_at: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// The mods installed when the game moved on from version `id`.
    pub fn game_version_mods(&self, id: i64) -> Result<Vec<SnapshotMod>> {
        let mut st = self.conn.prepare(
            "SELECT name, version, source, nexus_mod_id, nexus_file_id, source_ref, archive_name, enabled
             FROM game_version_mods WHERE game_version_id = ?1 ORDER BY name COLLATE NOCASE",
        )?;
        let rows = st.query_map([id], |r| {
            Ok(SnapshotMod {
                name: r.get(0)?,
                version: r.get(1)?,
                source: r.get(2)?,
                nexus_mod_id: r.get(3)?,
                nexus_file_id: r.get(4)?,
                source_ref: r.get(5)?,
                archive_name: r.get(6)?,
                enabled: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn games(&self) -> Result<Vec<GameRow>> {
        let mut st = self.conn.prepare(
            "SELECT id, path, store, build_id, exe_file_version, exe_product_version FROM games ORDER BY id",
        )?;
        let rows = st.query_map([], |r| {
            Ok(GameRow {
                id: r.get(0)?,
                path: r.get(1)?,
                store: r.get(2)?,
                build_id: r.get(3)?,
                exe_file_version: r.get(4)?,
                exe_product_version: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn game(&self, id: i64) -> Result<GameRow> {
        self.games()?
            .into_iter()
            .find(|g| g.id == id)
            .ok_or_else(|| crate::Error::GameNotFound(format!("game id {id}")))
    }

    pub fn insert_mod(&self, game: &GameRow, m: &NewMod) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO mods (game_id, name, version, source, nexus_mod_id, nexus_file_id,
               archive_name, archive_sha256, archive_md5, game_build_id, game_version,
               category, source_ref, source_file)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                game.id,
                m.name,
                m.version,
                m.source,
                m.nexus_mod_id,
                m.nexus_file_id,
                m.archive_name,
                m.archive_sha256,
                m.archive_md5,
                game.build_id,
                game.exe_product_version.clone().or(game.exe_file_version.clone()),
                m.category,
                m.source_ref,
                m.source_file,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn delete_mod(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM mods WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn mods(&self, game_id: i64) -> Result<Vec<ModRow>> {
        let mut st = self.conn.prepare(
            "SELECT m.id, m.game_id, m.name, m.version, m.source, m.nexus_mod_id, m.nexus_file_id,
                    m.archive_name, m.archive_sha256, m.archive_md5, m.game_build_id, m.game_version,
                    m.status, m.installed_at,
                    (SELECT count(*) FROM mod_files f WHERE f.mod_id = m.id),
                    m.category, m.source_ref, m.source_file
             FROM mods m WHERE m.game_id = ?1 ORDER BY m.id",
        )?;
        let rows = st.query_map([game_id], |r| {
            Ok(ModRow {
                id: r.get(0)?,
                game_id: r.get(1)?,
                name: r.get(2)?,
                version: r.get(3)?,
                source: r.get(4)?,
                nexus_mod_id: r.get(5)?,
                nexus_file_id: r.get(6)?,
                archive_name: r.get(7)?,
                archive_sha256: r.get(8)?,
                archive_md5: r.get(9)?,
                game_build_id: r.get(10)?,
                game_version: r.get(11)?,
                status: r.get(12)?,
                installed_at: r.get(13)?,
                file_count: r.get(14)?,
                category: r.get(15)?,
                source_ref: r.get(16)?,
                source_file: r.get(17)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn set_mod_status(&self, id: i64, status: &str) -> Result<()> {
        self.conn.execute("UPDATE mods SET status = ?2 WHERE id = ?1", params![id, status])?;
        Ok(())
    }

    /// Replace the record of files left behind when `mod_id` was disabled.
    pub fn set_kept_files(&self, mod_id: i64, files: &[(String, String)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM kept_files WHERE mod_id = ?1", [mod_id])?;
        for (path, sha) in files {
            tx.execute("INSERT OR REPLACE INTO kept_files (mod_id, rel_path, sha256) VALUES (?1, ?2, ?3)", params![mod_id, path, sha])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// `(rel_path, sha256)` of files left behind when `mod_id` was disabled.
    pub fn kept_files(&self, mod_id: i64) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare("SELECT rel_path, sha256 FROM kept_files WHERE mod_id = ?1 ORDER BY rel_path")?;
        let rows = st.query_map([mod_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn get_mod(&self, id: i64) -> Result<ModRow> {
        let game_id: i64 = self.conn.query_row("SELECT game_id FROM mods WHERE id = ?1", [id], |r| r.get(0))?;
        self.mods(game_id)?
            .into_iter()
            .find(|m| m.id == id)
            .ok_or_else(|| crate::Error::Other(format!("mod {id} not found")))
    }

    pub fn add_mod_file(&self, f: &ModFile) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO mod_files (mod_id, rel_path, staged_path, sha256, size)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![f.mod_id, f.rel_path, f.staged_path, f.sha256, f.size],
        )?;
        Ok(())
    }

    pub fn mod_files(&self, mod_id: i64) -> Result<Vec<ModFile>> {
        let mut st = self.conn.prepare(
            "SELECT mod_id, rel_path, staged_path, sha256, size FROM mod_files WHERE mod_id = ?1 ORDER BY rel_path",
        )?;
        let rows = st.query_map([mod_id], map_file)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Other mods (same game) that also ship this path, newest first.
    /// Matching is case-insensitive because the game runs on Windows rules.
    pub fn owners_of(&self, game_id: i64, rel_path: &str, except_mod: i64) -> Result<Vec<ModFile>> {
        let mut st = self.conn.prepare(
            "SELECT f.mod_id, f.rel_path, f.staged_path, f.sha256, f.size
             FROM mod_files f JOIN mods m ON m.id = f.mod_id
             WHERE m.game_id = ?1 AND lower(f.rel_path) = lower(?2) AND f.mod_id != ?3
               AND m.status = 'installed'
             ORDER BY m.installed_at DESC, m.id DESC",
        )?;
        let rows = st.query_map(params![game_id, rel_path, except_mod], map_file)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn add_backup(&self, game_id: i64, rel_path: &str, backup_path: &str, sha256: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO backups (game_id, rel_path, backup_path, sha256) VALUES (?1, ?2, ?3, ?4)",
            params![game_id, rel_path, backup_path, sha256],
        )?;
        Ok(())
    }

    pub fn take_backup(&self, game_id: i64, rel_path: &str) -> Result<Option<(String, String)>> {
        let row: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT backup_path, sha256 FROM backups WHERE game_id = ?1 AND lower(rel_path) = lower(?2)",
                params![game_id, rel_path],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if row.is_some() {
            self.conn.execute(
                "DELETE FROM backups WHERE game_id = ?1 AND lower(rel_path) = lower(?2)",
                params![game_id, rel_path],
            )?;
        }
        Ok(row)
    }

    pub fn insert_download(&self, d: &DownloadRow) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO downloads (nexus_mod_id, nexus_file_id, file_name, path, sha256, md5, size, verified,
               source, source_ref, source_file, mod_name, version, game_version, checked, category, channel)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                d.nexus_mod_id,
                d.nexus_file_id,
                d.file_name,
                d.path,
                d.sha256,
                d.md5,
                d.size,
                d.verified,
                if d.source.is_empty() { "nexus" } else { d.source.as_str() },
                d.source_ref,
                d.source_file,
                d.mod_name,
                d.version,
                d.game_version,
                d.checked,
                d.category,
                d.channel,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn downloads(&self) -> Result<Vec<DownloadRow>> {
        let mut st = self.conn.prepare(
            "SELECT id, nexus_mod_id, nexus_file_id, file_name, path, sha256, md5, size, verified, downloaded_at,
                    source, source_ref, source_file, mod_name, version, game_version, checked, category, channel
             FROM downloads ORDER BY id DESC",
        )?;
        let rows = st.query_map([], |r| {
            Ok(DownloadRow {
                id: r.get(0)?,
                nexus_mod_id: r.get(1)?,
                nexus_file_id: r.get(2)?,
                file_name: r.get(3)?,
                path: r.get(4)?,
                sha256: r.get(5)?,
                md5: r.get(6)?,
                size: r.get(7)?,
                verified: r.get(8)?,
                downloaded_at: r.get(9)?,
                source: r.get(10)?,
                source_ref: r.get(11)?,
                source_file: r.get(12)?,
                mod_name: r.get(13)?,
                version: r.get(14)?,
                game_version: r.get(15)?,
                checked: r.get(16)?,
                category: r.get(17)?,
                channel: r.get(18)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn download(&self, id: i64) -> Result<DownloadRow> {
        self.downloads()?
            .into_iter()
            .find(|d| d.id == id)
            .ok_or_else(|| crate::Error::Other("download not found".into()))
    }

    /// Forget every download entry for the file at `path`.
    pub fn delete_downloads_at(&self, path: &str) -> Result<()> {
        self.conn.execute("DELETE FROM downloads WHERE path = ?1", [path])?;
        Ok(())
    }

    /// The file moved (a new download location, or sorted into its folder).
    pub fn set_download_path(&self, id: i64, path: &str) -> Result<()> {
        self.conn.execute("UPDATE downloads SET path = ?2 WHERE id = ?1", params![id, path])?;
        Ok(())
    }
}

impl Db {
    /// Replace a mod's index. `version` is the scanner version so old
    /// indexes get rebuilt when the scanner improves.
    pub fn set_touches(&self, mod_id: i64, version: i64, touches: &[crate::analysis::Touch]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM touches WHERE mod_id = ?1", [mod_id])?;
        {
            let mut st = tx.prepare("INSERT INTO touches (mod_id, kind, key, file) VALUES (?1, ?2, ?3, ?4)")?;
            for t in touches {
                st.execute(params![mod_id, t.kind.as_str(), t.key, t.file])?;
            }
        }
        tx.execute(
            "INSERT INTO indexed_mods (mod_id, version) VALUES (?1, ?2)
             ON CONFLICT(mod_id) DO UPDATE SET version = excluded.version",
            params![mod_id, version],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn touches(&self, mod_id: i64) -> Result<Vec<crate::analysis::Touch>> {
        let mut st = self.conn.prepare("SELECT kind, key, file FROM touches WHERE mod_id = ?1")?;
        let rows = st.query_map([mod_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (k, key, file) = row?;
            if let Some(kind) = crate::analysis::Kind::parse(&k) {
                out.push(crate::analysis::Touch { kind, key, file });
            }
        }
        Ok(out)
    }

    pub fn index_version(&self, mod_id: i64) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT version FROM indexed_mods WHERE mod_id = ?1", [mod_id], |r| r.get(0))
            .optional()?)
    }

    pub fn set_base_resources(&self, game_id: i64, build: &str, hashes: &std::collections::BTreeSet<u64>) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM base_resources WHERE game_id = ?1", [game_id])?;
        {
            let mut st = tx.prepare("INSERT OR IGNORE INTO base_resources (game_id, hash) VALUES (?1, ?2)")?;
            for h in hashes {
                st.execute(params![game_id, *h as i64])?;
            }
        }
        tx.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![format!("base_index_build:{game_id}"), build],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Cached base-game hashes, if they were built for `build`.
    pub fn base_resources(&self, game_id: i64, build: &str) -> Result<Option<std::collections::BTreeSet<u64>>> {
        if self.get_setting(&format!("base_index_build:{game_id}"))?.as_deref() != Some(build) {
            return Ok(None);
        }
        let mut st = self.conn.prepare("SELECT hash FROM base_resources WHERE game_id = ?1")?;
        let rows = st.query_map([game_id], |r| r.get::<_, i64>(0))?;
        let mut out = std::collections::BTreeSet::new();
        for h in rows {
            out.insert(h? as u64);
        }
        Ok(Some(out))
    }
}

fn map_file(r: &rusqlite::Row<'_>) -> rusqlite::Result<ModFile> {
    Ok(ModFile { mod_id: r.get(0)?, rel_path: r.get(1)?, staged_path: r.get(2)?, sha256: r.get(3)?, size: r.get(4)? })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turns_custom_categories_into_tags() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(
                "CREATE TABLE custom_categories (name TEXT PRIMARY KEY COLLATE NOCASE, color TEXT, position INTEGER NOT NULL DEFAULT 0);
                 CREATE TABLE mod_custom_categories (mod_key TEXT PRIMARY KEY,
                   category TEXT NOT NULL REFERENCES custom_categories (name) ON DELETE CASCADE ON UPDATE CASCADE);
                 INSERT INTO custom_categories VALUES ('Visuals', '#ff0000', 0), ('Core', NULL, 1);
                 INSERT INTO mod_custom_categories VALUES ('nexus:107', 'Core'), ('github:a/b', 'Visuals');",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let tags: Vec<(String, Option<String>)> = db
            .conn
            .prepare("SELECT name, color FROM tags ORDER BY position")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(tags, [("Visuals".into(), Some("#ff0000".into())), ("Core".into(), None)]);
        let n: i64 = db.conn.query_row("SELECT count(*) FROM mod_tags WHERE (mod_key, tag) IN (VALUES ('nexus:107', 'Core'), ('github:a/b', 'Visuals'))", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2);
        let old: i64 = db.conn.query_row("SELECT count(*) FROM sqlite_master WHERE name LIKE '%custom_categories'", [], |r| r.get(0)).unwrap();
        assert_eq!(old, 0, "old tables are gone");
        drop(db);
        Db::open(&path).unwrap();
    }

    #[test]
    fn upgrades_a_v01_library() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        {
            // The v0.1 tables, before the source/category columns existed.
            let c = Connection::open(&path).unwrap();
            c.execute_batch(
                "CREATE TABLE mods (id INTEGER PRIMARY KEY, game_id INTEGER NOT NULL, name TEXT NOT NULL, version TEXT,
                   source TEXT NOT NULL, nexus_mod_id INTEGER, nexus_file_id INTEGER, archive_name TEXT NOT NULL,
                   archive_sha256 TEXT NOT NULL, archive_md5 TEXT NOT NULL, game_build_id TEXT, game_version TEXT,
                   status TEXT NOT NULL DEFAULT 'installed', installed_at TEXT NOT NULL DEFAULT (datetime('now')));
                 CREATE TABLE downloads (id INTEGER PRIMARY KEY, nexus_mod_id INTEGER, nexus_file_id INTEGER,
                   file_name TEXT NOT NULL, path TEXT NOT NULL, sha256 TEXT NOT NULL, md5 TEXT NOT NULL,
                   size INTEGER NOT NULL, verified INTEGER NOT NULL DEFAULT 0,
                   downloaded_at TEXT NOT NULL DEFAULT (datetime('now')));
                 INSERT INTO downloads (nexus_mod_id, nexus_file_id, file_name, path, sha256, md5, size, verified)
                   VALUES (107, 1, 'cet.zip', '/x/cet.zip', 'aa', 'bb', 3, 1);",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let d = &db.downloads().unwrap()[0];
        assert_eq!(d.source, "nexus", "old downloads came from Nexus");
        assert_eq!(d.version, None);
        db.insert_download(&DownloadRow {
            file_name: "ArchiveXL.zip".into(),
            path: "/x/a.zip".into(),
            sha256: "cc".into(),
            md5: "dd".into(),
            size: 1,
            source: "github".into(),
            source_ref: Some("psiberx/cp2077-archive-xl".into()),
            version: Some("1.21.0".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(db.downloads().unwrap()[0].source_ref.as_deref(), Some("psiberx/cp2077-archive-xl"));
        drop(db);
        // Opening again doesn't try to add the columns twice.
        Db::open(&path).unwrap();
    }
}
