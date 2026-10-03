//! Browsing Nexus Mods from inside the app. Search and sorted lists go through
//! the v2 GraphQL API; Nexus' own trending/latest lists and mod pages through
//! v1. Everything here is read-only, cached briefly and sent at
//! [`Priority::Browse`] so it can't eat the quota downloads need.

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::nexus::{Client, FileInfo, ModInfo, Priority, parse_time};
use crate::{Error, NEXUS_GAME_DOMAIN, Result};

const LIST_TTL: Duration = Duration::from_secs(5 * 60);
const DETAIL_TTL: Duration = Duration::from_secs(10 * 60);
pub const DEFAULT_PAGE: u32 = 20;
pub const MAX_PAGE: u32 = 50;
const MAX_OFFSET: u32 = 10_000;
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
}

#[derive(Debug, Clone, Serialize)]
pub struct Page {
    pub mods: Vec<ModCard>,
    /// Total matches, when Nexus reports it (GraphQL only).
    pub total: Option<i64>,
    pub offset: u32,
    pub count: u32,
    /// Results left out because they are marked adult and the user hasn't
    /// opted in.
    pub hidden_adult: u32,
}

/// Nexus' curated v1 lists (10 mods each).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum List {
    Trending,
    LatestAdded,
    LatestUpdated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sort {
    /// Best match for the search text; most endorsed without text.
    #[default]
    Relevance,
    Endorsements,
    Downloads,
    Updated,
    Created,
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
}

#[derive(Debug, Clone, Serialize)]
pub struct ModDetails {
    pub info: ModInfo,
    /// The mod page description with BBCode/HTML removed.
    pub description_text: String,
    pub files: Vec<FileInfo>,
    pub page_url: String,
}

/// Mod page on the website; `file_id` jumps to that file's entry on the
/// Files tab, where free accounts click "Mod Manager Download".
pub fn mod_page_url(mod_id: i64, file_id: Option<i64>) -> String {
    match file_id {
        Some(f) => format!("https://www.nexusmods.com/{NEXUS_GAME_DOMAIN}/mods/{mod_id}?tab=files&file_id={f}"),
        None => format!("https://www.nexusmods.com/{NEXUS_GAME_DOMAIN}/mods/{mod_id}"),
    }
}

/// The UI's content security policy only allows images from this host.
fn safe_image(u: Option<&str>) -> Option<String> {
    let url = url::Url::parse(u?.trim()).ok()?;
    (url.scheme() == "https" && url.host_str() == Some("staticdelivery.nexusmods.com")).then(|| url.to_string())
}

fn clean_line(s: Option<&str>, max: usize) -> Option<String> {
    let s: String = s?.chars().filter(|c| !c.is_control() || *c == '\n').take(max).collect();
    let s = decode_entities(s.trim());
    (!s.is_empty()).then_some(s)
}

fn card_from_v1(m: &ModInfo) -> Option<ModCard> {
    if !m.available || m.status.as_deref().is_some_and(|s| s != "published") {
        return None;
    }
    Some(ModCard {
        mod_id: m.mod_id,
        name: clean_line(m.name.as_deref(), 300)?,
        summary: clean_line(m.summary.as_deref(), 1000),
        author: clean_line(m.author.as_deref().or(m.uploaded_by.as_deref()), 200),
        version: clean_line(m.version.as_deref(), 100),
        picture_url: safe_image(m.picture_url.as_deref()),
        endorsements: m.endorsement_count,
        downloads: m.mod_downloads,
        created: m.created_timestamp,
        updated: m.updated_timestamp,
        adult: m.contains_adult_content,
    })
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
/// it, a plain `*text*` wildcard on the name.
pub fn graphql_variables(q: &Search, stemmed: bool) -> Value {
    let text = clean_query(&q.text);
    let mut filter = json!({ "gameDomainName": [{ "value": NEXUS_GAME_DOMAIN, "op": "EQUALS" }] });
    if !text.is_empty() {
        if stemmed {
            filter["nameStemmed"] = json!([{ "value": text, "op": "MATCHES" }]);
        } else {
            let wild = text.replace(['*', '?'], " ");
            filter["name"] = json!([{ "value": format!("*{}*", wild.trim()), "op": "WILDCARD" }]);
        }
    }
    let key = match q.sort {
        Sort::Relevance if !text.is_empty() => "relevance",
        Sort::Relevance | Sort::Endorsements => "endorsements",
        Sort::Downloads => "downloads",
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

impl Client {
    fn cached_v1<T: DeserializeOwned>(&self, path: &str, ttl: Duration) -> Result<T> {
        let key = format!("v1:{path}");
        if let Some(v) = self.shared.cached(&key, ttl) {
            return Ok(serde_json::from_value(v)?);
        }
        let v: Value = self.get_with(Priority::Browse, path, &[])?;
        let out = serde_json::from_value(v.clone())?;
        self.shared.store(key, v);
        Ok(out)
    }

    /// One of Nexus' curated lists.
    pub fn browse_list(&self, which: List, include_adult: bool) -> Result<Page> {
        let name = match which {
            List::Trending => "trending",
            List::LatestAdded => "latest_added",
            List::LatestUpdated => "latest_updated",
        };
        let mods: Vec<ModInfo> = self.cached_v1(&format!("/games/{NEXUS_GAME_DOMAIN}/mods/{name}.json"), LIST_TTL)?;
        let (mods, hidden_adult) = split_adult(mods.iter().filter_map(card_from_v1).collect(), include_adult);
        let count = mods.len() as u32;
        Ok(Page { mods, total: None, offset: 0, count, hidden_adult })
    }

    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let body = json!({ "query": query, "variables": variables });
        let key = format!("gql:{body}");
        if let Some(v) = self.shared.cached(&key, LIST_TTL) {
            return Ok(v);
        }
        let resp = self.send_api(Priority::Browse, self.http.post(&self.graphql).json(&body))?;
        let status = resp.status();
        let text = resp.text()?;
        let v: Value = serde_json::from_str(&text).map_err(|_| {
            Error::Nexus(format!("{status}: unexpected reply from the Nexus search API: {}", text.chars().take(200).collect::<String>()))
        })?;
        if let Some(err) = v.pointer("/errors/0/message").and_then(Value::as_str) {
            return Err(Error::Nexus(format!("search API: {}", err.chars().take(300).collect::<String>())));
        }
        if !status.is_success() {
            return Err(Error::Nexus(format!("{status}: Nexus search API request failed")));
        }
        let data = v.get("data").cloned().filter(|d| !d.is_null()).ok_or_else(|| Error::Nexus("search API returned no data".into()))?;
        self.shared.store(key, data.clone());
        Ok(data)
    }

    /// Search by name and/or list mods sorted by endorsements, downloads or date.
    pub fn search(&self, q: &Search, include_adult: bool) -> Result<Page> {
        let has_text = !clean_query(&q.text).is_empty();
        let data = match self.graphql(MODS_QUERY, graphql_variables(q, true)) {
            // If Nexus rejects the full-text filter, fall back to a wildcard.
            Err(Error::Nexus(msg)) if has_text && msg.starts_with("search API:") => {
                log::warn!("stemmed search failed ({msg}); retrying with wildcard");
                self.graphql(MODS_QUERY, graphql_variables(q, false))?
            }
            r => r?,
        };
        let mods = data.pointer("/mods/nodes").and_then(Value::as_array).cloned().unwrap_or_default();
        let total = data.pointer("/mods/totalCount").and_then(Value::as_i64);
        let returned = mods.len() as u32;
        let (mods, hidden_adult) = split_adult(mods.iter().filter_map(card_from_graphql).collect(), include_adult);
        Ok(Page { mods, total, offset: q.offset.min(MAX_OFFSET), count: returned, hidden_adult })
    }

    /// Mod page and file list, for browsing (cached).
    pub fn mod_details(&self, mod_id: i64) -> Result<ModDetails> {
        let info: ModInfo = self.cached_v1(&format!("/games/{NEXUS_GAME_DOMAIN}/mods/{mod_id}.json"), DETAIL_TTL)?;
        #[derive(Deserialize)]
        struct Files {
            files: Vec<FileInfo>,
        }
        let files: Files = self.cached_v1(&format!("/games/{NEXUS_GAME_DOMAIN}/mods/{mod_id}/files.json"), DETAIL_TTL)?;
        let description_text = info.description.as_deref().map(bbcode_to_text).unwrap_or_default();
        Ok(ModDetails { info, description_text, files: files.files, page_url: mod_page_url(mod_id, None) })
    }
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let end = rest[..rest.len().min(12)].find(';');
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
        let Some(end) = rest[..rest.len().min(300)].find(close) else {
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
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A canned response: (path substring, status, extra headers, body).
    type Canned = (&'static str, u16, Vec<(&'static str, String)>, String);

    #[derive(Debug, Clone)]
    struct Seen {
        line: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    /// Minimal HTTP/1.1 server answering from recorded fixtures.
    fn serve(responses: Vec<Canned>) -> (String, Arc<Mutex<Vec<Seen>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let responses = Arc::new(Mutex::new(responses));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut headers = Vec::new();
                let mut len = 0;
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    let h = h.trim_end();
                    if h.is_empty() {
                        break;
                    }
                    let (k, v) = h.split_once(':').unwrap();
                    let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_string());
                    if k == "content-length" {
                        len = v.parse().unwrap();
                    }
                    headers.push((k, v));
                }
                let mut body = vec![0; len];
                reader.read_exact(&mut body).unwrap();
                let body = String::from_utf8(body).unwrap();
                log.lock().unwrap().push(Seen { line: line.trim().to_string(), headers, body: body.clone() });
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let mut rs = responses.lock().unwrap();
                // First matching response is used once, unless it's the last match.
                let idx = rs.iter().position(|(p, ..)| path.contains(p));
                let (status, extra, out) = match idx {
                    Some(i) => {
                        let matches = rs.iter().filter(|(p, ..)| path.contains(p)).count();
                        let r = if matches > 1 { rs.remove(i) } else { rs[i].clone() };
                        (r.1, r.2, r.3)
                    }
                    None => (404, vec![], r#"{"message":"No route"}"#.to_string()),
                };
                let mut resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                    out.len()
                );
                for (k, v) in extra {
                    resp.push_str(&format!("{k}: {v}\r\n"));
                }
                resp.push_str("\r\n");
                resp.push_str(&out);
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        (addr, seen)
    }

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
    fn trending_list_filters_and_tracks_quota() {
        let (addr, seen) = serve(vec![("/mods/trending.json", 200, rl(99, 19876), fixture("trending.json"))]);
        let c = client(&addr);
        let page = c.browse_list(List::Trending, false).unwrap();
        let names: Vec<_> = page.mods.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["Cyber Engine Tweaks", "Better Vehicle Handling & Physics", "Appearance Menu Mod"]);
        assert_eq!(page.hidden_adult, 1, "adult mod hidden by default");
        let cet = &page.mods[0];
        assert_eq!(cet.mod_id, 107);
        assert_eq!(cet.endorsements, Some(91234));
        assert!(cet.picture_url.as_deref().unwrap().starts_with("https://staticdelivery.nexusmods.com/"));
        // Off-host image URLs are dropped rather than shown.
        assert_eq!(page.mods[2].picture_url, None);
        // Entities in names are decoded.
        assert_eq!(page.mods[1].name, "Better Vehicle Handling & Physics");

        let with_adult = c.browse_list(List::Trending, true).unwrap();
        assert_eq!(with_adult.mods.len(), 4);
        // Second call was served from the cache.
        assert_eq!(seen.lock().unwrap().len(), 1);

        let req = &seen.lock().unwrap()[0];
        assert!(req.line.starts_with("GET /v1/games/cyberpunk2077/mods/trending.json"));
        assert!(req.headers.iter().any(|(k, v)| k == "apikey" && v == "test-key"));
        assert!(req.headers.iter().any(|(k, _)| k == "application-name"));

        let rate = c.rate();
        assert_eq!(rate.hourly_remaining, Some(99));
        assert_eq!(rate.daily_remaining, Some(19876));
        assert_eq!(rate.hourly_reset, parse_time("2099-01-01T13:00:00Z"));
    }

    #[test]
    fn search_uses_graphql_with_filters_and_paging() {
        let (addr, seen) = serve(vec![("graphql", 200, vec![], fixture("search.json"))]);
        let c = client(&addr);
        let q = Search { text: "  vehicle\u{7}  handling ".into(), sort: Sort::Relevance, offset: 20, count: 500 };
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
        assert_eq!(v["filter"]["name"][0]["value"], "*a b*");
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
        assert_eq!(second["variables"]["filter"]["name"][0]["value"], "*vehicle*");
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
        let (addr, seen) = serve(vec![("/mods/trending.json", 429, headers, r#"{"msg":"Rate limit exceeded"}"#.into())]);
        let c = client(&addr);
        let e = c.browse_list(List::Trending, false).unwrap_err().to_string();
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
            ("/mods/trending.json", 200, rl(99, 100), fixture("trending.json")),
        ]);
        let c = client(&addr);
        assert_eq!(c.validate().unwrap().name, "V");
        let e = c.browse_list(List::Trending, false).unwrap_err().to_string();
        assert!(e.contains("browsing is paused"), "{e}");
        assert!(c.validate().is_ok(), "essential calls still go out");
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[test]
    fn mod_details_include_plain_text_description_and_files() {
        let (addr, seen) = serve(vec![
            ("/mods/107/files.json", 200, rl(98, 19870), fixture("files_107.json")),
            ("/mods/107.json", 200, rl(97, 19869), fixture("mod_107.json")),
        ]);
        let c = client(&addr);
        let d = c.mod_details(107).unwrap();
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
        c.mod_details(107).unwrap();
        assert_eq!(seen.lock().unwrap().len(), 2, "second view cached");
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
    }

    #[test]
    fn page_urls() {
        assert_eq!(mod_page_url(107, Some(5)), "https://www.nexusmods.com/cyberpunk2077/mods/107?tab=files&file_id=5");
    }
}
