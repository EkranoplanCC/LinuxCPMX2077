//! Modpacks: Nexus collections the user installed and follows, the user's
//! own mod categories, and mod lists exported to and imported from a JSON
//! file. Everything here is local; the Nexus calls live in `nexus_browse`.

use std::collections::{BTreeMap, HashMap};

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::db::{Db, ModRow};
use crate::nexus_browse::{Collection, CollectionMod, is_collection_slug};
use crate::{Error, Result};

/// A stable name for an installed mod that survives updates and reinstalls:
/// `nexus:<mod id>`, `<source>:<id>` (e.g. `github:owner/repo`) or, for
/// archives installed by hand, `name:<lowercase name>`.
pub fn mod_key(m: &ModRow) -> String {
    key_for(&m.source, m.nexus_mod_id, m.source_ref.as_deref(), &m.name)
}

fn key_for(source: &str, nexus_mod_id: Option<i64>, source_ref: Option<&str>, name: &str) -> String {
    if let Some(id) = nexus_mod_id.filter(|i| *i > 0) {
        return format!("nexus:{id}");
    }
    match source_ref.map(str::trim).filter(|r| !r.is_empty()) {
        Some(r) if source != "manual" && source != "nexus" => format!("{}:{}", source.to_lowercase(), r.to_lowercase()),
        _ => format!("name:{}", name.trim().to_lowercase()),
    }
}

// ---------------------------------------------------------------------------
// Tracked collections

#[derive(Debug, Clone, Serialize)]
pub struct TrackedCollection {
    pub slug: String,
    pub name: String,
    pub author: Option<String>,
    /// The revision the user installed.
    pub revision: Option<u32>,
    pub game_version: Option<String>,
    /// The newest revision seen on Nexus, when it was last checked.
    pub latest_revision: Option<u32>,
    pub added_at: String,
    pub mods: Vec<CollectionMod>,
}

impl TrackedCollection {
    pub fn update_available(&self) -> bool {
        matches!((self.revision, self.latest_revision), (Some(have), Some(latest)) if latest > have)
    }
}

/// Follow `c` in `game_id`, or move an already followed collection to the
/// revision in `c`.
pub fn track(db: &Db, game_id: i64, c: &Collection) -> Result<()> {
    if !is_collection_slug(&c.slug) {
        return Err(Error::Other(format!("`{}` is not a collection id", c.slug)));
    }
    let tx = db.conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO tracked_collections (game_id, slug, name, author, revision, game_version, latest_revision)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?5)
         ON CONFLICT(game_id, slug) DO UPDATE SET name = excluded.name, author = excluded.author,
           revision = excluded.revision, game_version = excluded.game_version,
           latest_revision = max(coalesce(latest_revision, 0), coalesce(excluded.revision, 0))",
        params![game_id, c.slug, c.name, c.author, c.revision, c.game_version],
    )?;
    tx.execute("DELETE FROM collection_mods WHERE game_id = ?1 AND slug = ?2", params![game_id, c.slug])?;
    for (i, m) in c.mods.iter().enumerate() {
        tx.execute(
            "INSERT OR IGNORE INTO collection_mods
               (game_id, slug, position, nexus_mod_id, nexus_file_id, mod_name, file_name, version, optional)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![game_id, c.slug, i as i64, m.mod_id, m.file_id, m.mod_name, m.file_name, m.version, m.optional],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn untrack(db: &Db, game_id: i64, slug: &str) -> Result<()> {
    db.conn.execute("DELETE FROM tracked_collections WHERE game_id = ?1 AND slug = ?2", params![game_id, slug])?;
    Ok(())
}

/// Record the newest revision Nexus has for a followed collection.
pub fn set_latest_revision(db: &Db, game_id: i64, slug: &str, latest: Option<u32>) -> Result<()> {
    db.conn.execute(
        "UPDATE tracked_collections SET latest_revision = ?3 WHERE game_id = ?1 AND slug = ?2",
        params![game_id, slug, latest],
    )?;
    Ok(())
}

pub fn tracked(db: &Db, game_id: i64) -> Result<Vec<TrackedCollection>> {
    let mut st = db.conn.prepare(
        "SELECT slug, name, author, revision, game_version, latest_revision, added_at
         FROM tracked_collections WHERE game_id = ?1 ORDER BY lower(name)",
    )?;
    let rows = st.query_map([game_id], |r| {
        Ok(TrackedCollection {
            slug: r.get(0)?,
            name: r.get(1)?,
            author: r.get(2)?,
            revision: r.get(3)?,
            game_version: r.get(4)?,
            latest_revision: r.get(5)?,
            added_at: r.get(6)?,
            mods: Vec::new(),
        })
    })?;
    let mut out: Vec<TrackedCollection> = rows.collect::<std::result::Result<_, _>>()?;
    let mut st = db.conn.prepare(
        "SELECT nexus_mod_id, nexus_file_id, mod_name, file_name, version, optional
         FROM collection_mods WHERE game_id = ?1 AND slug = ?2 ORDER BY position",
    )?;
    for c in &mut out {
        let rows = st.query_map(params![game_id, c.slug], |r| {
            Ok(CollectionMod {
                mod_id: r.get(0)?,
                file_id: r.get(1)?,
                mod_name: r.get(2)?,
                file_name: r.get(3)?,
                version: r.get(4)?,
                optional: r.get(5)?,
            })
        })?;
        c.mods = rows.collect::<std::result::Result<_, _>>()?;
    }
    Ok(out)
}

pub fn tracked_one(db: &Db, game_id: i64, slug: &str) -> Result<Option<TrackedCollection>> {
    Ok(tracked(db, game_id)?.into_iter().find(|c| c.slug == slug))
}

/// Where one of a collection's mods stands in the user's library.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ModState {
    /// The exact file the collection asks for.
    Installed { installed_id: i64 },
    /// Installed, but switched off.
    Disabled { installed_id: i64 },
    /// The mod is installed from a different file (an older or newer version).
    OtherFile { installed_id: i64, installed_version: Option<String> },
    Missing,
}

#[derive(Debug, Clone, Serialize)]
pub struct CollectionModStatus {
    #[serde(flatten)]
    pub item: CollectionMod,
    #[serde(flatten)]
    pub state: ModState,
}

/// Each of `wanted` checked against the installed mods.
pub fn status(wanted: &[CollectionMod], installed: &[ModRow]) -> Vec<CollectionModStatus> {
    let mut by_mod: HashMap<i64, Vec<&ModRow>> = HashMap::new();
    for m in installed {
        if let Some(id) = m.nexus_mod_id {
            by_mod.entry(id).or_default().push(m);
        }
    }
    wanted
        .iter()
        .map(|w| {
            let have = by_mod.get(&w.mod_id).map(Vec::as_slice).unwrap_or_default();
            let state = if let Some(m) = have.iter().find(|m| m.nexus_file_id == Some(w.file_id)) {
                if m.enabled() { ModState::Installed { installed_id: m.id } } else { ModState::Disabled { installed_id: m.id } }
            } else if let Some(m) = have.first() {
                ModState::OtherFile { installed_id: m.id, installed_version: m.version.clone() }
            } else {
                ModState::Missing
            };
            CollectionModStatus { item: w.clone(), state }
        })
        .collect()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct StatusCounts {
    pub installed: usize,
    pub disabled: usize,
    pub other_file: usize,
    pub missing: usize,
    /// Of `missing`, the ones the collection doesn't mark optional.
    pub missing_required: usize,
}

pub fn counts(list: &[CollectionModStatus]) -> StatusCounts {
    let mut c = StatusCounts::default();
    for s in list {
        match s.state {
            ModState::Installed { .. } => c.installed += 1,
            ModState::Disabled { .. } => c.disabled += 1,
            ModState::OtherFile { .. } => c.other_file += 1,
            ModState::Missing => {
                c.missing += 1;
                if !s.item.optional {
                    c.missing_required += 1;
                }
            }
        }
    }
    c
}

/// What changed between two revisions of a collection, by mod.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RevisionDiff {
    pub added: Vec<CollectionMod>,
    pub removed: Vec<CollectionMod>,
    /// `(old, new)`: same mod, different file.
    pub changed: Vec<(CollectionMod, CollectionMod)>,
}

pub fn diff(old: &[CollectionMod], new: &[CollectionMod]) -> RevisionDiff {
    let first = |list: &[CollectionMod]| {
        let mut m: BTreeMap<i64, CollectionMod> = BTreeMap::new();
        for c in list {
            m.entry(c.mod_id).or_insert_with(|| c.clone());
        }
        m
    };
    let (o, n) = (first(old), first(new));
    let mut d = RevisionDiff::default();
    // Keep the new revision's order for what's added or changed.
    for c in new {
        match o.get(&c.mod_id) {
            None if !d.added.iter().any(|a| a.mod_id == c.mod_id) => d.added.push(c.clone()),
            Some(prev) if prev.file_id != c.file_id && n.get(&c.mod_id).is_some_and(|f| f.file_id == c.file_id) => {
                d.changed.push((prev.clone(), c.clone()))
            }
            _ => {}
        }
    }
    d.removed = old.iter().filter(|c| !n.contains_key(&c.mod_id)).cloned().collect();
    d.removed.dedup_by_key(|c| c.mod_id);
    d
}

// ---------------------------------------------------------------------------
// The user's own categories

pub const MAX_CATEGORY_CHARS: usize = 60;
const MAX_CATEGORIES: usize = 500;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomCategory {
    pub name: String,
    /// `#rrggbb`, or none for the default look.
    #[serde(default)]
    pub color: Option<String>,
}

/// A category name as stored: trimmed, single-spaced, no control characters.
pub fn clean_category(name: &str) -> Result<String> {
    let s: String = name.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.is_empty() {
        return Err(Error::Other("a category needs a name".into()));
    }
    if s.chars().count() > MAX_CATEGORY_CHARS {
        return Err(Error::Other(format!("category names can be at most {MAX_CATEGORY_CHARS} characters")));
    }
    Ok(s)
}

fn clean_color(c: Option<&str>) -> Option<String> {
    let c = c?.trim();
    (c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|h| h.is_ascii_hexdigit())).then(|| c.to_ascii_lowercase())
}

pub fn categories(db: &Db) -> Result<Vec<CustomCategory>> {
    let mut st = db.conn.prepare("SELECT name, color FROM custom_categories ORDER BY position, lower(name)")?;
    let rows = st.query_map([], |r| Ok(CustomCategory { name: r.get(0)?, color: r.get(1)? }))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Add a category; returns its stored name. Adding one that exists (in any
/// letter case) keeps the existing one and updates its color if given.
pub fn add_category(db: &Db, name: &str, color: Option<&str>) -> Result<String> {
    let name = clean_category(name)?;
    let existing: Option<String> =
        db.conn.query_row("SELECT name FROM custom_categories WHERE name = ?1", [&name], |r| r.get(0)).optional()?;
    if let Some(existing) = existing {
        if let Some(c) = clean_color(color) {
            db.conn.execute("UPDATE custom_categories SET color = ?2 WHERE name = ?1", params![existing, c])?;
        }
        return Ok(existing);
    }
    let count: i64 = db.conn.query_row("SELECT count(*) FROM custom_categories", [], |r| r.get(0))?;
    if count as usize >= MAX_CATEGORIES {
        return Err(Error::Other(format!("at most {MAX_CATEGORIES} categories")));
    }
    db.conn.execute(
        "INSERT INTO custom_categories (name, color, position) VALUES (?1, ?2, ?3)",
        params![name, clean_color(color), count],
    )?;
    Ok(name)
}

/// Rename or recolor; mods in the category follow the new name.
pub fn edit_category(db: &Db, old: &str, new_name: &str, color: Option<&str>) -> Result<()> {
    let new_name = clean_category(new_name)?;
    let taken: Option<String> = db
        .conn
        .query_row("SELECT name FROM custom_categories WHERE name = ?1 AND name != ?2 COLLATE BINARY", params![new_name, old], |r| r.get(0))
        .optional()?;
    if taken.is_some_and(|t| !t.eq_ignore_ascii_case(old)) {
        return Err(Error::Other(format!("there is already a category called {new_name}")));
    }
    let n = db
        .conn
        .execute("UPDATE custom_categories SET name = ?2, color = ?3 WHERE name = ?1", params![old, new_name, clean_color(color)])?;
    if n == 0 {
        return Err(Error::Other(format!("no category called {old}")));
    }
    Ok(())
}

/// Delete a category; its mods become uncategorized (they stay installed).
pub fn delete_category(db: &Db, name: &str) -> Result<()> {
    db.conn.execute("DELETE FROM custom_categories WHERE name = ?1", [name])?;
    Ok(())
}

/// Put the mod with `key` in `category`, or in none.
pub fn set_mod_category(db: &Db, key: &str, category: Option<&str>) -> Result<()> {
    match category.map(str::trim).filter(|c| !c.is_empty()) {
        None => {
            db.conn.execute("DELETE FROM mod_custom_categories WHERE mod_key = ?1", [key])?;
        }
        Some(c) => {
            let name: String = db
                .conn
                .query_row("SELECT name FROM custom_categories WHERE name = ?1", [c], |r| r.get(0))
                .optional()?
                .ok_or_else(|| Error::Other(format!("no category called {c}")))?;
            db.conn.execute(
                "INSERT INTO mod_custom_categories (mod_key, category) VALUES (?1, ?2)
                 ON CONFLICT(mod_key) DO UPDATE SET category = excluded.category",
                params![key, name],
            )?;
        }
    }
    Ok(())
}

/// Mod key -> custom category.
pub fn mod_categories(db: &Db) -> Result<HashMap<String, String>> {
    let mut st = db.conn.prepare("SELECT mod_key, category FROM mod_custom_categories")?;
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

// ---------------------------------------------------------------------------
// Mod lists: export and import

pub const MODLIST_FORMAT: &str = "cpmx2077-modlist";
pub const MODLIST_VERSION: u32 = 1;
/// Imported files bigger than this are refused before parsing.
pub const MAX_MODLIST_BYTES: usize = 4 * 1024 * 1024;
const MAX_MODLIST_MODS: usize = 5000;

/// One mod in an exported list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModListEntry {
    pub name: String,
    /// `nexus`, `github`, `manual`, …
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nexus_mod_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nexus_file_id: Option<i64>,
    /// The source's own id, e.g. `owner/repo` on GitHub.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The user's own category.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

impl ModListEntry {
    pub fn key(&self) -> String {
        key_for(&self.source, self.nexus_mod_id, self.source_ref.as_deref(), &self.name)
    }
}

/// A followed collection, by id and revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModListCollection {
    pub slug: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub revision: Option<u32>,
}

/// The file written by "Export": the user's categories, mods and followed
/// collections. Plain JSON so it can be shared, diffed and edited by hand.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModList {
    pub format: String,
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game_version: Option<String>,
    #[serde(default)]
    pub categories: Vec<CustomCategory>,
    #[serde(default)]
    pub mods: Vec<ModListEntry>,
    #[serde(default)]
    pub collections: Vec<ModListCollection>,
}

/// The list to export for `game_id`.
pub fn export(db: &Db, game_id: i64) -> Result<ModList> {
    let game = db.game(game_id)?;
    let cats = mod_categories(db)?;
    let mods = db
        .mods(game_id)?
        .into_iter()
        .map(|m| ModListEntry {
            category: cats.get(&mod_key(&m)).cloned(),
            enabled: m.enabled(),
            name: m.name,
            source: m.source,
            nexus_mod_id: m.nexus_mod_id,
            nexus_file_id: m.nexus_file_id,
            source_ref: m.source_ref,
            version: m.version,
        })
        .collect();
    let collections = tracked(db, game_id)?
        .into_iter()
        .map(|c| ModListCollection { slug: c.slug, name: Some(c.name), revision: c.revision })
        .collect();
    Ok(ModList {
        format: MODLIST_FORMAT.into(),
        version: MODLIST_VERSION,
        game_version: game.exe_product_version.or(game.exe_file_version),
        categories: categories(db)?,
        mods,
        collections,
    })
}

/// Read an exported list, refusing anything that isn't one or is too big.
/// Names are cleaned and bad entries dropped rather than trusted.
pub fn parse_modlist(bytes: &[u8]) -> Result<ModList> {
    if bytes.len() > MAX_MODLIST_BYTES {
        return Err(Error::Other("that file is too big to be a CPMX2077 mod list".into()));
    }
    let mut l: ModList =
        serde_json::from_slice(bytes).map_err(|e| Error::Other(format!("that isn't a CPMX2077 mod list ({e})")))?;
    if l.format != MODLIST_FORMAT {
        return Err(Error::Other("that isn't a CPMX2077 mod list".into()));
    }
    if l.version > MODLIST_VERSION {
        return Err(Error::Other("that mod list was made by a newer CPMX2077; update the app to import it".into()));
    }
    if l.mods.len() > MAX_MODLIST_MODS || l.categories.len() > MAX_CATEGORIES {
        return Err(Error::Other("that mod list has too many entries".into()));
    }
    let short = |s: &str, max: usize| -> Option<String> {
        let s: String = s.chars().filter(|c| !c.is_control()).take(max).collect();
        let s = s.trim().to_string();
        (!s.is_empty()).then_some(s)
    };
    l.categories = l
        .categories
        .into_iter()
        .filter_map(|c| Some(CustomCategory { name: clean_category(&c.name).ok()?, color: clean_color(c.color.as_deref()) }))
        .collect();
    l.mods = l
        .mods
        .into_iter()
        .filter_map(|m| {
            Some(ModListEntry {
                name: short(&m.name, 300)?,
                source: short(&m.source, 30).filter(|s| s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))?,
                nexus_mod_id: m.nexus_mod_id.filter(|i| *i > 0),
                nexus_file_id: m.nexus_file_id.filter(|i| *i > 0),
                source_ref: m.source_ref.as_deref().and_then(|r| short(r, 200)),
                version: m.version.as_deref().and_then(|v| short(v, 100)),
                category: m.category.as_deref().and_then(|c| clean_category(c).ok()),
                enabled: m.enabled,
            })
        })
        .collect();
    l.collections.retain(|c| is_collection_slug(&c.slug));
    for c in &mut l.collections {
        c.slug = c.slug.to_ascii_lowercase();
        c.name = c.name.as_deref().and_then(|n| short(n, 200));
    }
    l.game_version = l.game_version.as_deref().and_then(|v| short(v, 50));
    Ok(l)
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ImportReport {
    pub categories_added: usize,
    /// Installed mods put in a category from the list.
    pub assigned: usize,
    /// Mods in the list that aren't installed, to get them.
    pub not_installed: Vec<ModListEntry>,
    /// Collections in the list that aren't followed yet.
    pub collections: Vec<ModListCollection>,
}

/// Add the list's categories, put the installed mods it names in them, and
/// report what it has that isn't installed. Nothing is downloaded here.
pub fn import(db: &Db, game_id: i64, l: &ModList) -> Result<ImportReport> {
    let mut r = ImportReport::default();
    let before = categories(db)?.len();
    let wanted = l.categories.iter().map(|c| (c.name.as_str(), c.color.as_deref()));
    for (name, color) in wanted.chain(l.mods.iter().filter_map(|m| Some((m.category.as_deref()?, None)))) {
        add_category(db, name, color)?;
    }
    r.categories_added = categories(db)?.len().saturating_sub(before);
    let installed: HashMap<String, ModRow> = db.mods(game_id)?.into_iter().map(|m| (mod_key(&m), m)).collect();
    for m in &l.mods {
        if installed.contains_key(&m.key()) {
            if let Some(c) = &m.category {
                set_mod_category(db, &m.key(), Some(c))?;
                r.assigned += 1;
            }
        } else {
            r.not_installed.push(m.clone());
        }
    }
    let followed: Vec<String> = tracked(db, game_id)?.into_iter().map(|c| c.slug).collect();
    r.collections = l.collections.iter().filter(|c| !followed.contains(&c.slug)).cloned().collect();
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::NewMod;
    use crate::game::{GameInstall, Store};

    fn cm(mod_id: i64, file_id: i64, optional: bool) -> CollectionMod {
        CollectionMod { mod_id, file_id, mod_name: format!("Mod {mod_id}"), file_name: None, version: Some(format!("{file_id}")), optional }
    }

    fn setup() -> (Db, i64) {
        let db = Db::open_in_memory().unwrap();
        let g = GameInstall {
            path: "/games/cp".into(),
            store: Store::Manual,
            proton_prefix: None,
            build_id: None,
            exe_file_version: None,
            exe_product_version: Some("2.31".into()),
            frameworks: vec![],
            launch_options: None,
            warnings: vec![],
        };
        let id = db.upsert_game(&g).unwrap();
        (db, id)
    }

    fn add_mod(db: &Db, game_id: i64, name: &str, source: &str, nexus: Option<(i64, i64)>, source_ref: Option<&str>) -> i64 {
        let game = db.game(game_id).unwrap();
        db.insert_mod(
            &game,
            &NewMod {
                name: name.into(),
                source: source.into(),
                nexus_mod_id: nexus.map(|n| n.0),
                nexus_file_id: nexus.map(|n| n.1),
                source_ref: source_ref.map(Into::into),
                archive_name: format!("{name}.zip"),
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn collection(revision: u32, mods: Vec<CollectionMod>) -> Collection {
        Collection {
            slug: "rcwfx9".into(),
            name: "Night City".into(),
            author: Some("someone".into()),
            summary: None,
            revision: Some(revision),
            game_version: Some("2.31".into()),
            mods,
            external: vec![],
            page_url: String::new(),
            changelog: None,
            image: None,
        }
    }

    #[test]
    fn keys_survive_updates() {
        let (db, g) = setup();
        add_mod(&db, g, "CET", "nexus", Some((107, 1)), None);
        add_mod(&db, g, "ArchiveXL", "github", None, Some("psiberx/cp2077-archive-XL"));
        add_mod(&db, g, " My Mod ", "manual", None, None);
        let keys: Vec<String> = db.mods(g).unwrap().iter().map(mod_key).collect();
        assert_eq!(keys, ["nexus:107", "github:psiberx/cp2077-archive-xl", "name:my mod"]);
    }

    #[test]
    fn tracks_collections_and_reports_their_state() {
        let (db, g) = setup();
        track(&db, g, &collection(3, vec![cm(1, 10, false), cm(2, 20, false), cm(3, 30, true), cm(4, 40, false)])).unwrap();
        add_mod(&db, g, "one", "nexus", Some((1, 10)), None);
        let two = add_mod(&db, g, "two", "nexus", Some((2, 19)), None);
        let four = add_mod(&db, g, "four", "nexus", Some((4, 40)), None);
        db.set_mod_status(four, crate::db::STATUS_DISABLED).unwrap();

        let t = tracked(&db, g).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].revision, Some(3));
        assert_eq!(t[0].mods.iter().map(|m| m.mod_id).collect::<Vec<_>>(), [1, 2, 3, 4], "kept in collection order");
        assert!(!t[0].update_available());

        let s = status(&t[0].mods, &db.mods(g).unwrap());
        assert!(matches!(s[0].state, ModState::Installed { .. }));
        assert_eq!(s[1].state, ModState::OtherFile { installed_id: two, installed_version: None });
        assert_eq!(s[2].state, ModState::Missing);
        assert_eq!(s[3].state, ModState::Disabled { installed_id: four });
        assert_eq!(counts(&s), StatusCounts { installed: 1, disabled: 1, other_file: 1, missing: 1, missing_required: 0 });

        set_latest_revision(&db, g, "rcwfx9", Some(5)).unwrap();
        assert!(tracked(&db, g).unwrap()[0].update_available());
        // Moving to the new revision replaces the stored mod list.
        track(&db, g, &collection(5, vec![cm(1, 11, false)])).unwrap();
        let t = tracked_one(&db, g, "rcwfx9").unwrap().unwrap();
        assert_eq!((t.revision, t.latest_revision, t.mods.len()), (Some(5), Some(5), 1));
        untrack(&db, g, "rcwfx9").unwrap();
        assert!(tracked(&db, g).unwrap().is_empty());
        let left: i64 = db.conn.query_row("SELECT count(*) FROM collection_mods", [], |r| r.get(0)).unwrap();
        assert_eq!(left, 0, "mods go with the collection");
    }

    #[test]
    fn diffs_revisions_by_mod() {
        let old = vec![cm(1, 10, false), cm(2, 20, false), cm(3, 30, false)];
        let new = vec![cm(4, 40, false), cm(1, 10, false), cm(2, 21, true)];
        let d = diff(&old, &new);
        assert_eq!(d.added.iter().map(|m| m.mod_id).collect::<Vec<_>>(), [4]);
        assert_eq!(d.removed.iter().map(|m| m.mod_id).collect::<Vec<_>>(), [3]);
        assert_eq!(d.changed.len(), 1);
        assert_eq!((d.changed[0].0.file_id, d.changed[0].1.file_id), (20, 21));
        assert_eq!(diff(&old, &old), RevisionDiff::default());
    }

    #[test]
    fn manages_custom_categories() {
        let (db, g) = setup();
        assert_eq!(add_category(&db, "  Visuals \t and  UI ", Some("#FF0000")).unwrap(), "Visuals and UI");
        assert_eq!(add_category(&db, "visuals AND ui", None).unwrap(), "Visuals and UI", "case-insensitive duplicate");
        add_category(&db, "Gameplay", Some("red")).unwrap();
        assert!(add_category(&db, "   ", None).is_err());
        assert!(add_category(&db, &"x".repeat(61), None).is_err());
        let cats = categories(&db).unwrap();
        assert_eq!(cats, [
            CustomCategory { name: "Visuals and UI".into(), color: Some("#ff0000".into()) },
            CustomCategory { name: "Gameplay".into(), color: None },
        ]);

        add_mod(&db, g, "CET", "nexus", Some((107, 1)), None);
        set_mod_category(&db, "nexus:107", Some("gameplay")).unwrap();
        assert_eq!(mod_categories(&db).unwrap()["nexus:107"], "Gameplay");
        assert!(set_mod_category(&db, "nexus:107", Some("Nope")).is_err());

        edit_category(&db, "Gameplay", "Core", None).unwrap();
        assert_eq!(mod_categories(&db).unwrap()["nexus:107"], "Core", "mods follow a rename");
        edit_category(&db, "Core", "core", Some("#00ff00")).unwrap();
        assert!(edit_category(&db, "core", "Visuals and UI", None).is_err(), "can't take another's name");
        delete_category(&db, "core").unwrap();
        assert!(mod_categories(&db).unwrap().is_empty(), "mods are uncategorized, not removed");
        assert_eq!(db.mods(g).unwrap().len(), 1);
        set_mod_category(&db, "nexus:107", Some("Visuals and UI")).unwrap();
        set_mod_category(&db, "nexus:107", None).unwrap();
        assert!(mod_categories(&db).unwrap().is_empty());
    }

    #[test]
    fn exports_and_imports_mod_lists() {
        let (db, g) = setup();
        add_category(&db, "Core", Some("#112233")).unwrap();
        add_mod(&db, g, "CET", "nexus", Some((107, 1)), None);
        add_mod(&db, g, "ArchiveXL", "github", None, Some("psiberx/cp2077-archive-xl"));
        set_mod_category(&db, "nexus:107", Some("Core")).unwrap();
        track(&db, g, &collection(3, vec![cm(107, 1, false)])).unwrap();
        let out = export(&db, g).unwrap();
        assert_eq!(out.mods.len(), 2);
        assert_eq!(out.mods[0].category.as_deref(), Some("Core"));
        assert_eq!(out.collections[0].revision, Some(3));
        let json = serde_json::to_vec_pretty(&out).unwrap();
        assert_eq!(parse_modlist(&json).unwrap(), out, "round-trips");

        // Into a fresh library with only ArchiveXL installed.
        let (db2, g2) = setup();
        add_mod(&db2, g2, "ArchiveXL", "github", None, Some("PSIBERX/cp2077-archive-xl"));
        let mut l = parse_modlist(&json).unwrap();
        l.mods[1].category = Some("Frameworks".into());
        let r = import(&db2, g2, &l).unwrap();
        assert_eq!(r.categories_added, 2, "Core from the list, Frameworks from a mod");
        assert_eq!(r.assigned, 1);
        assert_eq!(mod_categories(&db2).unwrap()["github:psiberx/cp2077-archive-xl"], "Frameworks");
        assert_eq!(r.not_installed.iter().map(|m| m.nexus_mod_id).collect::<Vec<_>>(), [Some(107)]);
        assert_eq!(r.collections.len(), 1);
        // Importing again adds nothing new.
        assert_eq!(import(&db2, g2, &l).unwrap().categories_added, 0);
    }

    #[test]
    fn refuses_bad_mod_lists() {
        assert!(parse_modlist(b"not json").is_err());
        assert!(parse_modlist(br#"{"format":"something-else","version":1}"#).is_err());
        assert!(parse_modlist(br#"{"format":"cpmx2077-modlist","version":99}"#).unwrap_err().to_string().contains("newer"));
        assert!(parse_modlist(&vec![b' '; MAX_MODLIST_BYTES + 1]).is_err());
        let l = parse_modlist(
            br#"{"format":"cpmx2077-modlist","version":1,
                 "categories":[{"name":"  ok  ","color":"javascript:x"},{"name":""}],
                 "mods":[{"name":"a\u0007b","source":"nexus","nexus_mod_id":-4},
                         {"name":"","source":"nexus"},
                         {"name":"x","source":"../etc"}],
                 "collections":[{"slug":"../x"},{"slug":"ABC123","revision":2}]}"#,
        )
        .unwrap();
        assert_eq!(l.categories, [CustomCategory { name: "ok".into(), color: None }]);
        assert_eq!(l.mods.len(), 1);
        assert_eq!((l.mods[0].name.as_str(), l.mods[0].nexus_mod_id, l.mods[0].enabled), ("ab", None, true));
        assert_eq!(l.collections, [ModListCollection { slug: "abc123".into(), name: None, revision: Some(2) }]);
    }
}
