//! GitHub as a second mod source: find repositories, list their releases and
//! download release assets. Many frameworks (CET, RED4ext, redscript,
//! ArchiveXL, TweakXL, Codeware) publish there first.
//!
//! Works without an account (GitHub allows 60 API requests an hour per IP),
//! so answers are cached for a while. Downloads only come from GitHub's own
//! hosts over HTTPS, must have the size GitHub reports, and are checked
//! against the SHA-256 digest GitHub publishes for release assets.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Details, InstalledRef, Listing, ListingPage, ModSource, Progress, SourceFile, SourceInfo, SourceQuery, UpdateOffer};
use crate::activity::{self, Kind};
use crate::nexus::{parse_time, safe_file_name};
use crate::{APP_NAME, APP_VERSION, Error, Result, hash};

pub const API_BASE: &str = "https://api.github.com";
const CACHE_TTL: Duration = Duration::from_secs(10 * 60);
const PAGE_SIZE: u32 = 20;
const MAX_BODY_CHARS: usize = 5000;
/// Hosts GitHub serves release downloads from (github.com redirects to one
/// of the others).
const DOWNLOAD_HOSTS: &[&str] =
    &["github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com", "github-releases.githubusercontent.com"];

/// A well-known framework hosted on GitHub, shown before the user searches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Featured {
    /// `owner/repo`.
    pub repo: &'static str,
    pub name: &'static str,
    /// What it is, in one line.
    pub what: &'static str,
    /// Id in [`crate::game::detect_frameworks`].
    pub key: &'static str,
    /// Keys of the frameworks it needs to load.
    pub requires: &'static [&'static str],
}

/// Well-known mod frameworks hosted on GitHub, in the order they install
/// (each after what it needs).
pub const FEATURED: &[Featured] = &[
    Featured {
        repo: "maximegmd/CyberEngineTweaks",
        name: "Cyber Engine Tweaks",
        what: "Scripting framework and in-game console; many mods need it.",
        key: "cet",
        requires: &[],
    },
    Featured { repo: "wopss/RED4ext", name: "RED4ext", what: "Script extender that loads native plugins.", key: "red4ext", requires: &[] },
    Featured { repo: "jac3km4/redscript", name: "redscript", what: "Compiler for .reds script mods.", key: "redscript", requires: &[] },
    Featured {
        repo: "psiberx/cp2077-archive-xl",
        name: "ArchiveXL",
        what: "Loads custom resources and appearances without replacing game files.",
        key: "archivexl",
        requires: &["red4ext"],
    },
    Featured {
        repo: "psiberx/cp2077-tweak-xl",
        name: "TweakXL",
        what: "Loads tweak (.yaml/.tweak) files that change game records.",
        key: "tweakxl",
        requires: &["red4ext"],
    },
    Featured {
        repo: "psiberx/cp2077-codeware",
        name: "Codeware",
        what: "Library that extends what redscript and CET mods can do.",
        key: "codeware",
        requires: &["red4ext", "redscript"],
    },
];

fn featured_by_key(key: &str) -> Option<&'static Featured> {
    FEATURED.iter().find(|f| f.key.eq_ignore_ascii_case(key))
}

fn featured_by_repo(repo: &str) -> Option<&'static Featured> {
    FEATURED.iter().find(|f| f.repo.eq_ignore_ascii_case(repo))
}

/// The frameworks to install for `wanted` (keys or `owner/repo`), with
/// everything they need that isn't in `present` (installed framework keys),
/// each after its requirements. Wanted frameworks are kept even if present,
/// so a reinstall stays possible.
pub fn install_plan(wanted: &[String], present: &[String]) -> Result<Vec<&'static Featured>> {
    let is_present = |k: &str| present.iter().any(|p| p.eq_ignore_ascii_case(k));
    let mut need: Vec<&'static Featured> = Vec::new();
    let mut stack = Vec::new();
    for w in wanted {
        let f = featured_by_key(w).or_else(|| featured_by_repo(w)).ok_or_else(|| Error::Other(format!("{w} is not a known framework")))?;
        stack.push((f, true));
    }
    while let Some((f, explicit)) = stack.pop() {
        if need.contains(&f) || (!explicit && is_present(f.key)) {
            continue;
        }
        need.push(f);
        stack.extend(f.requires.iter().filter_map(|k| featured_by_key(k)).map(|d| (d, false)));
    }
    // FEATURED lists requirements first, so its order is an install order.
    Ok(FEATURED.iter().filter(|f| need.contains(f)).collect())
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RepoCard {
    /// `owner/name`.
    pub repo: String,
    pub owner: String,
    pub name: String,
    pub description: Option<String>,
    pub stars: Option<i64>,
    pub updated: Option<i64>,
    pub topics: Vec<String>,
    pub archived: bool,
    pub url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoPage {
    pub repos: Vec<RepoCard>,
    pub total: Option<i64>,
    pub page: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Asset {
    pub id: i64,
    pub name: String,
    pub size: u64,
    /// `sha256:<hex>` when GitHub has computed it.
    pub digest: Option<String>,
    pub download_url: String,
    pub download_count: Option<i64>,
    /// A .zip/.7z/.rar the installer can take.
    pub installable: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Release {
    pub id: i64,
    pub tag: String,
    pub name: Option<String>,
    pub published: Option<i64>,
    pub prerelease: bool,
    /// Release notes as plain text (never render as HTML).
    pub notes: String,
    pub url: String,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Downloaded {
    pub path: PathBuf,
    pub file_name: String,
    pub sha256: String,
    pub md5: String,
    pub size: u64,
    /// The SHA-256 matched the digest GitHub published for the asset.
    pub verified: bool,
}

/// Accepts `owner/repo` or any github.com URL inside a repository.
pub fn parse_repo_ref(s: &str) -> Option<(String, String)> {
    let s = s.trim().trim_end_matches('/');
    let path = if let Ok(url) = url::Url::parse(s) {
        let host = url.host_str()?.to_ascii_lowercase();
        if host != "github.com" && host != "www.github.com" {
            return None;
        }
        url.path().trim_matches('/').to_string()
    } else {
        s.trim_start_matches("github.com/").to_string()
    };
    let mut parts = path.split('/');
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.trim_end_matches(".git").to_string();
    let ok_owner = !owner.is_empty() && owner.len() <= 39 && owner.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    let ok_repo = !repo.is_empty()
        && repo.len() <= 100
        && repo != "."
        && repo != ".."
        && repo.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    (ok_owner && ok_repo).then_some((owner, repo))
}

/// An archive the installer can take. Releases often add debug symbols
/// (`red4ext-symbols-1.30.0.zip`, `*-pdb.zip`) or builds for other systems
/// (`redscript-v0.5.31-macos.zip`) next to the game files; those are
/// downloadable but never installed, so the one real archive is picked.
fn is_installable(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    let archive = [".zip", ".7z", ".rar"].iter().any(|e| n.ends_with(e));
    let stem = n.rsplit_once('.').map_or(n.as_str(), |(s, _)| s);
    let words: Vec<&str> = stem.split(|c: char| !c.is_ascii_alphanumeric()).collect();
    let other = ["pdb", "pdbs", "symbols", "debug", "macos", "darwin", "osx", "mac", "linux", "source", "src", "sdk"];
    archive && !words.iter().any(|w| other.contains(w))
}

/// Release notes are Markdown; show them as text, bounded.
fn notes_text(s: Option<&str>) -> String {
    s.unwrap_or_default()
        .replace("\r\n", "\n")
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .take(MAX_BODY_CHARS)
        .collect()
}

fn clean(s: Option<&str>, max: usize) -> Option<String> {
    let s: String = s?.chars().filter(|c| !c.is_control()).take(max).collect();
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn repo_card(v: &Value) -> Option<RepoCard> {
    let full = v.get("full_name")?.as_str()?;
    let (owner, name) = parse_repo_ref(full)?;
    Some(RepoCard {
        repo: format!("{owner}/{name}"),
        url: format!("https://github.com/{owner}/{name}"),
        owner,
        name,
        description: clean(v.get("description").and_then(Value::as_str), 500),
        stars: v.get("stargazers_count").and_then(Value::as_i64),
        updated: v.get("pushed_at").or(v.get("updated_at")).and_then(Value::as_str).and_then(parse_time),
        topics: v
            .get("topics")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|t| clean(t.as_str(), 50)).take(10).collect())
            .unwrap_or_default(),
        archived: v.get("archived").and_then(Value::as_bool).unwrap_or(false),
    })
}

fn release(v: &Value) -> Option<Release> {
    if v.get("draft").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    let assets = v
        .get("assets")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    let name = clean(x.get("name")?.as_str(), 255)?;
                    Some(Asset {
                        id: x.get("id")?.as_i64()?,
                        installable: is_installable(&name),
                        name,
                        size: x.get("size")?.as_u64()?,
                        digest: x.get("digest").and_then(Value::as_str).filter(|d| d.starts_with("sha256:")).map(str::to_string),
                        download_url: x.get("browser_download_url")?.as_str()?.to_string(),
                        download_count: x.get("download_count").and_then(Value::as_i64),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Release {
        id: v.get("id")?.as_i64()?,
        tag: clean(v.get("tag_name")?.as_str(), 100)?,
        name: clean(v.get("name").and_then(Value::as_str), 200),
        published: v.get("published_at").and_then(Value::as_str).and_then(parse_time),
        prerelease: v.get("prerelease").and_then(Value::as_bool).unwrap_or(false),
        notes: notes_text(v.get("body").and_then(Value::as_str)),
        url: v.get("html_url").and_then(Value::as_str).unwrap_or_default().to_string(),
        assets,
    })
}

/// Asset names with the version taken out, to find the same download in a
/// newer release (`ArchiveXL-1.20.0.zip` → `archivexl-.zip`).
pub fn asset_shape(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    let mut out = String::new();
    let mut chars = lower.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            // Swallow the whole version: digits and the dots between them.
            while chars.peek().is_some_and(|n| n.is_ascii_digit() || *n == '.') {
                let n = chars.next().unwrap();
                if n == '.' && !chars.peek().is_some_and(|d| d.is_ascii_digit()) {
                    out.push('.');
                    break;
                }
            }
            if out.ends_with('v') {
                out.pop();
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// The asset in `release` that replaces `old_asset`.
pub fn matching_asset<'a>(release: &'a Release, old_asset: Option<&str>) -> Option<&'a Asset> {
    let installable: Vec<&Asset> = release.assets.iter().filter(|a| a.installable).collect();
    if let Some(old) = old_asset {
        let shape = asset_shape(old);
        if let Some(a) = installable.iter().find(|a| asset_shape(&a.name) == shape) {
            return Some(a);
        }
    }
    (installable.len() == 1).then(|| installable[0])
}

pub struct Client {
    http: reqwest::blocking::Client,
    base: String,
    cache: Arc<Mutex<HashMap<String, (Instant, Value)>>>,
}

impl Client {
    pub fn new() -> Result<Self> {
        Self::with_base(API_BASE)
    }

    pub fn with_base(base: &str) -> Result<Self> {
        let base = base.trim_end_matches('/').to_string();
        let allowed_base = base.clone();
        let policy = reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 5 {
                attempt.error("too many redirects")
            } else if allowed_host(&allowed_base, attempt.url()) {
                attempt.follow()
            } else {
                let to = attempt.url().to_string();
                attempt.error(format!("redirect to untrusted host {to}"))
            }
        });
        let http = reqwest::blocking::Client::builder()
            .user_agent(format!("{APP_NAME}/{APP_VERSION} (Linux)"))
            .https_only(base.starts_with("https://"))
            .redirect(policy)
            .connect_timeout(Duration::from_secs(20))
            .timeout(None)
            .build()?;
        Ok(Self { http, base, cache: Arc::default() })
    }

    /// Share one cache between clients (the app keeps one for its lifetime).
    pub fn with_cache(mut self, cache: Arc<Mutex<HashMap<String, (Instant, Value)>>>) -> Self {
        self.cache = cache;
        self
    }

    fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        let key = format!("{path}?{query:?}");
        if let Some((at, v)) = self.cache.lock().unwrap().get(&key)
            && at.elapsed() < CACHE_TTL
        {
            activity::record(Kind::Api, format!("GitHub GET {path} (cached)"));
            return Ok(v.clone());
        }
        let started = Instant::now();
        let resp = self
            .http
            .get(format!("{}{}", self.base, path))
            .query(query)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .timeout(Duration::from_secs(30))
            .send()?;
        let status = resp.status();
        let remaining = header_i64(&resp, "x-ratelimit-remaining");
        let reset = header_i64(&resp, "x-ratelimit-reset");
        let text = resp.text()?;
        activity::record(
            Kind::Api,
            format!(
                "GitHub GET {path} → {} in {} ms ({}{})",
                status.as_u16(),
                started.elapsed().as_millis(),
                activity::size(text.len() as u64),
                remaining.map(|r| format!(", {r} requests left this hour")).unwrap_or_default(),
            ),
        );
        if (status.as_u16() == 403 || status.as_u16() == 429) && remaining == Some(0) {
            let wait = reset.map(|r| (r - crate::nexus::now_unix()).max(0) / 60 + 1).unwrap_or(60);
            return Err(Error::Other(format!(
                "GitHub's limit of 60 requests an hour without an account is used up; try again in {wait} min"
            )));
        }
        if !status.is_success() {
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("message").and_then(Value::as_str).map(String::from))
                .unwrap_or_else(|| text.chars().take(200).collect());
            return Err(Error::Other(format!("GitHub: {status}: {msg}")));
        }
        let v: Value = serde_json::from_str(&text)?;
        self.cache.lock().unwrap().insert(key, (Instant::now(), v.clone()));
        Ok(v)
    }

    /// Search repositories about Cyberpunk 2077, most starred first.
    pub fn search(&self, text: &str, page: u32) -> Result<RepoPage> {
        let text = crate::nexus_browse::clean_query(text);
        let q = format!("{text} cyberpunk 2077 in:name,description,topics fork:false").trim().to_string();
        let page = page.clamp(1, 50);
        let v = self.get(
            "/search/repositories",
            &[("q", q), ("sort", "stars".into()), ("order", "desc".into()), ("per_page", PAGE_SIZE.to_string()), ("page", page.to_string())],
        )?;
        let repos = v.get("items").and_then(Value::as_array).map(|a| a.iter().filter_map(repo_card).collect()).unwrap_or_default();
        Ok(RepoPage { repos, total: v.get("total_count").and_then(Value::as_i64), page })
    }

    pub fn repo(&self, owner: &str, repo: &str) -> Result<RepoCard> {
        let (owner, repo) = checked(owner, repo)?;
        repo_card(&self.get(&format!("/repos/{owner}/{repo}"), &[])?).ok_or_else(|| Error::Other("GitHub: unexpected repository data".into()))
    }

    /// Recent releases, newest first (drafts left out).
    pub fn releases(&self, owner: &str, repo: &str) -> Result<Vec<Release>> {
        let (owner, repo) = checked(owner, repo)?;
        let v = self.get(&format!("/repos/{owner}/{repo}/releases"), &[("per_page", "15".into())])?;
        Ok(v.as_array().map(|a| a.iter().filter_map(release).collect()).unwrap_or_default())
    }

    /// Download a release asset of `owner/repo` into `dest_dir`.
    pub fn download(
        &self,
        owner: &str,
        repo: &str,
        asset: &Asset,
        dest_dir: &Path,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<Downloaded> {
        let (owner, repo) = checked(owner, repo)?;
        let url = url::Url::parse(&asset.download_url).map_err(|e| Error::Other(format!("bad download URL: {e}")))?;
        let on_github = url.host_str() == Some("github.com");
        if !allowed_host(&self.base, &url)
            || (on_github && !url.path().starts_with(&format!("/{owner}/{repo}/releases/download/")))
        {
            return Err(Error::Other(format!("refusing to download {} (not a release of {owner}/{repo})", asset.download_url)));
        }
        let file_name = safe_file_name(&asset.name);
        std::fs::create_dir_all(dest_dir)?;
        let final_path = dest_dir.join(format!("gh-{owner}-{repo}-{}-{file_name}", asset.id));
        let part_path = final_path.with_extension("part");

        activity::record_path(
            Kind::Download,
            format!("Downloading {} from {}", asset.name, activity::safe_url(&asset.download_url)),
            &final_path,
        );
        let mut resp = self.http.get(url).header("Accept", "application/octet-stream").send()?;
        if !resp.status().is_success() {
            activity::record(Kind::Error, format!("Download failed: {}", resp.status()));
            return Err(Error::Other(format!("GitHub download failed: {}", resp.status())));
        }
        let expected = asset.size;
        let mut out = std::fs::File::create(&part_path)?;
        let mut buf = vec![0u8; 1 << 16];
        let mut done = 0u64;
        loop {
            let n = resp.read(&mut buf)?;
            if n == 0 {
                break;
            }
            done += n as u64;
            if done > expected {
                drop(out);
                let _ = std::fs::remove_file(&part_path);
                activity::record_path(Kind::Error, "Download larger than GitHub says; deleted", &part_path);
                return Err(Error::Integrity("download is larger than GitHub says it should be".into()));
            }
            out.write_all(&buf[..n])?;
            progress(done, expected);
        }
        out.sync_all()?;
        drop(out);
        if done != expected {
            let _ = std::fs::remove_file(&part_path);
            activity::record_path(Kind::Error, format!("Size mismatch ({done} of {expected} bytes); deleted"), &part_path);
            return Err(Error::Integrity(format!("size mismatch: got {done} bytes, expected {expected}")));
        }
        let (sha256, md5) = hash::file_digests(&part_path)?;
        let verified = match asset.digest.as_deref().and_then(|d| d.strip_prefix("sha256:")) {
            Some(want) if want.eq_ignore_ascii_case(&sha256) => true,
            Some(_) => {
                let _ = std::fs::remove_file(&part_path);
                activity::record_path(Kind::Error, format!("SHA-256 {sha256} doesn't match GitHub's; deleted"), &part_path);
                return Err(Error::Integrity("the file doesn't match the SHA-256 GitHub published for it; discarded".into()));
            }
            None => false,
        };
        activity::record(
            Kind::Verify,
            if verified {
                format!("SHA-256 {sha256} matches GitHub's ({})", activity::size(done))
            } else {
                format!("GitHub publishes no checksum for this file; SHA-256 {sha256} ({})", activity::size(done))
            },
        );
        std::fs::rename(&part_path, &final_path)?;
        activity::record_path(Kind::Move, "Saved download as", &final_path);
        Ok(Downloaded { path: final_path, file_name, sha256, md5, size: done, verified })
    }
}

/// GitHub releases as a [`ModSource`].
pub struct GitHubSource {
    client: Client,
}

impl GitHubSource {
    pub fn new() -> Result<Self> {
        // Debug builds can point at a local mock for UI testing.
        let base = if cfg!(debug_assertions) { std::env::var("CPMX_GITHUB_API").ok() } else { None };
        Ok(Self { client: Client::with_base(base.as_deref().unwrap_or(API_BASE))? })
    }

    pub fn with_client(client: Client) -> Self {
        Self { client }
    }

    fn listing(card: &RepoCard, category: Option<&str>) -> Listing {
        Listing {
            source: "github".into(),
            id: card.repo.clone(),
            name: card.name.clone(),
            author: Some(card.owner.clone()),
            summary: card.description.clone(),
            version: None,
            popularity: card.stars,
            downloads: None,
            updated: card.updated,
            category: category.map(String::from),
            tags: card.topics.clone(),
            requires: vec![],
            url: card.url.clone(),
        }
    }

    /// Files grouped by release, the latest full release first (pre-releases
    /// can be newer, but most mods are built against the stable one).
    fn files(releases: &[Release]) -> Vec<SourceFile> {
        let latest = releases.iter().find(|r| !r.prerelease).map(|r| r.id);
        let mut ordered: Vec<&Release> = releases.iter().collect();
        ordered.sort_by_key(|r| Some(r.id) != latest);
        ordered
            .into_iter()
            .flat_map(|r| {
                let group = match (Some(r.id) == latest, r.prerelease) {
                    (true, _) => format!("{} (latest)", r.tag),
                    (_, true) => format!("{} (pre-release)", r.tag),
                    _ => r.tag.clone(),
                };
                r.assets.iter().map(move |a| SourceFile {
                    id: a.id.to_string(),
                    file_name: a.name.clone(),
                    version: Some(r.tag.clone()),
                    group: group.clone(),
                    size: a.size,
                    uploaded: r.published,
                    installable: a.installable,
                    verifiable: a.digest.is_some(),
                    prerelease: r.prerelease,
                    downloads: a.download_count,
                })
            })
            .collect()
    }
}

/// Display names of what `f` needs.
fn requires_names(f: &Featured) -> Vec<String> {
    f.requires.iter().filter_map(|k| featured_by_key(k)).map(|d| d.name.to_string()).collect()
}

fn split_ref(id: &str) -> Result<(String, String)> {
    parse_repo_ref(id).ok_or_else(|| Error::Other(format!("not a GitHub repository: {id}")))
}

impl ModSource for GitHubSource {
    fn info(&self) -> SourceInfo {
        SourceInfo {
            id: "github",
            label: "GitHub",
            description: "Frameworks and mods published as GitHub releases. Downloads are checked against GitHub's SHA-256 where it publishes one.",
            search_hint: "Search GitHub, or paste owner/repo or a github.com link",
            popularity_label: "stars",
        }
    }

    fn featured(&self) -> Result<Vec<Listing>> {
        Ok(FEATURED
            .iter()
            .filter_map(|f| {
                let (owner, repo_name) = parse_repo_ref(f.repo)?;
                Some(Listing {
                    source: "github".into(),
                    id: format!("{owner}/{repo_name}"),
                    name: f.name.into(),
                    author: Some(owner.clone()),
                    summary: Some(f.what.into()),
                    version: None,
                    popularity: None,
                    downloads: None,
                    updated: None,
                    category: Some("Framework".into()),
                    tags: vec![],
                    requires: requires_names(f),
                    url: format!("https://github.com/{owner}/{repo_name}"),
                })
            })
            .collect())
    }

    fn search(&self, query: &SourceQuery) -> Result<ListingPage> {
        let page = query.page.max(1);
        let p = self.client.search(&query.text, page)?;
        let has_more = p.total.is_some_and(|t| (page as i64) * (PAGE_SIZE as i64) < t.min(1000));
        Ok(ListingPage {
            listings: p.repos.iter().filter(|r| !r.archived).map(|r| Self::listing(r, None)).collect(),
            total: p.total,
            page,
            has_more,
        })
    }

    fn parse_ref(&self, input: &str) -> Option<String> {
        parse_repo_ref(input).map(|(o, r)| format!("{o}/{r}"))
    }

    fn details(&self, id: &str) -> Result<Details> {
        let (owner, repo) = split_ref(id)?;
        let card = self.client.repo(&owner, &repo)?;
        let releases = self.client.releases(&owner, &repo)?;
        let featured = featured_by_repo(&card.repo);
        let mut listing = Self::listing(&card, featured.map(|_| "Framework"));
        if let Some(f) = featured {
            listing.name = f.name.into();
            listing.requires = requires_names(f);
        }
        listing.version = releases.iter().find(|r| !r.prerelease).map(|r| r.tag.clone());
        let description = releases
            .iter()
            .find(|r| !r.prerelease)
            .or(releases.first())
            .map(|r| format!("Release notes for {}:\n\n{}", r.tag, r.notes))
            .unwrap_or_else(|| "This repository has no releases to download.".into());
        Ok(Details { listing, description, files: Self::files(&releases) })
    }

    fn download(&self, id: &str, file_id: &str, dest_dir: &Path, progress: Progress) -> Result<crate::sources::Downloaded> {
        let (owner, repo) = split_ref(id)?;
        let asset = self
            .client
            .releases(&owner, &repo)?
            .into_iter()
            .flat_map(|r| r.assets)
            .find(|a| a.id.to_string() == file_id)
            .ok_or_else(|| Error::Other(format!("{id} has no release file {file_id}")))?;
        let d = self.client.download(&owner, &repo, &asset, dest_dir, progress)?;
        Ok(crate::sources::Downloaded {
            check: if d.verified { "SHA-256 matches GitHub".into() } else { "GitHub published no checksum".into() },
            path: d.path,
            file_name: d.file_name,
            sha256: d.sha256,
            md5: d.md5,
            size: d.size,
            verified: d.verified,
        })
    }

    fn check_update(&self, installed: &InstalledRef) -> Result<Option<UpdateOffer>> {
        let (owner, repo) = split_ref(&installed.source_ref)?;
        let releases = self.client.releases(&owner, &repo)?;
        let Some(latest) = releases.iter().find(|r| !r.prerelease).or(releases.first()).cloned() else { return Ok(None) };
        if installed.version.as_deref() == Some(latest.tag.as_str()) {
            return Ok(None);
        }
        // Still on the newest file (e.g. installed before the tag was recorded).
        if installed.file_id.as_deref().is_some_and(|f| latest.assets.iter().any(|a| a.id.to_string() == f)) {
            return Ok(None);
        }
        let file = matching_asset(&latest, installed.file_name.as_deref())
            .and_then(|a| Self::files(std::slice::from_ref(&latest)).into_iter().find(|f| f.id == a.id.to_string()));
        // A pre-release is installed: the offer is the stable release, even
        // when its version number is lower.
        let to_stable = !latest.prerelease
            && releases.iter().any(|r| r.prerelease && installed.version.as_deref() == Some(r.tag.as_str()));
        Ok(Some(UpdateOffer { version: latest.tag.clone(), file, to_stable }))
    }
}

fn header_i64(resp: &reqwest::blocking::Response, name: &str) -> Option<i64> {
    resp.headers().get(name)?.to_str().ok()?.parse().ok()
}

fn checked(owner: &str, repo: &str) -> Result<(String, String)> {
    parse_repo_ref(&format!("{owner}/{repo}")).ok_or_else(|| Error::Other(format!("not a GitHub repository: {owner}/{repo}")))
}

/// GitHub's download hosts over HTTPS, or the client's own base (tests).
fn allowed_host(base: &str, url: &url::Url) -> bool {
    if let Ok(b) = url::Url::parse(base)
        && b.scheme() == url.scheme()
        && b.host_str() == url.host_str()
        && b.port_or_known_default() == url.port_or_known_default()
    {
        return true;
    }
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    url.scheme() == "https" && (host == "api.github.com" || DOWNLOAD_HOSTS.contains(&host.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::serve;

    #[test]
    fn parses_repo_references() {
        let r = |s: &str| parse_repo_ref(s).map(|(o, n)| format!("{o}/{n}"));
        assert_eq!(r("psiberx/cp2077-archive-xl").as_deref(), Some("psiberx/cp2077-archive-xl"));
        assert_eq!(r("https://github.com/wopss/RED4ext/releases/tag/v1.0").as_deref(), Some("wopss/RED4ext"));
        assert_eq!(r("github.com/jac3km4/redscript.git").as_deref(), Some("jac3km4/redscript"));
        assert_eq!(r("https://gitlab.com/a/b"), None);
        assert_eq!(r("a/.."), None);
        assert_eq!(r("a b/c"), None);
        assert_eq!(r("just-a-name"), None);
    }

    #[test]
    fn plans_frameworks_with_their_requirements_first() {
        let keys = |wanted: &[&str], present: &[&str]| {
            let w: Vec<String> = wanted.iter().map(|s| s.to_string()).collect();
            let p: Vec<String> = present.iter().map(|s| s.to_string()).collect();
            install_plan(&w, &p).unwrap().iter().map(|f| f.key).collect::<Vec<_>>()
        };
        assert_eq!(keys(&["codeware"], &[]), ["red4ext", "redscript", "codeware"]);
        assert_eq!(keys(&["codeware"], &["red4ext"]), ["redscript", "codeware"]);
        assert_eq!(keys(&["archivexl", "tweakxl"], &[]), ["red4ext", "archivexl", "tweakxl"]);
        assert_eq!(keys(&["psiberx/CP2077-Archive-XL"], &["red4ext"]), ["archivexl"], "by repo, any case");
        assert_eq!(keys(&["red4ext"], &["red4ext"]), ["red4ext"], "asked for explicitly: reinstall");
        assert!(install_plan(&["nope".into()], &[]).is_err());
    }

    #[test]
    fn featured_order_is_an_install_order() {
        for (i, f) in FEATURED.iter().enumerate() {
            for r in f.requires {
                let at = FEATURED.iter().position(|d| d.key == *r).expect("requirement is featured");
                assert!(at < i, "{} must come after {r}", f.key);
            }
            assert!(crate::game::detect_frameworks(std::path::Path::new("/nonexistent")).iter().any(|d| d.id == f.key));
        }
    }

    #[test]
    fn finds_the_same_asset_in_a_newer_release() {
        assert_eq!(asset_shape("ArchiveXL-1.20.0.zip"), asset_shape("ArchiveXL-1.21.3.zip"));
        assert_eq!(asset_shape("red4ext_1.25.1.zip"), asset_shape("red4ext_1.26.0.zip"));
        assert_eq!(asset_shape("cet_v1.37.1.zip"), asset_shape("cet_v1.38.0.zip"));
        assert_ne!(asset_shape("TweakXL-1.0.zip"), asset_shape("TweakXL-1.0-pdb.zip"));
        let rel = |names: &[&str]| Release {
            id: 1,
            tag: "v2".into(),
            name: None,
            published: None,
            prerelease: false,
            notes: String::new(),
            url: String::new(),
            assets: names
                .iter()
                .enumerate()
                .map(|(i, n)| Asset {
                    id: i as i64,
                    name: n.to_string(),
                    size: 1,
                    digest: None,
                    download_url: String::new(),
                    download_count: None,
                    installable: is_installable(n),
                })
                .collect(),
        };
        let r = rel(&["ArchiveXL-1.21.0.zip", "ArchiveXL-1.21.0-pdb.zip", "checksums.txt"]);
        assert_eq!(matching_asset(&r, Some("ArchiveXL-1.20.0.zip")).unwrap().name, "ArchiveXL-1.21.0.zip");
        assert_eq!(matching_asset(&r, None).unwrap().name, "ArchiveXL-1.21.0.zip", "symbols don't count");
        let r = rel(&["Mod-Lite-1.0.zip", "Mod-Full-1.0.zip"]);
        assert!(matching_asset(&r, None).is_none(), "two candidates and nothing to go on");
        let r = rel(&["only-1.0.7z", "notes.txt"]);
        assert_eq!(matching_asset(&r, Some("renamed.zip")).unwrap().name, "only-1.0.7z");
    }

    fn release_json(addr: &str, digest: &str) -> String {
        format!(
            r#"[{{"id":11,"tag_name":"1.21.0","name":"ArchiveXL 1.21.0","draft":false,"prerelease":false,
              "published_at":"2026-09-01T10:00:00Z","html_url":"https://github.com/psiberx/cp2077-archive-xl/releases/tag/1.21.0",
              "body":"- Fixes\r\n- More fixes",
              "assets":[{{"id":501,"name":"ArchiveXL-1.21.0.zip","size":11,"digest":"{digest}",
                         "browser_download_url":"{addr}/dl/ArchiveXL-1.21.0.zip","download_count":42}},
                        {{"id":502,"name":"ArchiveXL-1.21.0.pdb","size":3,"digest":null,
                         "browser_download_url":"{addr}/dl/x.pdb"}}]}},
             {{"id":10,"tag_name":"1.22.0-rc1","draft":true,"assets":[]}}]"#
        )
    }

    #[test]
    fn lists_releases_and_verifies_downloads() {
        // sha256("hello world")
        let good = "sha256:b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        let (addr, seen) = serve(vec![]);
        let (addr2, _) = serve(vec![
            ("/repos/psiberx/cp2077-archive-xl/releases", 200, vec![], release_json(&addr, good)),
        ]);
        // The download server is a second mock; point the asset at it.
        let c = Client::with_base(&addr2).unwrap();
        let rels = c.releases("psiberx", "cp2077-archive-xl").unwrap();
        assert_eq!(rels.len(), 1, "drafts are left out");
        let r = &rels[0];
        assert_eq!(r.tag, "1.21.0");
        assert_eq!(r.notes, "- Fixes\n- More fixes");
        assert_eq!(r.assets.iter().filter(|a| a.installable).count(), 1);
        // A download URL on another host than GitHub (or the test base) is refused.
        let asset = &r.assets[0];
        let dir = tempfile::tempdir().unwrap();
        let err = c.download("psiberx", "cp2077-archive-xl", asset, dir.path(), &mut |_, _| {}).unwrap_err();
        assert!(err.to_string().contains("refusing"), "{err}");
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn download_checks_size_and_digest() {
        let good = "sha256:b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        let (addr, _) = serve(vec![
            ("/dl/ok.zip", 302, vec![("Location", "/cdn/ok.zip".into())], String::new()),
            ("/cdn/ok.zip", 200, vec![], "hello world".into()),
            ("/dl/bad.zip", 200, vec![], "hello there".into()),
            ("/dl/long.zip", 200, vec![], "hello world, and more".into()),
        ]);
        let c = Client::with_base(&addr).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let asset = |name: &str, digest: Option<&str>| Asset {
            id: 7,
            name: name.into(),
            size: 11,
            digest: digest.map(String::from),
            download_url: format!("{addr}/dl/{name}"),
            download_count: None,
            installable: true,
        };
        let d = c.download("a", "b", &asset("ok.zip", Some(good)), dir.path(), &mut |_, _| {}).unwrap();
        assert!(d.verified);
        assert_eq!(std::fs::read(&d.path).unwrap(), b"hello world");
        let d = c.download("a", "b", &asset("ok.zip", None), dir.path(), &mut |_, _| {}).unwrap();
        assert!(!d.verified, "no published digest: kept but unverified");
        let e = c.download("a", "b", &asset("bad.zip", Some(good)), dir.path(), &mut |_, _| {}).unwrap_err();
        assert!(matches!(e, Error::Integrity(_)), "{e}");
        let e = c.download("a", "b", &asset("long.zip", None), dir.path(), &mut |_, _| {}).unwrap_err();
        assert!(matches!(e, Error::Integrity(_)), "{e}");
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left.len(), 1, "failed downloads leave nothing behind: {left:?}");
    }

    #[test]
    fn source_shows_details_downloads_and_offers_updates() {
        let good = "sha256:b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        let repo = r#"{"full_name":"psiberx/cp2077-archive-xl","description":"ArchiveXL","stargazers_count":300,"topics":[],"archived":false}"#;
        // `{addr}` in a body is replaced with the mock's own address.
        let (base, _) = serve(vec![
            ("/repos/psiberx/cp2077-archive-xl/releases", 200, vec![], release_json("{addr}", good)),
            ("/repos/psiberx/cp2077-archive-xl", 200, vec![], repo.into()),
            ("/dl/ArchiveXL-1.21.0.zip", 200, vec![], "hello world".into()),
        ]);
        let src = GitHubSource::with_client(Client::with_base(&base).unwrap());
        let d = src.details("psiberx/cp2077-archive-xl").unwrap();
        assert_eq!(d.listing.name, "ArchiveXL", "featured repos keep their friendly name");
        assert_eq!(d.listing.category.as_deref(), Some("Framework"));
        assert_eq!(d.listing.version.as_deref(), Some("1.21.0"));
        assert!(d.description.contains("Fixes"));
        let zip = d.files.iter().find(|f| f.installable).unwrap();
        assert_eq!(zip.group, "1.21.0 (latest)");
        assert!(zip.verifiable);
        let dir = tempfile::tempdir().unwrap();
        let got = src.download("psiberx/cp2077-archive-xl", &zip.id, dir.path(), &mut |_, _| {}).unwrap();
        assert!(got.verified);
        assert_eq!(got.check, "SHA-256 matches GitHub");

        let old = InstalledRef {
            source_ref: "psiberx/cp2077-archive-xl".into(),
            file_id: Some("400".into()),
            file_name: Some("ArchiveXL-1.20.0.zip".into()),
            version: Some("1.20.0".into()),
        };
        let offer = src.check_update(&old).unwrap().unwrap();
        assert_eq!(offer.version, "1.21.0");
        assert_eq!(offer.file.unwrap().file_name, "ArchiveXL-1.21.0.zip");
        let current = InstalledRef { version: Some("1.21.0".into()), ..old };
        assert!(src.check_update(&current).unwrap().is_none());
    }

    #[test]
    fn symbols_and_other_systems_are_not_installable() {
        for n in ["red4ext-1.30.0.zip", "cet_1.37.1.zip", "redscript-v0.5.31-windows.zip", "TweakXL-1.11.4.zip", "Mod.7z"] {
            assert!(is_installable(n), "{n}");
        }
        for n in [
            "red4ext-symbols-1.30.0.zip",
            "red4ext_1.29.1_pdbs.zip",
            "windows-latest-x64-release-pdb.zip",
            "TweakXL-1.0-pdb.zip",
            "redscript-v0.5.31-macos.zip",
            "ArchiveXL-1.21.0.pdb",
        ] {
            assert!(!is_installable(n), "{n}");
        }
    }

    #[test]
    fn stable_release_comes_first_and_replaces_an_installed_prerelease() {
        // redscript's releases: pre-releases newer than the stable one.
        let rel = |id: i64, tag: &str, pre: bool| {
            format!(
                r#"{{"id":{id},"tag_name":"{tag}","prerelease":{pre},"assets":[
                    {{"id":{a},"name":"redscript-{tag}-windows.zip","size":1,"browser_download_url":"https://github.com/jac3km4/redscript/releases/download/{tag}/w.zip"}},
                    {{"id":{b},"name":"redscript-{tag}-macos.zip","size":1,"browser_download_url":"https://github.com/jac3km4/redscript/releases/download/{tag}/m.zip"}}]}}"#,
                a = id * 10,
                b = id * 10 + 1
            )
        };
        let body = format!("[{},{},{}]", rel(3, "v1.0.0-preview.22", true), rel(2, "v0.5.31", false), rel(1, "v0.5.30", false));
        let repo = r#"{"full_name":"jac3km4/redscript","description":"","stargazers_count":1,"topics":[],"archived":false}"#;
        let (base, _) = serve(vec![
            ("/repos/jac3km4/redscript/releases", 200, vec![], body),
            ("/repos/jac3km4/redscript", 200, vec![], repo.into()),
        ]);
        let src = GitHubSource::with_client(Client::with_base(&base).unwrap());
        let d = src.details("jac3km4/redscript").unwrap();
        assert_eq!(d.files[0].group, "v0.5.31 (latest)");
        let installable: Vec<_> = d.files.iter().filter(|f| f.installable && f.group == d.files[0].group).collect();
        assert_eq!(installable.len(), 1, "only the Windows zip installs, so it can be picked without asking");
        assert_eq!(installable[0].file_name, "redscript-v0.5.31-windows.zip");

        let preview = InstalledRef {
            source_ref: "jac3km4/redscript".into(),
            file_id: Some("30".into()),
            file_name: Some("redscript-v1.0.0-preview.22-windows.zip".into()),
            version: Some("v1.0.0-preview.22".into()),
        };
        let offer = src.check_update(&preview).unwrap().unwrap();
        assert_eq!(offer.version, "v0.5.31");
        assert!(offer.to_stable);
        assert_eq!(offer.file.unwrap().file_name, "redscript-v0.5.31-windows.zip");
        let older = InstalledRef { version: Some("v0.5.30".into()), file_id: Some("10".into()), ..preview };
        assert!(!src.check_update(&older).unwrap().unwrap().to_stable);
    }

    #[test]
    fn search_adds_game_terms_and_reports_rate_limit() {
        let items = r#"{"total_count":1,"items":[{"full_name":"psiberx/cp2077-archive-xl","description":"ArchiveXL","stargazers_count":300,
            "pushed_at":"2026-09-01T10:00:00Z","topics":["cyberpunk2077"],"archived":false}]}"#;
        let (addr, seen) = serve(vec![
            ("/search/repositories", 200, vec![], items.into()),
            ("/repos/a/b/releases", 403, vec![("x-ratelimit-remaining", "0".into()), ("x-ratelimit-reset", "0".into())], r#"{"message":"API rate limit exceeded"}"#.into()),
        ]);
        let c = Client::with_base(&addr).unwrap();
        let p = c.search("archive", 1).unwrap();
        assert_eq!(p.repos[0].repo, "psiberx/cp2077-archive-xl");
        assert_eq!(p.repos[0].url, "https://github.com/psiberx/cp2077-archive-xl");
        let line = seen.lock().unwrap()[0].line.clone();
        assert!(line.contains("q=archive+cyberpunk+2077"), "{line}");
        // Cached: a second search doesn't hit the server.
        c.search("archive", 1).unwrap();
        assert_eq!(seen.lock().unwrap().len(), 1);
        let e = c.releases("a", "b").unwrap_err();
        assert!(e.to_string().contains("60 requests an hour"), "{e}");
    }
}
