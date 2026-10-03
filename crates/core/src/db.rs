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
    status          TEXT NOT NULL DEFAULT 'installed',
    installed_at    TEXT NOT NULL DEFAULT (datetime('now'))
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
CREATE TABLE IF NOT EXISTS downloads (
    id             INTEGER PRIMARY KEY,
    nexus_mod_id   INTEGER,
    nexus_file_id  INTEGER,
    file_name      TEXT NOT NULL,
    path           TEXT NOT NULL,
    sha256         TEXT NOT NULL,
    md5            TEXT NOT NULL,
    size           INTEGER NOT NULL,
    verified       INTEGER NOT NULL DEFAULT 0, -- md5 matched Nexus' record
    downloaded_at  TEXT NOT NULL DEFAULT (datetime('now'))
);
"#;

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch("PRAGMA journal_mode = WAL;")?;
        conn.execute_batch(SCHEMA)?;
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
        Ok(self.conn.query_row(
            "SELECT id FROM games WHERE path = ?1",
            [g.path.to_string_lossy()],
            |r| r.get(0),
        )?)
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
               archive_name, archive_sha256, archive_md5, game_build_id, game_version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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
                    (SELECT count(*) FROM mod_files f WHERE f.mod_id = m.id)
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
            })
        })?;
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
            "INSERT INTO downloads (nexus_mod_id, nexus_file_id, file_name, path, sha256, md5, size, verified)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![d.nexus_mod_id, d.nexus_file_id, d.file_name, d.path, d.sha256, d.md5, d.size, d.verified],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn downloads(&self) -> Result<Vec<DownloadRow>> {
        let mut st = self.conn.prepare(
            "SELECT id, nexus_mod_id, nexus_file_id, file_name, path, sha256, md5, size, verified, downloaded_at
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
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
}

fn map_file(r: &rusqlite::Row<'_>) -> rusqlite::Result<ModFile> {
    Ok(ModFile { mod_id: r.get(0)?, rel_path: r.get(1)?, staged_path: r.get(2)?, sha256: r.get(3)?, size: r.get(4)? })
}
