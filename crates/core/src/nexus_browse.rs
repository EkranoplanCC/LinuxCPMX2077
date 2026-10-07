//! Browsing Nexus Mods from inside the app. Search and every list (trending,
//! latest, most endorsed...) go through the v2 GraphQL API, so they all page
//! the same way; mod pages through v1. Everything here is read-only, cached
//! (and saved to disk, see [`crate::nexus_cache`]) and sent at
//! [`Priority::Browse`] so it can't eat the quota downloads need.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::nexus::{Client, FileInfo, ModInfo, Priority, now_unix, parse_time};
use crate::nexus_cache::{DISK_MAX_AGE, RequestRecord, Served, started_ms};
use crate::nexus_markup::{self, Node};
use crate::{Error, NEXUS_GAME_DOMAIN, Result};

const LIST_TTL: Duration = Duration::from_secs(5 * 60);
const DETAIL_TTL: Duration = Duration::from_secs(10 * 60);
pub const DEFAULT_PAGE: u32 = 20;
/// Nexus returns at most 80 mods per request, whatever is asked for.
pub const MAX_PAGE: u32 = 80;
const MAX_OFFSET: u32 = 100_000;
/// Nexus' id for Cyberpunk 2077 (some GraphQL filters need it).
pub const NEXUS_GAME_ID: i64 = 3333;
/// "Trending" is the most downloaded of the mods added in this many days.
pub const TRENDING_DAYS: i64 = 14;
const MAX_QUERY_CHARS: usize = 100;
const MAX_DESCRIPTION_CHARS: usize = 20_000;

/// One mod in a result list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModCard {
    pub mod_id: i64,
    pub name: String,
    pub summary: Option<String>,
    pub author: Option<String>,
    pub version: Option<String>,
    /// Only ever a `https://staticdelivery.nexusmods.com/` URL.
    pub picture_url: Option<String>,
    pub endorsements: Option<i64>,
    pub downloads: Option<i64>,
    pub created: Option<i64>,
    pub updated: Option<i64>,
    pub adult: bool,
    /// Nexus category (v1 lists only; search results don't carry it).
    pub category_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Page {
    pub mods: Vec<ModCard>,
    /// Total matches, as Nexus reports it.
    pub total: Option<i64>,
    pub offset: u32,
    pub count: u32,
    /// Results left out because they are marked adult and the user hasn't
    /// opted in.
    pub hidden_adult: u32,
    /// When Nexus sent this page (Unix seconds); older than now when it came
    /// from the cache.
    pub fetched_at: i64,
    /// Nexus couldn't be asked, so this is a saved copy (see `fetched_at`).
    pub saved_copy: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sort {
    /// Best match for the search text; most endorsed without text.
    #[default]
    Relevance,
    Endorsements,
    Downloads,
    /// Mods that have had an update, newest update first.
    Updated,
    Created,
    /// Most downloaded of the mods added in the last [`TRENDING_DAYS`] days.
    Trending,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Search {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub sort: Sort,
    #[serde(default)]
    pub offset: u32,
    #[serde(default)]
    pub count: u32,
    /// Only mods in this Nexus category (by name, as `categories()` lists it).
    #[serde(default)]
    pub category: Option<String>,
    /// Ask Nexus even if a recent copy is cached.
    #[serde(default)]
    pub refresh: bool,
}

/// A mod category on Nexus, e.g. "Gameplay" or "Appearance".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Category {
    pub category_id: i64,
    pub name: String,
    pub parent: Option<i64>,
}

/// One step of a file's update chain: the author marked `new_file_id` as
/// the replacement of `old_file_id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileUpdate {
    pub old_file_id: i64,
    pub new_file_id: i64,
    pub uploaded_timestamp: Option<i64>,
}

/// The newest file that replaces `installed`, following the author's update
/// chain; without a chain, a newer main file with the same name.
pub fn newer_file<'a>(installed: i64, files: &'a [FileInfo], updates: &[FileUpdate]) -> Option<&'a FileInfo> {
    let gone = |f: &FileInfo| matches!(f.category_name.as_deref(), Some("ARCHIVED" | "DELETED"));
    let mut cur = installed;
    let mut seen = vec![cur];
    while let Some(u) = updates.iter().find(|u| u.old_file_id == cur) {
        if seen.contains(&u.new_file_id) {
            break;
        }
        cur = u.new_file_id;
        seen.push(cur);
    }
    if cur != installed {
        return files.iter().find(|f| f.file_id == cur && !gone(f));
    }
    let old = files.iter().find(|f| f.file_id == installed)?;
    let old_time = old.uploaded_timestamp.unwrap_or(0);
    files
        .iter()
        .filter(|f| f.file_id != installed && !gone(f) && f.category_name.as_deref() == Some("MAIN"))
        .filter(|f| f.name.is_some() && f.name == old.name && f.uploaded_timestamp.unwrap_or(0) > old_time)
        .max_by_key(|f| f.uploaded_timestamp.unwrap_or(0))
}

#[derive(Debug, Clone, Serialize)]
pub struct ModDetails {
    pub info: ModInfo,
    /// The mod page description with BBCode/HTML removed.
    pub description_text: String,
    /// The description's formatting as a tree of known-safe elements, for
    /// showing it the way the website does.
    pub description: Vec<Node>,
    pub files: Vec<FileInfo>,
    pub file_updates: Vec<FileUpdate>,
    pub page_url: String,
    /// What the author lists under "Requirements" (absent if Nexus' GraphQL
    /// API didn't answer).
    pub requirements: Option<Requirements>,
    pub tags: Vec<String>,
    pub fetched_at: i64,
    pub saved_copy: bool,
}

/// The "Requirements" section of a mod page.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Requirements {
    /// Other Nexus mods this one needs.
    pub nexus: Vec<Requirement>,
    /// Things to get elsewhere (a link, not a Nexus mod).
    pub external: Vec<Requirement>,
    /// Expansions it needs, e.g. "Phantom Liberty".
    pub dlc: Vec<String>,
    /// How many mods list this one as a requirement.
    pub required_by: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Requirement {
    pub mod_id: Option<i64>,
    pub name: String,
    pub notes: Option<String>,
    /// Only for off-site requirements; https/http only.
    pub url: Option<String>,
}

/// Mod page on the website; `file_id` jumps to that file's entry on the
/// Files tab, where free accounts click "Mod Manager Download".
pub fn mod_page_url(mod_id: i64, file_id: Option<i64>) -> String {
    match file_id {
        Some(f) => format!("https://www.nexusmods.com/{NEXUS_GAME_DOMAIN}/mods/{mod_id}?tab=files&file_id={f}"),
        None => format!("https://www.nexusmods.com/{NEXUS_GAME_DOMAIN}/mods/{mod_id}"),
    }
}

/// What a pasted Nexus link or id points at.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum NexusRef {
    Mod { mod_id: i64 },
    /// `revision` is `None` for "the latest published one".
    Collection { slug: String, revision: Option<u32> },
}

/// A Cyberpunk 2077 mod or collection from a numeric id, a website link
/// (old `/cyberpunk2077/mods/N` and new `/games/cyberpunk2077/...` forms) or a
/// collection `nxm://` link. Anything else, including other games, is `None`.
pub fn parse_nexus_ref(input: &str) -> Option<NexusRef> {
    let s = input.trim();
    if let Ok(id) = s.parse::<i64>() {
        return (id > 0).then_some(NexusRef::Mod { mod_id: id });
    }
    let url = url::Url::parse(s).ok()?;
    let segs: Vec<&str> = url.path_segments()?.filter(|p| !p.is_empty()).collect();
    let rest: &[&str] = match url.scheme() {
        "https" | "http" => {
            let host = url.host_str()?.to_ascii_lowercase();
            if host != "nexusmods.com" && !host.ends_with(".nexusmods.com") {
                return None;
            }
            match segs.as_slice() {
                ["games", game, rest @ ..] | [game, rest @ ..] if game.eq_ignore_ascii_case(NEXUS_GAME_DOMAIN) => rest,
                _ => return None,
            }
        }
        // nxm://cyberpunk2077/collections/<slug>/revisions/<n>
        "nxm" if url.host_str()?.eq_ignore_ascii_case(NEXUS_GAME_DOMAIN) => &segs,
        _ => return None,
    };
    match rest {
        ["mods", id, ..] => id.parse().ok().filter(|i| *i > 0).map(|mod_id| NexusRef::Mod { mod_id }),
        ["collections", slug, tail @ ..] if is_collection_slug(slug) => {
            let revision = match tail {
                ["revisions", n, ..] => Some(n.parse().ok()?),
                _ => None,
            };
            Some(NexusRef::Collection { slug: slug.to_ascii_lowercase(), revision })
        }
        _ => None,
    }
}

/// Collection ids are short and alphanumeric (`rcwfx9`).
pub fn is_collection_slug(s: &str) -> bool {
    (1..=32).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// One mod file a collection asks for.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CollectionMod {
    pub mod_id: i64,
    pub file_id: i64,
    pub mod_name: String,
    pub file_name: Option<String>,
    pub version: Option<String>,
    pub optional: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Collection {
    pub slug: String,
    pub name: String,
    pub author: Option<String>,
    pub summary: Option<String>,
    pub revision: Option<u32>,
    pub game_version: Option<String>,
    pub mods: Vec<CollectionMod>,
    /// Files hosted outside Nexus, which the user has to fetch by hand.
    pub external: Vec<String>,
    pub page_url: String,
    /// What the author says changed in this revision.
    pub changelog: Option<String>,
    /// Only ever a `https://media.nexusmods.com/` URL.
    pub image: Option<String>,
}

pub fn collection_page_url(slug: &str) -> String {
    format!("https://www.nexusmods.com/games/{NEXUS_GAME_DOMAIN}/collections/{slug}")
}

const COLLECTION_QUERY: &str = "query CollectionRevision($slug: String!, $domain: String, $revision: Int) {
  collectionRevision(slug: $slug, domainName: $domain, revision: $revision, viewAdultContent: true) {
    revisionNumber
    gameVersions { reference }
    collectionChangelog { description }
    collection { name summary user { name } tileImage { url } }
    modFiles { fileId optional file { fileId name version mod { modId name } } }
    externalResources { name }
  }
}";

fn collection_from_graphql(slug: &str, data: &Value) -> Result<Collection> {
    let rev = data
        .get("collectionRevision")
        .filter(|v| !v.is_null())
        .ok_or_else(|| Error::Nexus(format!("collection `{slug}` was not found (it may be hidden or removed)")))?;
    let text = |v: Option<&Value>, max: usize| clean_line(v.and_then(Value::as_str), max);
    let mut mods: Vec<CollectionMod> = rev
        .get("modFiles")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    let file = m.get("file").filter(|f| !f.is_null());
                    let file_id = m.get("fileId").or_else(|| file?.get("fileId")).and_then(as_id)?;
                    let md = file?.get("mod")?;
                    let mod_id = md.get("modId").and_then(as_id)?;
                    Some(CollectionMod {
                        mod_id,
                        file_id,
                        mod_name: text(md.get("name"), 200).unwrap_or_else(|| format!("Mod {mod_id}")),
                        file_name: text(file?.get("name"), 200),
                        version: text(file?.get("version"), 50),
                        optional: m.get("optional").and_then(Value::as_bool).unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    mods.dedup_by_key(|m| (m.mod_id, m.file_id));
    let coll = rev.get("collection");
    Ok(Collection {
        slug: slug.to_string(),
        name: text(coll.and_then(|c| c.get("name")), 200).unwrap_or_else(|| slug.to_string()),
        author: text(coll.and_then(|c| c.pointer("/user/name")), 100),
        summary: text(coll.and_then(|c| c.get("summary")), 1000),
        revision: rev.get("revisionNumber").and_then(Value::as_u64).map(|n| n as u32),
        game_version: text(rev.pointer("/gameVersions/0/reference"), 50),
        mods,
        external: rev
            .get("externalResources")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|r| text(r.get("name"), 200)).collect())
            .unwrap_or_default(),
        page_url: collection_page_url(slug),
        changelog: text(rev.pointer("/collectionChangelog/description"), 2000),
        image: safe_image(coll.and_then(|c| c.pointer("/tileImage/url")).and_then(Value::as_str)),
    })
}

/// One collection in a browse or search result.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CollectionCard {
    pub slug: String,
    pub name: String,
    pub author: Option<String>,
    pub summary: Option<String>,
    pub category: Option<String>,
    pub endorsements: Option<i64>,
    pub downloads: Option<i64>,
    /// Latest published revision.
    pub revision: Option<u32>,
    pub mod_count: Option<i64>,
    pub total_size: Option<u64>,
    pub game_version: Option<String>,
    pub updated: Option<i64>,
    pub adult: bool,
    /// Only ever a `https://media.nexusmods.com/` URL.
    pub image: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CollectionPage {
    pub collections: Vec<CollectionCard>,
    pub total: Option<i64>,
    pub offset: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollectionSort {
    #[default]
    Endorsements,
    Downloads,
    Rating,
    Updated,
    Created,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CollectionSearch {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub sort: CollectionSort,
    #[serde(default)]
    pub offset: u32,
    #[serde(default)]
    pub count: u32,
}

const COLLECTIONS_QUERY: &str = "query BrowseCollections($filter: CollectionsSearchFilter, $sort: [CollectionsSearchSort!], $offset: Int, $count: Int) {
  collectionsV2(filter: $filter, sort: $sort, offset: $offset, count: $count) {
    totalCount
    nodes {
      slug name summary endorsements totalDownloads updatedAt
      user { name }
      category { name }
      tileImage { url }
      latestPublishedRevision { revisionNumber modCount totalSize adultContent gameVersions { reference } }
    }
  }
}";

/// Variables for [`COLLECTIONS_QUERY`]. Adult collections are filtered out by
/// Nexus unless the user opted in.
pub fn collection_variables(q: &CollectionSearch, include_adult: bool) -> Value {
    let mut filter = json!({ "gameDomain": [{ "value": NEXUS_GAME_DOMAIN, "op": "EQUALS" }] });
    let text = clean_query(&q.text);
    if !text.is_empty() {
        // Nexus' general search over name, summary and author; a contains match.
        filter["generalSearch"] = json!([{ "value": text, "op": "WILDCARD" }]);
    }
    if !include_adult {
        filter["adultContent"] = json!([{ "value": false, "op": "EQUALS" }]);
    }
    let key = match q.sort {
        CollectionSort::Endorsements => "endorsements",
        CollectionSort::Downloads => "downloads",
        CollectionSort::Rating => "rating",
        CollectionSort::Updated => "updatedAt",
        CollectionSort::Created => "createdAt",
    };
    let count = if q.count == 0 { DEFAULT_PAGE } else { q.count.min(MAX_PAGE) };
    json!({
        "filter": filter,
        "sort": [{ key: { "direction": "DESC" } }],
        "offset": q.offset.min(MAX_OFFSET),
        "count": count,
    })
}

fn collection_card(n: &Value) -> Option<CollectionCard> {
    let slug = n.get("slug").and_then(Value::as_str).filter(|s| is_collection_slug(s))?.to_ascii_lowercase();
    let text = |p: &str, max: usize| clean_line(n.pointer(p).and_then(Value::as_str), max);
    let rev = n.get("latestPublishedRevision").filter(|r| !r.is_null());
    // BigInt sizes come back as strings.
    let big = |v: Option<&Value>| v.and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()));
    Some(CollectionCard {
        name: text("/name", 200).unwrap_or_else(|| slug.clone()),
        author: text("/user/name", 100),
        summary: text("/summary", 1000),
        category: text("/category/name", 100),
        endorsements: n.get("endorsements").and_then(Value::as_i64),
        downloads: n.get("totalDownloads").and_then(Value::as_i64),
        revision: rev.and_then(|r| r.get("revisionNumber")).and_then(Value::as_u64).map(|r| r as u32),
        mod_count: rev.and_then(|r| r.get("modCount")).and_then(Value::as_i64),
        total_size: big(rev.and_then(|r| r.get("totalSize"))),
        game_version: clean_line(rev.and_then(|r| r.pointer("/gameVersions/0/reference")).and_then(Value::as_str), 50),
        updated: n.get("updatedAt").and_then(Value::as_str).and_then(parse_time),
        adult: rev.and_then(|r| r.get("adultContent")).and_then(Value::as_bool).unwrap_or(false),
        image: safe_image(n.pointer("/tileImage/url").and_then(Value::as_str)),
        slug,
    })
}

/// Something a mod needs, as its author listed it on Nexus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListedRequirement {
    /// The Nexus mod, for requirements hosted on Nexus (this game only).
    pub mod_id: Option<i64>,
    pub name: String,
    /// The author's note, e.g. "only for the ArchiveXL version".
    pub notes: Option<String>,
    /// A link for requirements hosted elsewhere.
    pub url: Option<String>,
    /// A game expansion (Phantom Liberty) rather than a mod.
    pub dlc: bool,
}

const REQUIREMENTS_QUERY: &str = "query Requirements($ids: [CompositeDomainWithIdInput!]!, $count: Int) {
  legacyModsByDomain(ids: $ids, count: $count) {
    nodes {
      modId
      modRequirements {
        nexusRequirements { nodes { modId modName url notes externalRequirement gameId } }
        dlcRequirements { gameExpansion { name } }
      }
    }
  }
}";

/// Mods per requirements request.
const REQUIREMENTS_BATCH: usize = 50;

fn batch_requirements_from_graphql(data: &Value) -> Vec<(i64, Vec<ListedRequirement>)> {
    let nodes = data.pointer("/legacyModsByDomain/nodes").and_then(Value::as_array).cloned().unwrap_or_default();
    nodes
        .iter()
        .filter_map(|n| {
            let id = n.get("modId").and_then(as_id)?;
            let reqs = n.get("modRequirements").filter(|r| !r.is_null());
            let mut out: Vec<ListedRequirement> = Vec::new();
            for r in reqs.and_then(|r| r.pointer("/nexusRequirements/nodes")).and_then(Value::as_array).into_iter().flatten() {
                let external = r.get("externalRequirement").and_then(Value::as_bool).unwrap_or(false);
                let same_game = r.get("gameId").and_then(as_id) == Some(NEXUS_GAME_ID);
                let mod_id = if external || !same_game { None } else { r.get("modId").and_then(as_id).filter(|m| *m != id) };
                let url = clean_line(r.get("url").and_then(Value::as_str), 500)
                    .filter(|u| url::Url::parse(u).is_ok_and(|u| u.scheme() == "https" || u.scheme() == "http"));
                let Some(name) = clean_line(r.get("modName").and_then(Value::as_str), 200) else { continue };
                if mod_id.is_none() && url.is_none() && !external {
                    // Another game's mod with no link: nothing to act on.
                    continue;
                }
                if mod_id.is_some_and(|m| out.iter().any(|o| o.mod_id == Some(m))) {
                    continue;
                }
                out.push(ListedRequirement { mod_id, name, notes: clean_line(r.get("notes").and_then(Value::as_str), 500), url, dlc: false });
            }
            for d in reqs.and_then(|r| r.get("dlcRequirements")).and_then(Value::as_array).into_iter().flatten() {
                if let Some(name) = clean_line(d.pointer("/gameExpansion/name").and_then(Value::as_str), 100) {
                    out.push(ListedRequirement { mod_id: None, name, notes: None, url: None, dlc: true });
                }
            }
            Some((id, out))
        })
        .collect()
}

/// GraphQL ids come back as numbers or strings depending on the field.
fn as_id(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str()?.parse().ok()).filter(|i| *i > 0)
}

/// The UI's content security policy only allows images from these hosts.
fn safe_image(u: Option<&str>) -> Option<String> {
    let url = url::Url::parse(u?.trim()).ok()?;
    let host = url.host_str()?;
    (url.scheme() == "https" && (host == "staticdelivery.nexusmods.com" || host == "media.nexusmods.com")).then(|| url.to_string())
}

fn clean_line(s: Option<&str>, max: usize) -> Option<String> {
    let s: String = s?.chars().filter(|c| !c.is_control() || *c == '\n').take(max).collect();
    let s = decode_entities(s.trim());
    (!s.is_empty()).then_some(s)
}

fn card_from_graphql(n: &Value) -> Option<ModCard> {
    let s = |k: &str| n.get(k).and_then(Value::as_str);
    let i = |k: &str| n.get(k).and_then(Value::as_i64);
    Some(ModCard {
        mod_id: i("modId")?,
        name: clean_line(s("name"), 300)?,
        summary: clean_line(s("summary"), 1000),
        author: clean_line(s("author").or_else(|| n.pointer("/uploader/name").and_then(Value::as_str)), 200),
        version: clean_line(s("version"), 100),
        picture_url: safe_image(s("thumbnailUrl")).or_else(|| safe_image(s("pictureUrl"))),
        endorsements: i("endorsements"),
        downloads: i("downloads"),
        created: s("createdAt").and_then(parse_time),
        updated: s("updatedAt").and_then(parse_time),
        adult: n.get("adultContent").and_then(Value::as_bool).unwrap_or(false),
        category_id: None,
    })
}

fn split_adult(cards: Vec<ModCard>, include_adult: bool) -> (Vec<ModCard>, u32) {
    if include_adult {
        return (cards, 0);
    }
    let before = cards.len();
    let kept: Vec<_> = cards.into_iter().filter(|c| !c.adult).collect();
    let hidden = (before - kept.len()) as u32;
    (kept, hidden)
}

const MODS_QUERY: &str = "query BrowseMods($filter: ModsFilter, $sort: [ModsSort!], $offset: Int, $count: Int) {
  mods(filter: $filter, sort: $sort, offset: $offset, count: $count) {
    totalCount
    nodes {
      modId name summary version author pictureUrl thumbnailUrl
      endorsements downloads createdAt updatedAt adultContent
      uploader { name }
    }
  }
}";

/// Search text as sent to Nexus: no control characters, bounded length.
pub fn clean_query(s: &str) -> String {
    let s: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    s.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(MAX_QUERY_CHARS).collect()
}

/// `stemmed`: Nexus' full-text name match (what the website uses). Without
/// it, Nexus' WILDCARD name match, which already matches anywhere in the
/// name and treats `*` literally.
pub fn graphql_variables(q: &Search, stemmed: bool) -> Value {
    let text = clean_query(&q.text);
    let mut filter = json!({ "gameDomainName": [{ "value": NEXUS_GAME_DOMAIN, "op": "EQUALS" }] });
    if !text.is_empty() {
        if stemmed {
            filter["nameStemmed"] = json!([{ "value": text, "op": "MATCHES" }]);
        } else {
            let wild = clean_query(&text.replace(['*', '?'], " "));
            filter["name"] = json!([{ "value": wild, "op": "WILDCARD" }]);
        }
    }
    if let Some(cat) = q.category.as_deref().map(clean_query).filter(|c| !c.is_empty()) {
        filter["categoryName"] = json!([{ "value": cat, "op": "EQUALS" }]);
    }
    match q.sort {
        Sort::Updated => filter["hasUpdated"] = json!([{ "value": true }]),
        Sort::Trending => {
            // Whole days, so the request (and its cache key) stays the same all day.
            let since = (now_unix() / 86400 - TRENDING_DAYS) * 86400;
            filter["createdAt"] = json!([{ "value": since.to_string(), "op": "GTE" }]);
        }
        _ => {}
    }
    let key = match q.sort {
        Sort::Relevance if !text.is_empty() => "relevance",
        Sort::Relevance | Sort::Endorsements => "endorsements",
        Sort::Downloads | Sort::Trending => "downloads",
        Sort::Updated => "updatedAt",
        Sort::Created => "createdAt",
    };
    let count = if q.count == 0 { DEFAULT_PAGE } else { q.count.min(MAX_PAGE) };
    json!({
        "filter": filter,
        "sort": [{ key: { "direction": "DESC" } }],
        "offset": q.offset.min(MAX_OFFSET),
        "count": count,
    })
}

/// A response and where it came from.
struct Fetched {
    value: Value,
    fetched_at: i64,
    saved: bool,
}

const MOD_EXTRAS_QUERY: &str = "query ModExtras($filter: ModsFilter) {
  mods(filter: $filter, count: 1) {
    nodes {
      modId
      tags { name }
      modRequirements {
        nexusRequirements { nodes { modId modName notes url externalRequirement } }
        dlcRequirements { gameExpansion { name } notes }
        modsRequiringThisMod { totalCount }
      }
    }
  }
}";

fn requirements_from_graphql(data: &Value) -> (Option<Requirements>, Vec<String>) {
    let Some(node) = data.pointer("/mods/nodes/0") else { return (None, vec![]) };
    let text = |v: Option<&Value>, max: usize| clean_line(v.and_then(Value::as_str), max);
    let tags = node
        .get("tags")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|t| text(t.get("name"), 60)).take(50).collect())
        .unwrap_or_default();
    let Some(r) = node.get("modRequirements").filter(|r| !r.is_null()) else { return (None, tags) };
    let mut out = Requirements {
        required_by: r.pointer("/modsRequiringThisMod/totalCount").and_then(Value::as_i64),
        ..Default::default()
    };
    for n in r.pointer("/nexusRequirements/nodes").and_then(Value::as_array).into_iter().flatten().take(200) {
        let Some(name) = text(n.get("modName"), 200) else { continue };
        let external = n.get("externalRequirement").and_then(Value::as_bool).unwrap_or(false);
        let req = Requirement {
            mod_id: if external { None } else { n.get("modId").and_then(as_id) },
            name,
            notes: text(n.get("notes"), 500),
            url: text(n.get("url"), 500).filter(|u| nexus_markup::safe_link(u).is_some()),
        };
        if external { out.external.push(req) } else { out.nexus.push(req) }
    }
    out.dlc = r
        .get("dlcRequirements")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|d| text(d.pointer("/gameExpansion/name"), 100)).collect())
        .unwrap_or_default();
    (Some(out), tags)
}

impl Client {
    /// `key` from the cache when it is younger than `ttl` (unless `refresh`),
    /// otherwise from `fetch`. When `fetch` fails and `allow_saved`, an
    /// older saved copy stands in.
    fn through_cache(
        &self,
        key: &str,
        ttl: Duration,
        refresh: bool,
        allow_saved: bool,
        what: (&str, &str, Option<&str>),
        fetch: impl FnOnce() -> Result<Value>,
    ) -> Result<Fetched> {
        let now = now_unix();
        let note = |served: Served, fetched_at: i64| {
            let rate = self.shared.rate();
            self.shared.log.push(RequestRecord {
                id: 0,
                at_ms: started_ms(),
                method: what.0.into(),
                endpoint: what.1.into(),
                label: Some(match what.2 {
                    Some(l) => format!("{l} · copy from {} s ago", now - fetched_at),
                    None => format!("copy from {} s ago", now - fetched_at),
                }),
                priority: "browse",
                served,
                status: None,
                duration_ms: 0,
                bytes: None,
                error: None,
                hourly_remaining: rate.hourly_remaining,
                daily_remaining: rate.daily_remaining,
            });
        };
        if !refresh && let Some(e) = self.shared.cache.fresh(key, ttl, now) {
            note(Served::Cache, e.fetched_at);
            return Ok(Fetched { value: e.value, fetched_at: e.fetched_at, saved: false });
        }
        match fetch() {
            Ok(value) => {
                self.shared.cache.store(key.to_string(), value.clone(), now);
                Ok(Fetched { value, fetched_at: now, saved: false })
            }
            Err(err) => {
                let saved = allow_saved
                    .then(|| self.shared.cache.get(key))
                    .flatten()
                    .filter(|e| now - e.fetched_at < DISK_MAX_AGE.as_secs() as i64);
                match saved {
                    Some(e) => {
                        log::warn!("Nexus request failed ({err}); showing a saved copy");
                        note(Served::Saved, e.fetched_at);
                        Ok(Fetched { value: e.value, fetched_at: e.fetched_at, saved: true })
                    }
                    None => Err(err),
                }
            }
        }
    }

    fn cached_v1(&self, path: &str, ttl: Duration, refresh: bool, allow_saved: bool) -> Result<Fetched> {
        let endpoint = format!("/v1{path}");
        self.through_cache(&format!("v1:{path}"), ttl, refresh, allow_saved, ("GET", &endpoint, None), || {
            self.get_with(Priority::Browse, path, &[])
        })
    }

    fn graphql(&self, query: &str, variables: Value, refresh: bool) -> Result<Fetched> {
        let body = json!({ "query": query, "variables": variables });
        // "query BrowseMods(...)" -> "BrowseMods", for the request log.
        let op = query.split_whitespace().nth(1).map(|w| w.split('(').next().unwrap_or(w).to_string());
        self.through_cache(&format!("gql:{body}"), LIST_TTL, refresh, true, ("POST", "/v2/graphql", op.as_deref()), || {
            let resp = self.send_api_as(Priority::Browse, self.http.post(&self.graphql).json(&body), op.as_deref())?;
            let status = resp.status();
            let text = resp.text()?;
            let v: Value = serde_json::from_str(&text).map_err(|_| {
                Error::Nexus(format!(
                    "{status}: unexpected reply from the Nexus search API: {}",
                    text.chars().take(200).collect::<String>()
                ))
            })?;
            if let Some(err) = v.pointer("/errors/0/message").and_then(Value::as_str) {
                return Err(Error::Nexus(format!("search API: {}", err.chars().take(300).collect::<String>())));
            }
            if !status.is_success() {
                return Err(Error::Nexus(format!("{status}: Nexus search API request failed")));
            }
            v.get("data").cloned().filter(|d| !d.is_null()).ok_or_else(|| Error::Nexus("search API returned no data".into()))
        })
    }

    /// Search by name and/or list mods: trending, newest, latest updated,
    /// most endorsed or downloaded. Every list pages through all of Nexus.
    pub fn search(&self, q: &Search, include_adult: bool) -> Result<Page> {
        let has_text = !clean_query(&q.text).is_empty();
        let got = match self.graphql(MODS_QUERY, graphql_variables(q, true), q.refresh) {
            // If Nexus rejects the full-text filter, fall back to a wildcard.
            Err(Error::Nexus(msg)) if has_text && msg.starts_with("search API:") => {
                log::warn!("stemmed search failed ({msg}); retrying with wildcard");
                self.graphql(MODS_QUERY, graphql_variables(q, false), q.refresh)?
            }
            r => r?,
        };
        let data = &got.value;
        let mods = data.pointer("/mods/nodes").and_then(Value::as_array).cloned().unwrap_or_default();
        let total = data.pointer("/mods/totalCount").and_then(Value::as_i64);
        let returned = mods.len() as u32;
        let (mods, hidden_adult) = split_adult(mods.iter().filter_map(card_from_graphql).collect(), include_adult);
        Ok(Page {
            mods,
            total,
            offset: q.offset.min(MAX_OFFSET),
            count: returned,
            hidden_adult,
            fetched_at: got.fetched_at,
            saved_copy: got.saved,
        })
    }

    /// The mods of a collection revision (the latest when `revision` is `None`).
    pub fn collection(&self, slug: &str, revision: Option<u32>) -> Result<Collection> {
        if !is_collection_slug(slug) {
            return Err(Error::Nexus(format!("`{slug}` is not a collection id")));
        }
        let vars = json!({ "slug": slug, "domain": NEXUS_GAME_DOMAIN, "revision": revision });
        collection_from_graphql(slug, &self.graphql(COLLECTION_QUERY, vars, false)?.value)
    }

    /// Search or list the game's collections (cached briefly).
    pub fn search_collections(&self, q: &CollectionSearch, include_adult: bool) -> Result<CollectionPage> {
        let data = self.graphql(COLLECTIONS_QUERY, collection_variables(q, include_adult), false)?.value;
        let nodes = data.pointer("/collectionsV2/nodes").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(CollectionPage {
            collections: nodes.iter().filter_map(collection_card).collect(),
            total: data.pointer("/collectionsV2/totalCount").and_then(Value::as_i64),
            offset: q.offset.min(MAX_OFFSET),
        })
    }

    /// What each of `mod_ids` needs according to its Nexus page, in batches.
    /// Mods Nexus doesn't know are left out.
    pub fn requirements(&self, mod_ids: &[i64]) -> Result<Vec<(i64, Vec<ListedRequirement>)>> {
        let mut ids: Vec<i64> = mod_ids.iter().copied().filter(|i| *i > 0).collect();
        ids.sort_unstable();
        ids.dedup();
        let mut out = Vec::new();
        for chunk in ids.chunks(REQUIREMENTS_BATCH) {
            let list: Vec<Value> = chunk.iter().map(|i| json!({ "gameDomain": NEXUS_GAME_DOMAIN, "modId": i })).collect();
            let data = self.graphql(REQUIREMENTS_QUERY, json!({ "ids": list, "count": chunk.len() }), false)?.value;
            out.extend(batch_requirements_from_graphql(&data));
        }
        Ok(out)
    }

    /// Mod page, file list, requirements and tags, for browsing (cached).
    pub fn mod_details(&self, mod_id: i64, refresh: bool) -> Result<ModDetails> {
        let got = self.cached_v1(&format!("/games/{NEXUS_GAME_DOMAIN}/mods/{mod_id}.json"), DETAIL_TTL, refresh, true)?;
        let info: ModInfo = serde_json::from_value(got.value)?;
        let files = self.cached_v1(&format!("/games/{NEXUS_GAME_DOMAIN}/mods/{mod_id}/files.json"), DETAIL_TTL, refresh, true)?;
        let (files, file_updates) = parse_files(files.value)?;
        let filter = json!({
            "gameId": [{ "value": NEXUS_GAME_ID.to_string(), "op": "EQUALS" }],
            "modId": [{ "value": mod_id.to_string(), "op": "EQUALS" }],
        });
        // The page still shows without them.
        let (requirements, tags) = match self.graphql(MOD_EXTRAS_QUERY, json!({ "filter": filter }), refresh) {
            Ok(f) => requirements_from_graphql(&f.value),
            Err(e) => {
                log::warn!("mod {mod_id}: no requirements from Nexus ({e})");
                (None, vec![])
            }
        };
        let raw = info.description.as_deref().unwrap_or_default();
        Ok(ModDetails {
            description_text: bbcode_to_text(raw),
            description: nexus_markup::to_nodes(raw),
            info,
            files,
            file_updates,
            page_url: mod_page_url(mod_id, None),
            requirements,
            tags,
            fetched_at: got.fetched_at,
            saved_copy: got.saved,
        })
    }

    /// A mod's files and the author's update chain between them (cached
    /// briefly; never an old saved copy, since update checks rely on it).
    pub fn files_with_updates(&self, mod_id: i64) -> Result<(Vec<FileInfo>, Vec<FileUpdate>)> {
        parse_files(self.cached_v1(&format!("/games/{NEXUS_GAME_DOMAIN}/mods/{mod_id}/files.json"), DETAIL_TTL, false, false)?.value)
    }

    /// Nexus' mod categories for the game (cached for a day).
    pub fn categories(&self) -> Result<Vec<Category>> {
        let v = self.cached_v1(&format!("/games/{NEXUS_GAME_DOMAIN}.json"), Duration::from_secs(24 * 3600), false, true)?.value;
        let mut out: Vec<Category> = v
            .get("categories")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|c| {
                        Some(Category {
                            category_id: c.get("category_id")?.as_i64()?,
                            name: clean_line(c.get("name")?.as_str(), 100)?,
                            // `false` for top-level categories.
                            parent: c.get("parent_category").and_then(Value::as_i64),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by_key(|c| c.name.to_lowercase());
        Ok(out)
    }
}

fn parse_files(v: Value) -> Result<(Vec<FileInfo>, Vec<FileUpdate>)> {
    #[derive(Deserialize)]
    struct Files {
        files: Vec<FileInfo>,
        #[serde(default)]
        file_updates: Vec<FileUpdate>,
    }
    let f: Files = serde_json::from_value(v)?;
    Ok((f.files, f.file_updates))
}

/// At most the first `max` bytes of `s`, cut back to a character boundary
/// (descriptions are full of em dashes and other multi-byte characters).
pub(crate) fn head(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

pub(crate) fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let end = head(rest, 12).find(';');
        let decoded = end.and_then(|e| {
            let ent = &rest[1..e];
            let c = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                _ => ent
                    .strip_prefix("#x")
                    .or_else(|| ent.strip_prefix("#X"))
                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                    .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                    .and_then(char::from_u32)
                    .filter(|c| !c.is_control() || *c == '\n'),
            };
            c.map(|c| (c, e + 1))
        });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Turn a Nexus description (BBCode with some HTML) into plain text.
/// The result is only ever shown as text, never parsed as markup.
pub fn bbcode_to_text(src: &str) -> String {
    // Tags whose contents are not readable text.
    const DROP_CONTENT: [&str; 3] = ["img", "youtube", "script"];
    const BBCODE_TAGS: [&str; 33] = [
        "b", "i", "u", "s", "size", "color", "font", "url", "img", "quote", "code", "list", "*", "center", "left",
        "right", "youtube", "spoiler", "line", "heading", "h1", "h2", "h3", "email", "table", "tr", "td", "th", "sup",
        "sub", "hr", "strike", "li",
    ];
    let src: String = src.chars().take(MAX_DESCRIPTION_CHARS * 2).collect();
    let mut out = String::new();
    let mut rest = src.as_str();
    while !rest.is_empty() {
        let c = rest.chars().next().unwrap();
        let close = match c {
            '[' => ']',
            '<' => '>',
            _ => {
                out.push(c);
                rest = &rest[c.len_utf8()..];
                continue;
            }
        };
        let Some(end) = head(rest, 300).find(close) else {
            out.push(c);
            rest = &rest[1..];
            continue;
        };
        let inner = &rest[1..end];
        let tag_name: String = inner
            .trim_start_matches('/')
            .chars()
            .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '*' || *ch == '_')
            .collect::<String>()
            .to_ascii_lowercase();
        // Unknown [words] are shown literally on Nexus too; any <tag> is HTML.
        let known = close == '>' || BBCODE_TAGS.contains(&tag_name.as_str());
        let looks_like_tag = known
            && !tag_name.is_empty()
            && inner.len() == inner.trim_start_matches('/').len() + usize::from(inner.starts_with('/'))
            && inner.trim_start_matches('/')[tag_name.len()..]
                .chars()
                .next()
                .is_none_or(|ch| matches!(ch, '=' | ' ' | '/' | '"'));
        if !looks_like_tag {
            out.push(c);
            rest = &rest[1..];
            continue;
        }
        rest = &rest[end + 1..];
        let closing = inner.starts_with('/');
        match tag_name.as_str() {
            "br" => out.push('\n'),
            "p" | "div" | "li" | "ul" | "ol" | "h1" | "h2" | "h3" | "h4" | "line" | "list" | "quote" | "code" | "center"
            | "heading" | "table" | "tr" => {
                if !out.ends_with('\n') {
                    out.push('\n')
                }
            }
            "*" => {
                let trimmed = out.trim_end_matches([' ', '\t']).len();
                out.truncate(trimmed);
                if !out.ends_with('\n') {
                    out.push('\n')
                }
                out.push_str("• ")
            }
            t if !closing && DROP_CONTENT.contains(&t) => {
                let end_tag = format!("{}/{t}{close}", if close == ']' { '[' } else { '<' });
                let lower = rest.to_ascii_lowercase();
                rest = match lower.find(&end_tag) {
                    Some(i) => &rest[i + end_tag.len()..],
                    None => "",
                };
            }
            _ => {}
        }
    }
    let text = decode_entities(&out);
    let mut cleaned = String::new();
    let mut blank = 0;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        cleaned.push_str(line);
        cleaned.push('\n');
    }
    let cleaned: String = cleaned.trim().chars().filter(|c| !c.is_control() || *c == '\n').collect();
    if cleaned.chars().count() > MAX_DESCRIPTION_CHARS {
        cleaned.chars().take(MAX_DESCRIPTION_CHARS).collect::<String>() + "…"
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nexus::{BROWSE_RESERVE, Shared};
    use crate::nexus_cache::Served;
    use crate::testutil::serve;

    fn rl(hourly: u32, daily: u32) -> Vec<(&'static str, String)> {
        vec![
            ("X-RL-Hourly-Limit", "100".into()),
            ("X-RL-Hourly-Remaining", hourly.to_string()),
            ("X-RL-Hourly-Reset", "2099-01-01 13:00:00 +0000".into()),
            ("X-RL-Daily-Limit", "20000".into()),
            ("X-RL-Daily-Remaining", daily.to_string()),
            ("X-RL-Daily-Reset", "2099-01-02 00:00:00 +0000".into()),
        ]
    }

    fn fixture(name: &str) -> String {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nexus").join(name);
        std::fs::read_to_string(p).unwrap()
    }

    fn client(addr: &str) -> Client {
        Client::with_endpoints("test-key", &format!("{addr}/v1"), &format!("{addr}/v2/graphql"), Shared::new()).unwrap()
    }

    #[test]
    fn lists_page_through_graphql_cache_and_track_quota() {
        let (addr, seen) = serve(vec![("graphql", 200, rl(99, 19876), fixture("search.json"))]);
        let c = client(&addr);
        let q = Search { sort: Sort::Trending, offset: 40, count: 40, ..Default::default() };
        let page = c.search(&q, false).unwrap();
        assert_eq!((page.offset, page.total, page.saved_copy), (40, Some(57), false));
        assert_eq!(page.hidden_adult, 1, "adult mod hidden by default");
        // Off-host image URLs are dropped rather than shown.
        assert!(page.mods.iter().all(|m| m.picture_url.as_deref().is_none_or(|u| u.starts_with("https://staticdelivery.nexusmods.com/"))));

        // The same list again comes from the cache, adult mods included or not.
        let again = c.search(&q, true).unwrap();
        assert_eq!(again.mods.len(), 3);
        assert_eq!(again.fetched_at, page.fetched_at);
        assert_eq!(seen.lock().unwrap().len(), 1);
        // "Refresh" asks Nexus again.
        c.search(&Search { refresh: true, ..q.clone() }, false).unwrap();
        assert_eq!(seen.lock().unwrap().len(), 2);

        let body: Value = serde_json::from_str(&seen.lock().unwrap()[0].body).unwrap();
        let v = &body["variables"];
        assert_eq!(v["sort"][0]["downloads"]["direction"], "DESC");
        assert_eq!(v["filter"]["createdAt"][0]["op"], "GTE");
        let since: i64 = v["filter"]["createdAt"][0]["value"].as_str().unwrap().parse().unwrap();
        assert_eq!(since % 86400, 0, "whole days keep the cache key stable");
        assert!((now_unix() - since) / 86400 >= TRENDING_DAYS);
        assert_eq!((v["offset"].as_u64(), v["count"].as_u64()), (Some(40), Some(40)));
        let req = &seen.lock().unwrap()[0];
        assert!(req.headers.iter().any(|(k, v)| k == "apikey" && v == "test-key"));
        assert!(req.headers.iter().any(|(k, _)| k == "application-name"));

        let rate = c.rate();
        assert_eq!(rate.hourly_remaining, Some(99));
        assert_eq!(rate.daily_remaining, Some(19876));
        assert_eq!(rate.hourly_reset, parse_time("2099-01-01T13:00:00Z"));

        // The debug log saw: sent, cached, sent again.
        let log = c.shared.requests_since(0);
        let served: Vec<_> = log.iter().map(|r| r.served).collect();
        assert_eq!(served, [Served::Network, Served::Cache, Served::Network]);
        assert_eq!(log[0].label.as_deref(), Some("BrowseMods"));
        assert_eq!((log[0].method.as_str(), log[0].status), ("POST", Some(200)));
        assert_eq!(log[0].endpoint, "/v2/graphql");
        assert_eq!(log[0].hourly_remaining, Some(99));
        assert_eq!(c.shared.requests_since(log[1].id).len(), 1);
    }

    #[test]
    fn latest_lists_sort_by_date_and_updated_needs_an_update() {
        let v = graphql_variables(&Search { sort: Sort::Updated, ..Default::default() }, true);
        assert_eq!(v["sort"][0]["updatedAt"]["direction"], "DESC");
        assert_eq!(v["filter"]["hasUpdated"][0]["value"], true);
        let v = graphql_variables(&Search { sort: Sort::Created, ..Default::default() }, true);
        assert_eq!(v["sort"][0]["createdAt"]["direction"], "DESC");
        assert!(v["filter"].get("hasUpdated").is_none() && v["filter"].get("createdAt").is_none());
        let v = graphql_variables(&Search { offset: 5_000_000, count: 1000, ..Default::default() }, true);
        assert_eq!((v["offset"].as_u64(), v["count"].as_u64()), (Some(MAX_OFFSET as u64), Some(MAX_PAGE as u64)));
    }

    #[test]
    fn shows_a_saved_copy_when_nexus_fails() {
        let (addr, _) = serve(vec![
            ("graphql", 200, vec![], fixture("search.json")),
            ("graphql", 500, vec![], "oops".into()),
        ]);
        let c = client(&addr);
        let q = Search { sort: Sort::Endorsements, ..Default::default() };
        let first = c.search(&q, true).unwrap();
        let second = c.search(&Search { refresh: true, ..q }, true).unwrap();
        assert!(second.saved_copy);
        assert_eq!((second.fetched_at, second.mods.len()), (first.fetched_at, first.mods.len()));
        let log = c.shared.requests_since(0);
        assert_eq!(log.last().unwrap().served, Served::Saved);
        assert_eq!(log[log.len() - 2].status, Some(500));
        // Update checks never get an old copy.
        let (addr, _) = serve(vec![]);
        assert!(client(&addr).files_with_updates(1).is_err());
    }

    #[test]
    fn lists_categories_and_filters_search_by_one() {
        let (addr, _) = serve(vec![("/games/cyberpunk2077.json", 200, vec![], fixture("game.json"))]);
        let cats = client(&addr).categories().unwrap();
        let names: Vec<_> = cats.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["Appearance", "Cyberpunk 2077", "Gameplay", "Utilities"]);
        assert_eq!(cats[0].parent, Some(1));
        assert_eq!(cats[1].parent, None);
        let v = graphql_variables(&Search { category: Some("Gameplay".into()), ..Default::default() }, true);
        assert_eq!(v["filter"]["categoryName"][0]["value"], "Gameplay");
        assert_eq!(v["filter"]["categoryName"][0]["op"], "EQUALS");
    }

    #[test]
    fn follows_update_chains_to_the_newest_file() {
        let (addr, _) = serve(vec![("/mods/107/files.json", 200, vec![], fixture("files_107.json"))]);
        let (files, updates) = client(&addr).files_with_updates(107).unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(newer_file(98000, &files, &updates).map(|f| f.file_id), Some(98765));
        assert!(newer_file(98765, &files, &updates).is_none(), "already the newest");
        assert!(newer_file(98766, &files, &updates).is_none(), "optional file without a chain");
        // No chain: a later main file with the same name counts as the update.
        let mut f2 = files.clone();
        let mut newer = files[0].clone();
        newer.file_id = 99000;
        newer.version = Some("1.38".into());
        newer.uploaded_timestamp = Some(1758000000);
        f2.push(newer);
        assert_eq!(newer_file(98765, &f2, &[]).map(|f| f.file_id), Some(99000));
        // Loops in the chain don't hang.
        let looped = vec![
            FileUpdate { old_file_id: 1, new_file_id: 2, uploaded_timestamp: None },
            FileUpdate { old_file_id: 2, new_file_id: 1, uploaded_timestamp: None },
        ];
        assert!(newer_file(1, &files, &looped).is_none());
    }

    #[test]
    fn search_uses_graphql_with_filters_and_paging() {
        let (addr, seen) = serve(vec![("graphql", 200, vec![], fixture("search.json"))]);
        let c = client(&addr);
        let q = Search { text: "  vehicle\u{7}  handling ".into(), sort: Sort::Relevance, offset: 20, count: 500, ..Default::default() };
        let page = c.search(&q, false).unwrap();
        assert_eq!(page.total, Some(57));
        assert_eq!(page.offset, 20);
        assert_eq!(page.count, 3);
        assert_eq!(page.hidden_adult, 1);
        assert_eq!(page.mods.len(), 2);
        let m = &page.mods[0];
        assert_eq!(m.mod_id, 2418);
        assert_eq!(m.author.as_deref(), Some("pMarK"));
        assert_eq!(m.updated, parse_time("2026-09-14T18:22:05Z"));
        assert!(m.picture_url.as_deref().unwrap().contains("/thumbnails/"));
        // Author falls back to the uploader's name.
        assert_eq!(page.mods[1].author.as_deref(), Some("uploader-only"));

        let req = &seen.lock().unwrap()[0];
        assert!(req.line.starts_with("POST /v2/graphql"));
        let body: Value = serde_json::from_str(&req.body).unwrap();
        let v = &body["variables"];
        assert_eq!(v["filter"]["gameDomainName"][0]["value"], "cyberpunk2077");
        assert_eq!(v["filter"]["nameStemmed"][0]["value"], "vehicle handling");
        assert_eq!(v["filter"]["nameStemmed"][0]["op"], "MATCHES");
        assert_eq!(v["sort"][0]["relevance"]["direction"], "DESC");
        assert_eq!(v["count"], MAX_PAGE);
        assert_eq!(v["offset"], 20);
        assert!(body["query"].as_str().unwrap().contains("mods(filter: $filter"));
    }

    #[test]
    fn sorted_list_without_text_has_no_name_filter() {
        let v = graphql_variables(&Search { sort: Sort::Downloads, ..Default::default() }, true);
        assert!(v["filter"].get("nameStemmed").is_none());
        assert_eq!(v["sort"][0]["downloads"]["direction"], "DESC");
        assert_eq!(v["count"], DEFAULT_PAGE);
        let v = graphql_variables(&Search::default(), true);
        assert_eq!(v["sort"][0]["endorsements"]["direction"], "DESC", "relevance needs text");
        let v = graphql_variables(&Search { text: "a*b".into(), ..Default::default() }, false);
        assert_eq!(v["filter"]["name"][0]["value"], "a b");
    }

    #[test]
    fn search_falls_back_to_wildcard_when_stemmed_filter_is_rejected() {
        let err = r#"{"errors":[{"message":"Field 'nameStemmed' is not defined by type 'ModsFilter'."}],"data":null}"#;
        let (addr, seen) = serve(vec![
            ("graphql", 200, vec![], err.to_string()),
            ("graphql", 200, vec![], fixture("search.json")),
        ]);
        let page = client(&addr).search(&Search { text: "vehicle".into(), ..Default::default() }, true).unwrap();
        assert_eq!(page.mods.len(), 3);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        let second: Value = serde_json::from_str(&seen[1].body).unwrap();
        assert_eq!(second["variables"]["filter"]["name"][0]["value"], "vehicle");
        assert_eq!(second["variables"]["filter"]["name"][0]["op"], "WILDCARD");
    }

    #[test]
    fn graphql_errors_without_text_are_reported() {
        let err = r#"{"errors":[{"message":"Internal error"}],"data":null}"#;
        let (addr, _) = serve(vec![("graphql", 200, vec![], err.to_string())]);
        let e = client(&addr).search(&Search::default(), false).unwrap_err().to_string();
        assert!(e.contains("Internal error"), "{e}");
    }

    #[test]
    fn http_429_blocks_further_requests_locally() {
        let mut headers = rl(0, 0);
        headers.push(("Retry-After", "120".into()));
        let (addr, seen) = serve(vec![("/games/cyberpunk2077.json", 429, headers, r#"{"msg":"Rate limit exceeded"}"#.into())]);
        let c = client(&addr);
        let e = c.categories().unwrap_err().to_string();
        assert!(e.contains("limit reached"), "{e}");
        // Even essential calls now wait, without touching the network.
        let e = c.validate().unwrap_err().to_string();
        assert!(e.contains("try again in 2 min"), "{e}");
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert!(c.rate().blocked_until.is_some());
    }

    #[test]
    fn browsing_pauses_near_the_limit_but_essentials_continue() {
        let user = r#"{"user_id":1,"key":"x","name":"V","is_premium":true,"is_supporter":true,"email":"v@example.com","profile_url":""}"#;
        let (addr, seen) = serve(vec![
            ("/users/validate.json", 200, rl(BROWSE_RESERVE - 5, 0), user.into()),
            ("/games/cyberpunk2077.json", 200, rl(99, 100), fixture("game.json")),
        ]);
        let c = client(&addr);
        assert_eq!(c.validate().unwrap().name, "V");
        let e = c.categories().unwrap_err().to_string();
        assert!(e.contains("browsing is paused"), "{e}");
        assert!(c.validate().is_ok(), "essential calls still go out");
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[test]
    fn mod_details_include_plain_text_description_and_files() {
        let (addr, seen) = serve(vec![
            ("/mods/107/files.json", 200, rl(98, 19870), fixture("files_107.json")),
            ("/mods/107.json", 200, rl(97, 19869), fixture("mod_107.json")),
            ("graphql", 200, vec![], fixture("mod_extras_4198.json")),
        ]);
        let c = client(&addr);
        let d = c.mod_details(107, false).unwrap();
        assert_eq!(d.info.name.as_deref(), Some("Cyber Engine Tweaks"));
        assert_eq!(d.info.endorsement_count, Some(91234));
        assert_eq!(d.files.len(), 3);
        assert!(d.files.iter().any(|f| f.is_primary && f.file_name.ends_with(".zip")));
        assert!(d.description_text.contains("Requirements\n"), "{}", d.description_text);
        assert!(d.description_text.contains("• RED4ext"), "{}", d.description_text);
        assert!(!d.description_text.contains("[b]"));
        assert!(!d.description_text.contains("<script"));
        assert!(!d.description_text.contains("i.imgur.com"), "image URLs dropped");
        assert_eq!(d.page_url, "https://www.nexusmods.com/cyberpunk2077/mods/107");
        // The formatted description keeps headings and list items.
        let doc = serde_json::to_string(&d.description).unwrap();
        assert!(doc.contains(r#""tag":"li""#) && doc.contains("RED4ext"), "{doc}");
        assert!(!doc.contains("<script") && !doc.contains("alert"), "{doc}");

        let r = d.requirements.as_ref().unwrap();
        assert_eq!(r.nexus, vec![Requirement { mod_id: Some(2380), name: "RED4ext".into(), notes: None, url: None }]);
        assert_eq!(r.external.len(), 1);
        assert_eq!(r.external[0].url.as_deref(), Some("https://aka.ms/vs/17/release/vc_redist.x64.exe"));
        assert_eq!(r.dlc, vec!["Phantom Liberty".to_string()]);
        assert_eq!(r.required_by, Some(7497));
        assert_eq!(d.tags, vec!["Modder's Resource".to_string(), "Utilities for Players".to_string()]);
        let gql: Value = serde_json::from_str(&seen.lock().unwrap()[2].body).unwrap();
        assert_eq!(gql["variables"]["filter"]["gameId"][0]["value"], "3333");
        assert_eq!(gql["variables"]["filter"]["modId"][0]["value"], "107");

        c.mod_details(107, false).unwrap();
        assert_eq!(seen.lock().unwrap().len(), 3, "second view cached");
        c.mod_details(107, true).unwrap();
        assert_eq!(seen.lock().unwrap().len(), 6, "refresh asks again");
    }

    #[test]
    fn mod_page_shows_without_requirements() {
        let (addr, _) = serve(vec![
            ("/mods/107/files.json", 200, vec![], fixture("files_107.json")),
            ("/mods/107.json", 200, vec![], fixture("mod_107.json")),
            ("graphql", 200, vec![], r#"{"errors":[{"message":"boom"}],"data":null}"#.into()),
        ]);
        let d = client(&addr).mod_details(107, false).unwrap();
        assert!(d.requirements.is_none() && d.tags.is_empty());
        assert_eq!(d.files.len(), 3);
    }

    #[test]
    fn parses_nexus_timestamps() {
        assert_eq!(parse_time("1970-01-01 00:00:00 +0000"), Some(0));
        assert_eq!(parse_time("2024-02-29T12:00:00Z"), Some(1709208000));
        assert_eq!(parse_time("2024-02-29T12:00:00.123+01:00"), Some(1709204400));
        assert_eq!(parse_time("2024-02-29 13:00:00 +0100"), Some(1709208000));
        assert_eq!(parse_time("garbage"), None);
        assert_eq!(parse_time("2024-13-01T00:00:00Z"), None);
    }

    #[test]
    fn bbcode_becomes_plain_text() {
        let t = bbcode_to_text(
            "[size=5][b]Hello[/b][/size]<br />[url=https://x.example]link text[/url] a [notatag] &amp; b\
             [list][*]one[*]two[/list][img]https://i.imgur.com/a.png[/img]<script>alert(1)</script> 3 < 4 [",
        );
        assert_eq!(t, "Hello\nlink text a [notatag] & b\n• one\n• two\n3 < 4 [");
        assert_eq!(decode_entities("&#65;&#x42;&bogus; &"), "AB&bogus; &");
        let long = "x".repeat(MAX_DESCRIPTION_CHARS + 10);
        assert_eq!(bbcode_to_text(&long).chars().count(), MAX_DESCRIPTION_CHARS + 1);
        // Multi-byte characters near the 300-byte tag and 12-byte entity limits.
        let wide = format!("[{}— x", "a".repeat(298));
        assert_eq!(bbcode_to_text(&wide), wide);
        assert_eq!(decode_entities("&ééééééé; ok"), "&ééééééé; ok");
    }

    #[test]
    fn parses_mod_and_collection_links() {
        let m = |id| Some(NexusRef::Mod { mod_id: id });
        assert_eq!(parse_nexus_ref("107"), m(107));
        assert_eq!(parse_nexus_ref("https://www.nexusmods.com/cyberpunk2077/mods/107?tab=files"), m(107));
        assert_eq!(parse_nexus_ref("https://www.nexusmods.com/games/cyberpunk2077/mods/107"), m(107));
        assert_eq!(parse_nexus_ref(" https://nexusmods.com/games/Cyberpunk2077/mods/4198/ "), m(4198));
        let c = |slug: &str, revision| Some(NexusRef::Collection { slug: slug.into(), revision });
        assert_eq!(parse_nexus_ref("https://www.nexusmods.com/games/cyberpunk2077/collections/rcwfx9"), c("rcwfx9", None));
        assert_eq!(parse_nexus_ref("https://next.nexusmods.com/cyberpunk2077/collections/RCWFX9/revisions/12"), c("rcwfx9", Some(12)));
        assert_eq!(parse_nexus_ref("nxm://cyberpunk2077/collections/rcwfx9/revisions/7"), c("rcwfx9", Some(7)));
        for bad in [
            "https://www.nexusmods.com/games/skyrimspecialedition/mods/1",
            "https://evilnexusmods.com/games/cyberpunk2077/mods/1",
            "https://www.nexusmods.com/games/cyberpunk2077/collections/../x",
            "nxm://skyrim/collections/abc/revisions/1",
            "0",
            "hello",
        ] {
            assert_eq!(parse_nexus_ref(bad), None, "{bad}");
        }
    }

    #[test]
    fn reads_a_collection_revision() {
        let body = r#"{"data":{"collectionRevision":{"revisionNumber":12,"gameVersions":[{"reference":"2.31"}],
            "collectionChangelog":{"description":"Updated CET"},
            "collection":{"name":"Night City Overhaul","summary":"Many mods","user":{"name":"someone"},
              "tileImage":{"url":"https://media.nexusmods.com/a/8/x.webp"}},
            "modFiles":[
              {"fileId":"1001","optional":false,"file":{"fileId":1001,"name":"Main file","version":"1.37.1","mod":{"modId":107,"name":"Cyber Engine Tweaks"}}},
              {"fileId":2002,"optional":true,"file":{"fileId":2002,"name":"Extra","version":null,"mod":{"modId":"4198","name":"Some Mod"}}},
              {"fileId":3003,"optional":false,"file":null}],
            "externalResources":[{"name":"Off-site texture pack"}]}}}"#;
        let (addr, seen) = serve(vec![("/v2/graphql", 200, vec![], body.into())]);
        let c = client(&addr).collection("rcwfx9", None).unwrap();
        assert_eq!(c.name, "Night City Overhaul");
        assert_eq!(c.revision, Some(12));
        assert_eq!(c.game_version.as_deref(), Some("2.31"));
        assert_eq!(c.mods.len(), 2, "entries without a file are skipped");
        assert_eq!((c.mods[0].mod_id, c.mods[0].file_id, c.mods[0].optional), (107, 1001, false));
        assert_eq!((c.mods[1].mod_id, c.mods[1].optional), (4198, true));
        assert_eq!(c.external, vec!["Off-site texture pack".to_string()]);
        assert_eq!(c.changelog.as_deref(), Some("Updated CET"));
        assert_eq!(c.image.as_deref(), Some("https://media.nexusmods.com/a/8/x.webp"));
        let req = seen.lock().unwrap()[0].body.clone();
        let req: Value = serde_json::from_str(&req).unwrap();
        assert_eq!(req["variables"]["slug"], "rcwfx9");
        assert!(req["variables"]["revision"].is_null());

        let (addr, _) = serve(vec![("/v2/graphql", 200, vec![], r#"{"data":{"collectionRevision":null}}"#.into())]);
        let e = client(&addr).collection("gone12", Some(3)).unwrap_err();
        assert!(e.to_string().contains("not found"), "{e}");
        assert!(client(&addr).collection("../x", None).is_err());
    }

    #[test]
    fn page_urls() {
        assert_eq!(mod_page_url(107, Some(5)), "https://www.nexusmods.com/cyberpunk2077/mods/107?tab=files&file_id=5");
    }

    #[test]
    fn searches_collections() {
        // Shape as returned live by collectionsV2 (2026-10-07).
        let body = r#"{"data":{"collectionsV2":{"totalCount":70,"nodes":[
            {"slug":"l8bhvl","name":"Blood & Chrome","summary":"Big","endorsements":60,"totalDownloads":6530,
             "updatedAt":"2026-10-07T05:40:05Z","user":{"name":"WesternInfluence"},"category":{"name":"Total Overhaul"},
             "tileImage":{"url":"https://media.nexusmods.com/2/e/2e0d.webp"},
             "latestPublishedRevision":{"revisionNumber":7,"modCount":837,"totalSize":"15482852695","adultContent":true,
               "gameVersions":[{"reference":"2.3.1.0"}]}},
            {"slug":"../bad","name":"x"},
            {"slug":"yeyneq","name":"Driving","tileImage":{"url":"https://evil.example/x.png"},"latestPublishedRevision":null}]}}}"#;
        let (addr, seen) = serve(vec![("/v2/graphql", 200, vec![], body.into())]);
        let q = CollectionSearch { text: " car\u{7} ".into(), sort: CollectionSort::Downloads, offset: 20, count: 999 };
        let p = client(&addr).search_collections(&q, false).unwrap();
        assert_eq!(p.total, Some(70));
        assert_eq!(p.collections.len(), 2, "bad slugs are dropped");
        let c = &p.collections[0];
        assert_eq!((c.slug.as_str(), c.revision, c.mod_count, c.total_size), ("l8bhvl", Some(7), Some(837), Some(15482852695)));
        assert_eq!(c.game_version.as_deref(), Some("2.3.1.0"));
        assert!(c.adult);
        assert_eq!(p.collections[1].image, None, "only Nexus image hosts");
        let body: Value = serde_json::from_str(&seen.lock().unwrap()[0].body).unwrap();
        let v = &body["variables"];
        assert_eq!(v["filter"]["generalSearch"][0]["value"], "car");
        assert_eq!(v["filter"]["adultContent"][0]["value"], false);
        assert_eq!(v["sort"][0]["downloads"]["direction"], "DESC");
        assert_eq!((v["offset"].as_u64(), v["count"].as_u64()), (Some(20), Some(MAX_PAGE as u64)));
        assert!(collection_variables(&CollectionSearch::default(), true)["filter"].get("adultContent").is_none());
    }

    #[test]
    fn reads_requirements_in_batches() {
        // Shape as returned live by legacyModsByDomain (2026-10-07).
        let body = r#"{"data":{"legacyModsByDomain":{"nodes":[
            {"modId":4198,"modRequirements":{"nexusRequirements":{"nodes":[
                {"modId":"2380","modName":"RED4ext","url":"","notes":"","externalRequirement":false,"gameId":"3333"},
                {"modId":"2380","modName":"RED4ext again","url":"","notes":"","externalRequirement":false,"gameId":"3333"},
                {"modId":"4198","modName":"itself","url":"","notes":"","externalRequirement":false,"gameId":"3333"},
                {"modId":"0","modName":"Wolvenkit","url":"https://wiki.redmodding.org/","notes":"for modders","externalRequirement":true,"gameId":"3333"},
                {"modId":"55","modName":"Skyrim thing","url":"","notes":"","externalRequirement":false,"gameId":"110"},
                {"modId":"0","modName":"Bad link","url":"javascript:alert(1)","notes":"","externalRequirement":true,"gameId":"3333"}]},
              "dlcRequirements":[{"gameExpansion":{"name":"Phantom Liberty"}}]}},
            {"modId":107,"modRequirements":{"nexusRequirements":{"nodes":[]},"dlcRequirements":[]}}]}}}"#;
        let (addr, seen) = serve(vec![("/v2/graphql", 200, vec![], body.into())]);
        let ids: Vec<i64> = (1..=60).chain([4198, 107, 107, -1]).collect();
        let r = client(&addr).requirements(&ids).unwrap();
        let axl = &r.iter().find(|(id, _)| *id == 4198).unwrap().1;
        let names: Vec<&str> = axl.iter().map(|q| q.name.as_str()).collect();
        assert_eq!(names, ["RED4ext", "Wolvenkit", "Bad link", "Phantom Liberty"]);
        assert_eq!(axl[0].mod_id, Some(2380));
        assert_eq!((axl[1].mod_id, axl[1].url.as_deref(), axl[1].notes.as_deref()), (None, Some("https://wiki.redmodding.org/"), Some("for modders")));
        assert_eq!(axl[2].url, None, "only http(s) links");
        assert!(axl[3].dlc);
        assert!(r.iter().any(|(id, l)| *id == 107 && l.is_empty()));
        // 62 distinct ids: two requests of at most 50.
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        let first: Value = serde_json::from_str(&seen[0].body).unwrap();
        assert_eq!(first["variables"]["ids"].as_array().unwrap().len(), 50);
        assert_eq!(first["variables"]["count"], 50);
        assert_eq!(first["variables"]["ids"][0]["gameDomain"], "cyberpunk2077");
    }
}
