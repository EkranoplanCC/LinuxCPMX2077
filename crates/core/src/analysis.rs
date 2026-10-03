//! Compatibility analysis, layer 1: a deterministic index of what every
//! installed mod touches in the game, and the conflicts that follow from it.
//!
//! Each mod's deployed files are scanned for:
//! - resources inside `.archive` files (by depot path hash), which override
//!   each other and the base game;
//! - redscript annotations (`@replaceMethod`, `@wrapMethod`, `@addField`,
//!   `@addMethod`, `@replaceGlobal`);
//! - TweakXL records and the properties they set (`r6/tweaks/*.yaml`);
//! - ArchiveXL resource patches (`*.xl`);
//! - Cyber Engine Tweaks `Override` / `Observe` hooks;
//! - which modding frameworks the mod needs.
//!
//! No LLM is involved; the same index feeds the graph view and can be handed
//! to an external agent read-only.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde::Serialize;

use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Resource hash inside an `.archive`.
    Resource,
    RedsReplaceMethod,
    RedsWrapMethod,
    RedsAddMethod,
    RedsAddField,
    RedsReplaceGlobal,
    /// `Record.ID` a tweak touches.
    TweakRecord,
    /// `Record.ID.property` a tweak sets.
    TweakProperty,
    /// Resource an ArchiveXL `resource: patch` targets.
    XlPatch,
    CetOverride,
    CetObserve,
    /// Framework the mod needs (`cet`, `red4ext`, `redscript`, ...).
    Requires,
    /// A RED4ext plugin DLL the mod provides.
    Red4extPlugin,
}

impl Kind {
    pub const ALL: [Kind; 13] = [
        Kind::Resource,
        Kind::RedsReplaceMethod,
        Kind::RedsWrapMethod,
        Kind::RedsAddMethod,
        Kind::RedsAddField,
        Kind::RedsReplaceGlobal,
        Kind::TweakRecord,
        Kind::TweakProperty,
        Kind::XlPatch,
        Kind::CetOverride,
        Kind::CetObserve,
        Kind::Requires,
        Kind::Red4extPlugin,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Resource => "resource",
            Kind::RedsReplaceMethod => "reds_replace_method",
            Kind::RedsWrapMethod => "reds_wrap_method",
            Kind::RedsAddMethod => "reds_add_method",
            Kind::RedsAddField => "reds_add_field",
            Kind::RedsReplaceGlobal => "reds_replace_global",
            Kind::TweakRecord => "tweak_record",
            Kind::TweakProperty => "tweak_property",
            Kind::XlPatch => "xl_patch",
            Kind::CetOverride => "cet_override",
            Kind::CetObserve => "cet_observe",
            Kind::Requires => "requires",
            Kind::Red4extPlugin => "red4ext_plugin",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

/// Bump when the scanners change so existing indexes are rebuilt.
pub const SCANNER_VERSION: i64 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct Touch {
    pub kind: Kind,
    /// What is touched: a resource hash, `Class.method`, a record id, ...
    pub key: String,
    /// The mod file (game-relative) where it was found.
    pub file: String,
}

// ---------------------------------------------------------------------------
// .archive (RDAR) index

/// FNV-1a 64, the hash the game uses for depot paths.
pub fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Depot paths are hashed lowercase with backslashes.
pub fn depot_hash(path: &str) -> u64 {
    fnv1a64(&path.trim().replace('/', "\\").to_lowercase())
}

/// Resource hashes listed in an `.archive` file's index. Only the index is
/// read, so this is fast even for multi-GB archives.
pub fn archive_hashes(path: &Path) -> Result<Vec<u64>> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let mut hdr = [0u8; 40];
    f.read_exact(&mut hdr)?;
    if &hdr[0..4] != b"RDAR" {
        return Err(Error::Other(format!("{} is not an RDAR archive", path.display())));
    }
    let index_pos = u64::from_le_bytes(hdr[8..16].try_into().unwrap());
    let index_size = u32::from_le_bytes(hdr[16..20].try_into().unwrap()) as u64;
    if index_pos + index_size > len || index_size < 28 {
        return Err(Error::Other(format!("{}: index out of bounds", path.display())));
    }
    f.seek(SeekFrom::Start(index_pos))?;
    let mut idx = [0u8; 28];
    f.read_exact(&mut idx)?;
    let count = u32::from_le_bytes(idx[16..20].try_into().unwrap()) as u64;
    // 56 bytes per file entry; refuse counts the index can't hold.
    if 28 + count * 56 > index_size {
        return Err(Error::Other(format!("{}: corrupt file table", path.display())));
    }
    let mut table = vec![0u8; (count * 56) as usize];
    f.read_exact(&mut table)?;
    Ok(table.chunks_exact(56).map(|e| u64::from_le_bytes(e[0..8].try_into().unwrap())).collect())
}

/// Hashes of every resource in the base game's archives.
pub fn base_game_hashes(game_dir: &Path) -> BTreeSet<u64> {
    let mut out = BTreeSet::new();
    for sub in ["archive/pc/content", "archive/pc/ep1"] {
        let Ok(rd) = std::fs::read_dir(game_dir.join(sub)) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("archive"))
                && let Ok(h) = archive_hashes(&p) {
                    out.extend(h);
                }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Text scanners

fn strip_line_comment<'a>(line: &'a str, marker: &str) -> &'a str {
    match line.find(marker) {
        Some(i) => &line[..i],
        None => line,
    }
}

/// redscript: `@wrapMethod(Class)` followed (possibly on the next line) by
/// `func name(`.
pub fn scan_redscript(src: &str) -> Vec<(Kind, String)> {
    let mut out = Vec::new();
    let mut pending: Option<(Kind, String)> = None;
    for raw in src.lines() {
        let line = strip_line_comment(raw, "//").trim();
        let mut rest = line;
        while let Some(at) = rest.find('@') {
            let after = &rest[at + 1..];
            let name: String = after.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            let arg = after[name.len()..]
                .strip_prefix('(')
                .and_then(|a| a.split(')').next())
                .map(|a| a.trim().to_string())
                .unwrap_or_default();
            let kind = match name.as_str() {
                "replaceMethod" => Some(Kind::RedsReplaceMethod),
                "wrapMethod" => Some(Kind::RedsWrapMethod),
                "addMethod" => Some(Kind::RedsAddMethod),
                "addField" => Some(Kind::RedsAddField),
                "replaceGlobal" => Some(Kind::RedsReplaceGlobal),
                _ => None,
            };
            if let Some(k) = kind {
                pending = Some((k, arg));
            }
            rest = &after[name.len()..];
        }
        let Some((kind, class)) = pending.clone() else { continue };
        let member = if kind == Kind::RedsAddField {
            // `@addField(Class) public let name: Type;`
            line.split_whitespace()
                .skip_while(|w| *w != "let")
                .nth(1)
                .map(|w| w.trim_end_matches(':').trim_end_matches(';').to_string())
        } else {
            line.split_whitespace()
                .skip_while(|w| *w != "func")
                .nth(1)
                .map(|w| w.split('(').next().unwrap_or(w).to_string())
        };
        if let Some(m) = member.filter(|m| !m.is_empty()) {
            let key = if class.is_empty() { m } else { format!("{class}.{m}") };
            out.push((kind, key));
            pending = None;
        }
    }
    out
}

/// CET Lua: `Override('Class', 'Method', ...)`, `Observe(...)`,
/// `ObserveBefore(...)`, `ObserveAfter(...)`.
pub fn scan_cet_lua(src: &str) -> Vec<(Kind, String)> {
    let mut out = Vec::new();
    for raw in src.lines() {
        let line = strip_line_comment(raw, "--");
        for (needle, kind) in [
            ("Override(", Kind::CetOverride),
            ("ObserveBefore(", Kind::CetObserve),
            ("ObserveAfter(", Kind::CetObserve),
            ("Observe(", Kind::CetObserve),
        ] {
            let mut rest = line;
            while let Some(i) = rest.find(needle) {
                // Skip ObserveBefore/After when matching plain Observe(.
                let prev = rest[..i].chars().last();
                let args = &rest[i + needle.len()..];
                rest = args;
                if prev.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                let strs: Vec<String> = args
                    .split(',')
                    .take(2)
                    .map(|a| a.trim().trim_matches(|c| c == '\'' || c == '"').to_string())
                    .collect();
                if strs.len() == 2 && !strs[0].is_empty() && !strs[1].contains(')') {
                    out.push((kind, format!("{}.{}", strs[0], strs[1])));
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn yaml_key(line: &str) -> Option<(usize, String)> {
    let trimmed = line.trim_end();
    if trimmed.trim_start().starts_with('#') || trimmed.trim_start().starts_with('-') || trimmed.trim().is_empty() {
        return None;
    }
    let indent = trimmed.len() - trimmed.trim_start().len();
    let body = trimmed.trim_start();
    let colon = body.find(':')?;
    let key = body[..colon].trim().trim_matches(|c| c == '\'' || c == '"');
    if key.is_empty() || key.contains(' ') {
        return None;
    }
    Some((indent, key.to_string()))
}

/// TweakXL YAML: top-level keys are record ids; the keys under them are the
/// properties being set.
pub fn scan_tweak_yaml(src: &str) -> Vec<(Kind, String)> {
    let mut out = Vec::new();
    let mut record: Option<String> = None;
    let mut prop_indent: Option<usize> = None;
    for line in src.lines() {
        let Some((indent, key)) = yaml_key(line) else { continue };
        if indent == 0 {
            out.push((Kind::TweakRecord, key.clone()));
            record = Some(key);
            prop_indent = None;
        } else if let Some(r) = &record {
            let pi = *prop_indent.get_or_insert(indent);
            if indent == pi && !key.starts_with('$') {
                out.push((Kind::TweakProperty, format!("{r}.{key}")));
            }
        }
    }
    out
}

/// ArchiveXL: targets of `resource: patch:` blocks.
pub fn scan_xl(src: &str) -> Vec<(Kind, String)> {
    let mut out = Vec::new();
    let mut in_resource = false;
    let mut in_patch: Option<usize> = None;
    for line in src.lines() {
        let trimmed = line.trim_end();
        let indent = trimmed.len() - trimmed.trim_start().len();
        let body = trimmed.trim_start();
        if body.is_empty() || body.starts_with('#') {
            continue;
        }
        if indent == 0 {
            in_resource = body.starts_with("resource:");
            in_patch = None;
            continue;
        }
        if in_resource && body.starts_with("patch:") {
            in_patch = Some(indent);
            continue;
        }
        if let Some(pi) = in_patch {
            if indent <= pi {
                in_patch = None;
                continue;
            }
            // Patch targets are list items or inline lists under each source.
            if let Some(item) = body.strip_prefix("- ") {
                out.push((Kind::XlPatch, item.trim().trim_matches(|c| c == '\'' || c == '"').to_lowercase().replace('/', "\\")));
            } else if let Some((_, v)) = body.split_once(':') {
                let v = v.trim();
                if let Some(list) = v.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                    for t in list.split(',') {
                        let t = t.trim().trim_matches(|c| c == '\'' || c == '"');
                        if !t.is_empty() {
                            out.push((Kind::XlPatch, t.to_lowercase().replace('/', "\\")));
                        }
                    }
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Scanning a mod

fn lower_ext(p: &str) -> String {
    Path::new(p).extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default()
}

/// Scan one mod. `files` are `(game-relative target, absolute path of the
/// staged copy)` pairs.
pub fn scan_mod(files: &[(String, std::path::PathBuf)]) -> Vec<Touch> {
    let mut out: BTreeSet<Touch> = BTreeSet::new();
    let mut add = |kind: Kind, key: String, file: &str| {
        out.insert(Touch { kind, key, file: file.to_string() });
    };
    for (target, path) in files {
        let lower = target.to_lowercase();
        let ext = lower_ext(&lower);
        let read_text = || -> Option<String> {
            let meta = std::fs::metadata(path).ok()?;
            // Scripts and configs are small; skip anything suspiciously big.
            if meta.len() > 8 * 1024 * 1024 {
                return None;
            }
            std::fs::read(path).ok().map(|b| String::from_utf8_lossy(&b).into_owned())
        };
        match ext.as_str() {
            "archive" => {
                if let Ok(hashes) = archive_hashes(path) {
                    for h in hashes {
                        add(Kind::Resource, format!("{h:016x}"), target);
                    }
                }
            }
            "reds" => {
                add(Kind::Requires, "redscript".into(), target);
                if let Some(src) = read_text() {
                    if src.contains("import Codeware") {
                        add(Kind::Requires, "codeware".into(), target);
                    }
                    for (k, key) in scan_redscript(&src) {
                        add(k, key, target);
                    }
                }
            }
            "xl" => {
                add(Kind::Requires, "archivexl".into(), target);
                if let Some(src) = read_text() {
                    for (k, key) in scan_xl(&src) {
                        add(k, key, target);
                    }
                }
            }
            "yaml" | "yml" if lower.starts_with("r6/tweaks/") => {
                add(Kind::Requires, "tweakxl".into(), target);
                if let Some(src) = read_text() {
                    for (k, key) in scan_tweak_yaml(&src) {
                        add(k, key, target);
                    }
                }
            }
            "lua" if lower.starts_with("bin/x64/plugins/cyber_engine_tweaks/mods/") => {
                add(Kind::Requires, "cet".into(), target);
                if let Some(src) = read_text() {
                    for (k, key) in scan_cet_lua(&src) {
                        add(k, key, target);
                    }
                }
            }
            "dll" if lower.starts_with("red4ext/plugins/") => {
                add(Kind::Requires, "red4ext".into(), target);
                let name = Path::new(target).file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
                add(Kind::Red4extPlugin, name, target);
            }
            _ => {}
        }
        if lower.starts_with("mods/") && lower.ends_with("/info.json") {
            add(Kind::Requires, "redmod".into(), target);
        }
    }
    out.into_iter().collect()
}

// ---------------------------------------------------------------------------
// Findings

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub severity: Severity,
    pub kind: Kind,
    pub key: String,
    pub mod_ids: Vec<i64>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModSummary {
    pub mod_id: i64,
    pub name: String,
    pub counts: BTreeMap<Kind, usize>,
    pub requires: Vec<String>,
    pub base_overrides: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub findings: Vec<Finding>,
    pub mods: Vec<ModSummary>,
    pub graph: Graph,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeType {
    Mod,
    Framework,
    /// A game class touched by redscript or CET.
    Class,
    /// A TweakDB record touched by more than one mod.
    Record,
    /// Tweak records only one mod touches, aggregated per mod.
    Records,
    /// Resources several mods replace.
    SharedResources,
    BaseGame,
}

#[derive(Debug, Clone, Serialize)]
pub struct Node {
    pub id: String,
    pub label: String,
    #[serde(rename = "type")]
    pub node_type: NodeType,
    /// For mods: their id; otherwise None.
    pub mod_id: Option<i64>,
    pub missing: bool,
    pub detail: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    /// What the mod does: "requires", "replaces", "wraps", "adds", "overrides",
    /// "observes", "tweaks", "patches", "overrides resources".
    pub label: String,
    pub weight: usize,
    pub conflict: bool,
    pub detail: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

/// Mods, the frameworks they need and the game parts they touch.
pub fn build_graph(mods: &[ModIndex], base: &BTreeSet<u64>, installed_frameworks: &BTreeSet<String>, findings: &[Finding]) -> Graph {
    let mut g = Graph::default();
    let mut node_ids: BTreeSet<String> = BTreeSet::new();
    let mut add_node = |g: &mut Graph, id: String, label: String, t: NodeType, mod_id: Option<i64>, missing: bool| {
        if node_ids.insert(id.clone()) {
            g.nodes.push(Node { id, label, node_type: t, mod_id, missing, detail: vec![] });
        }
    };
    // Keys involved in error/warning findings mark edges as conflicts.
    let conflict_keys: BTreeSet<(Kind, String)> = findings
        .iter()
        .filter(|f| f.severity != Severity::Info && f.kind != Kind::Requires && f.kind != Kind::Resource)
        .map(|f| (f.kind, f.key.clone()))
        .collect();
    let record_users: HashMap<String, BTreeSet<i64>> = {
        let mut m: HashMap<String, BTreeSet<i64>> = HashMap::new();
        for x in mods {
            for t in x.touches.iter().filter(|t| t.kind == Kind::TweakProperty || t.kind == Kind::TweakRecord) {
                let rec = if t.kind == Kind::TweakRecord { t.key.clone() } else { t.key.rsplit_once('.').map(|(r, _)| r.to_string()).unwrap_or_default() };
                m.entry(rec).or_default().insert(x.mod_id);
            }
        }
        m
    };
    let mut resource_owners: BTreeMap<String, BTreeSet<i64>> = BTreeMap::new();

    for m in mods {
        let mid = format!("mod:{}", m.mod_id);
        add_node(&mut g, mid.clone(), m.name.to_string(), NodeType::Mod, Some(m.mod_id), false);
        // (target id) -> (label, detail, conflict)
        let mut edges: BTreeMap<(String, String), (BTreeSet<String>, bool)> = BTreeMap::new();
        let mut solo_records: BTreeSet<String> = BTreeSet::new();
        let mut base_count = 0usize;
        for t in m.touches {
            let conflict = conflict_keys.contains(&(t.kind, t.key.clone()));
            let (target, label, detail) = match t.kind {
                Kind::Requires => {
                    let id = format!("fw:{}", t.key);
                    add_node(&mut g, id.clone(), framework_name(&t.key).to_string(), NodeType::Framework, None, !installed_frameworks.contains(&t.key));
                    (id, "requires", String::new())
                }
                Kind::RedsReplaceMethod | Kind::RedsWrapMethod | Kind::RedsAddMethod | Kind::RedsAddField | Kind::RedsReplaceGlobal | Kind::CetOverride | Kind::CetObserve => {
                    let (class, member) = t.key.rsplit_once('.').unwrap_or(("global", &t.key));
                    let id = format!("class:{class}");
                    add_node(&mut g, id.clone(), class.to_string(), NodeType::Class, None, false);
                    let label = match t.kind {
                        Kind::RedsReplaceMethod | Kind::RedsReplaceGlobal => "replaces",
                        Kind::RedsWrapMethod => "wraps",
                        Kind::RedsAddMethod | Kind::RedsAddField => "adds",
                        Kind::CetOverride => "overrides",
                        _ => "observes",
                    };
                    (id, label, member.to_string())
                }
                Kind::TweakRecord | Kind::TweakProperty => {
                    let rec = if t.kind == Kind::TweakRecord { t.key.clone() } else { t.key.rsplit_once('.').map(|(r, _)| r.to_string()).unwrap_or_default() };
                    if record_users.get(&rec).is_some_and(|u| u.len() > 1) {
                        let id = format!("rec:{rec}");
                        add_node(&mut g, id.clone(), rec.clone(), NodeType::Record, None, false);
                        let prop = if t.kind == Kind::TweakProperty { t.key.rsplit('.').next().unwrap_or("").to_string() } else { String::new() };
                        (id, "tweaks", prop)
                    } else {
                        solo_records.insert(rec);
                        continue;
                    }
                }
                Kind::XlPatch => {
                    let id = format!("rec:{}", t.key);
                    add_node(&mut g, id.clone(), t.key.rsplit('\\').next().unwrap_or(&t.key).to_string(), NodeType::Record, None, false);
                    (id, "patches", t.key.clone())
                }
                Kind::Resource => {
                    if u64::from_str_radix(&t.key, 16).is_ok_and(|h| base.contains(&h)) {
                        base_count += 1;
                    }
                    resource_owners.entry(t.key.clone()).or_default().insert(m.mod_id);
                    continue;
                }
                Kind::Red4extPlugin => continue,
            };
            let e = edges.entry((target, label.to_string())).or_default();
            if !detail.is_empty() {
                e.0.insert(detail);
            }
            e.1 |= conflict;
        }
        for ((to, label), (detail, conflict)) in edges {
            g.edges.push(Edge { from: mid.clone(), to, label, weight: detail.len().max(1), conflict, detail: detail.into_iter().collect() });
        }
        if !solo_records.is_empty() {
            let id = format!("recs:{}", m.mod_id);
            add_node(&mut g, id.clone(), format!("{} tweak records", solo_records.len()), NodeType::Records, None, false);
            let detail: Vec<String> = solo_records.into_iter().take(200).collect();
            g.edges.push(Edge { from: mid.clone(), to: id, label: "tweaks".into(), weight: detail.len(), conflict: false, detail });
        }
        if base_count > 0 {
            add_node(&mut g, "base".into(), "Base game".into(), NodeType::BaseGame, None, false);
            g.edges.push(Edge { from: mid.clone(), to: "base".into(), label: "overrides resources".into(), weight: base_count, conflict: false, detail: vec![format!("{base_count} base-game resources replaced")] });
        }
    }
    // Resources shared by the same set of mods become one node.
    let mut shared: BTreeMap<Vec<i64>, usize> = BTreeMap::new();
    for owners in resource_owners.values().filter(|o| o.len() > 1) {
        *shared.entry(owners.iter().copied().collect()).or_default() += 1;
    }
    for (i, (owners, count)) in shared.into_iter().enumerate() {
        let id = format!("res:{i}");
        add_node(&mut g, id.clone(), format!("{count} shared resources"), NodeType::SharedResources, None, false);
        for o in owners {
            g.edges.push(Edge { from: format!("mod:{o}"), to: id.clone(), label: "replaces".into(), weight: count, conflict: true, detail: vec![] });
        }
    }
    g
}

pub struct ModIndex<'a> {
    pub mod_id: i64,
    pub name: &'a str,
    /// Archive file names in load order matter for resources; ArchiveXL and
    /// the game load `archive/pc/mod` alphabetically.
    pub touches: &'a [Touch],
}

fn framework_name(id: &str) -> &str {
    match id {
        "cet" => "Cyber Engine Tweaks",
        "red4ext" => "RED4ext",
        "redscript" => "redscript",
        "archivexl" => "ArchiveXL",
        "tweakxl" => "TweakXL",
        "codeware" => "Codeware",
        "redmod" => "REDmod",
        other => other,
    }
}

/// Cross-reference all mods' touches.
/// `installed_frameworks` are framework ids present in the game folder.
pub fn analyze(mods: &[ModIndex], base: &BTreeSet<u64>, installed_frameworks: &BTreeSet<String>) -> Report {
    let names: HashMap<i64, &str> = mods.iter().map(|m| (m.mod_id, m.name)).collect();
    // (kind, key) -> [(mod, file)]
    let mut by_key: BTreeMap<(Kind, String), Vec<(i64, String)>> = BTreeMap::new();
    for m in mods {
        for t in m.touches {
            let entry = by_key.entry((t.kind, t.key.clone())).or_default();
            if !entry.iter().any(|(id, _)| *id == m.mod_id) {
                entry.push((m.mod_id, t.file.clone()));
            }
        }
    }

    let mut findings = Vec::new();
    // "A and B both" / "A, B and C all"
    let list = |ids: &[(i64, String)]| {
        let n: Vec<&str> = ids.iter().map(|(id, _)| names[id]).collect();
        match n.as_slice() {
            [] => String::new(),
            [one] => one.to_string(),
            [init @ .., last] => format!("{} and {last}", init.join(", ")),
        }
    };
    let all = |ids: &[(i64, String)]| if ids.len() == 2 { "both" } else { "all" };

    // Resources: group shared hashes per set of mods so a texture pack
    // overlapping another doesn't produce thousands of lines.
    let mut shared_resources: BTreeMap<Vec<i64>, (usize, Vec<String>)> = BTreeMap::new();
    for ((kind, key), owners) in &by_key {
        if owners.len() < 2 {
            continue;
        }
        let ids: Vec<i64> = owners.iter().map(|(id, _)| *id).collect();
        match kind {
            Kind::Resource => {
                let e = shared_resources.entry(ids).or_default();
                e.0 += 1;
                for (_, f) in owners {
                    if !e.1.contains(f) {
                        e.1.push(f.clone());
                    }
                }
            }
            Kind::RedsReplaceMethod => findings.push(Finding {
                severity: Severity::Error,
                kind: *kind,
                key: key.clone(),
                mod_ids: ids,
                message: format!("{} {} replace {key}; only one replacement can win.", list(owners), all(owners)),
            }),
            Kind::RedsAddField | Kind::RedsAddMethod => findings.push(Finding {
                severity: Severity::Error,
                kind: *kind,
                key: key.clone(),
                mod_ids: ids,
                message: format!("{} each add {key}; redscript will fail to compile with a duplicate definition.", list(owners)),
            }),
            Kind::RedsReplaceGlobal => findings.push(Finding {
                severity: Severity::Error,
                kind: *kind,
                key: key.clone(),
                mod_ids: ids,
                message: format!("{} {} replace the global function {key}.", list(owners), all(owners)),
            }),
            Kind::CetOverride => findings.push(Finding {
                severity: Severity::Warning,
                kind: *kind,
                key: key.clone(),
                mod_ids: ids,
                message: format!("{} {} Override {key} in CET; the last one loaded wins.", list(owners), all(owners)),
            }),
            Kind::TweakProperty => findings.push(Finding {
                severity: Severity::Warning,
                kind: *kind,
                key: key.clone(),
                mod_ids: ids,
                message: format!("{} {} set {key}; the tweak loaded last decides the value.", list(owners), all(owners)),
            }),
            Kind::XlPatch => findings.push(Finding {
                severity: Severity::Info,
                kind: *kind,
                key: key.clone(),
                mod_ids: ids,
                message: format!("{} {} patch {key} with ArchiveXL; patches stack but can clash.", list(owners), all(owners)),
            }),
            Kind::RedsWrapMethod | Kind::CetObserve => findings.push(Finding {
                severity: Severity::Info,
                kind: *kind,
                key: key.clone(),
                mod_ids: ids,
                message: format!("{} {} hook {key}; hooks chain, so this is usually fine.", list(owners), all(owners)),
            }),
            Kind::Red4extPlugin => findings.push(Finding {
                severity: Severity::Error,
                kind: *kind,
                key: key.clone(),
                mod_ids: ids,
                message: format!("{} each ship the RED4ext plugin {key}; keep only one copy.", list(owners)),
            }),
            Kind::TweakRecord | Kind::Requires => {}
        }
    }
    for (ids, (count, files)) in shared_resources {
        let owners: Vec<(i64, String)> = ids.iter().map(|i| (*i, String::new())).collect();
        // The game loads archive/pc/mod alphabetically; the first file
        // containing a resource wins.
        let mut sorted = files.clone();
        sorted.sort_by_key(|f| f.rsplit('/').next().unwrap_or(f).to_lowercase());
        let winner = sorted.first().map(|f| f.rsplit('/').next().unwrap_or(f).to_string()).unwrap_or_default();
        findings.push(Finding {
            severity: Severity::Warning,
            kind: Kind::Resource,
            key: format!("{count} resources"),
            mod_ids: ids,
            message: format!(
                "{} {} replace the same {count} game resource{}; {winner} loads first and wins.",
                list(&owners),
                all(&owners),
                if count == 1 { "" } else { "s" }
            ),
        });
    }

    // Missing frameworks and per-mod summaries.
    let mut summaries = Vec::new();
    for m in mods {
        let mut counts: BTreeMap<Kind, usize> = BTreeMap::new();
        let mut requires: BTreeSet<String> = BTreeSet::new();
        let mut base_overrides = 0;
        for t in m.touches {
            *counts.entry(t.kind).or_default() += 1;
            if t.kind == Kind::Requires {
                requires.insert(t.key.clone());
            }
            if t.kind == Kind::Resource && u64::from_str_radix(&t.key, 16).is_ok_and(|h| base.contains(&h)) {
                base_overrides += 1;
            }
        }
        // ArchiveXL, TweakXL and Codeware run on RED4ext.
        let mut needed = requires.clone();
        if ["archivexl", "tweakxl", "codeware"].iter().any(|f| needed.contains(*f)) {
            needed.insert("red4ext".into());
        }
        for req in &needed {
            if !installed_frameworks.contains(req) {
                findings.push(Finding {
                    severity: Severity::Error,
                    kind: Kind::Requires,
                    key: req.clone(),
                    mod_ids: vec![m.mod_id],
                    message: format!("{} needs {}, which isn't installed.", m.name, framework_name(req)),
                });
            }
        }
        summaries.push(ModSummary { mod_id: m.mod_id, name: m.name.to_string(), counts, requires: requires.into_iter().collect(), base_overrides });
    }

    findings.sort_by(|a, b| a.severity.cmp(&b.severity).then(a.key.cmp(&b.key)));
    let graph = build_graph(mods, base, installed_frameworks, &findings);
    Report { findings, mods: summaries, graph }
}

// ---------------------------------------------------------------------------
// Running it against the library

/// Scan an installed mod from its staged copy and store the result.
pub fn index_mod(db: &crate::db::Db, staging_root: &Path, mod_id: i64) -> Result<Vec<Touch>> {
    let files: Vec<(String, std::path::PathBuf)> = db
        .mod_files(mod_id)?
        .into_iter()
        .map(|f| {
            let p = staging_root.join(mod_id.to_string()).join(&f.staged_path);
            (f.rel_path, p)
        })
        .collect();
    let touches = scan_mod(&files);
    db.set_touches(mod_id, SCANNER_VERSION, &touches)?;
    Ok(touches)
}

/// Analyse every enabled mod in a game, indexing any that aren't yet.
pub fn report_for_game(db: &crate::db::Db, staging_root: &Path, game: &crate::db::GameRow) -> Result<Report> {
    let game_dir = Path::new(&game.path);
    let build = build_key(game);
    let base = match db.base_resources(game.id, &build)? {
        Some(b) => b,
        None => {
            let b = base_game_hashes(game_dir);
            db.set_base_resources(game.id, &build, &b)?;
            b
        }
    };
    let mut indexes = Vec::new();
    for m in db.mods(game.id)?.into_iter().filter(|m| m.enabled()) {
        let touches = if db.index_version(m.id)? == Some(SCANNER_VERSION) { db.touches(m.id)? } else { index_mod(db, staging_root, m.id)? };
        indexes.push((m.id, m.name, touches));
    }
    Ok(build_report(game_dir, &base, &indexes))
}

/// The same report from what's already stored, without writing anything.
/// Returns the names of mods that haven't been indexed yet.
pub fn report_from_index(db: &crate::db::Db, game: &crate::db::GameRow) -> Result<(Report, Vec<String>)> {
    let base = db.base_resources(game.id, &build_key(game))?.unwrap_or_default();
    let mut indexes = Vec::new();
    let mut unindexed = Vec::new();
    for m in db.mods(game.id)?.into_iter().filter(|m| m.enabled()) {
        if db.index_version(m.id)?.is_none() {
            unindexed.push(m.name.clone());
        }
        indexes.push((m.id, m.name.clone(), db.touches(m.id)?));
    }
    Ok((build_report(Path::new(&game.path), &base, &indexes), unindexed))
}

fn build_key(game: &crate::db::GameRow) -> String {
    game.build_id.clone().or(game.exe_file_version.clone()).unwrap_or_else(|| "unknown".into())
}

fn build_report(game_dir: &Path, base: &BTreeSet<u64>, indexes: &[(i64, String, Vec<Touch>)]) -> Report {
    let frameworks: BTreeSet<String> =
        crate::game::detect_frameworks(game_dir).into_iter().filter(|f| f.installed).map(|f| f.id).collect();
    let views: Vec<ModIndex> = indexes.iter().map(|(id, name, t)| ModIndex { mod_id: *id, name, touches: t }).collect();
    analyze(&views, base, &frameworks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_names_round_trip() {
        for k in Kind::ALL {
            assert_eq!(Kind::parse(k.as_str()), Some(k));
            assert_eq!(serde_json::to_value(k).unwrap(), k.as_str());
        }
    }

    #[test]
    fn fnv_matches_known_value() {
        // FNV-1a 64 test vector.
        assert_eq!(fnv1a64("a"), 0xaf63dc4c8601ec8c);
        assert_eq!(depot_hash("Base/Characters/x.mesh"), fnv1a64("base\\characters\\x.mesh"));
    }

    #[test]
    fn reads_rdar_index() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("t.archive");
        let hashes = [depot_hash("base\\a.mesh"), depot_hash("base\\b.xbm")];
        let index_pos = 40u64;
        let index_size = 28 + 56 * hashes.len() as u32;
        let mut buf = Vec::new();
        buf.extend(b"RDAR");
        buf.extend(12u32.to_le_bytes());
        buf.extend(index_pos.to_le_bytes());
        buf.extend(index_size.to_le_bytes());
        buf.extend(0u64.to_le_bytes());
        buf.extend(0u32.to_le_bytes());
        buf.extend(0u64.to_le_bytes()); // filesize (unused)
        assert_eq!(buf.len(), 40);
        buf.extend(8u32.to_le_bytes());
        buf.extend((56 * hashes.len() as u32).to_le_bytes());
        buf.extend(0u64.to_le_bytes());
        buf.extend((hashes.len() as u32).to_le_bytes());
        buf.extend(0u32.to_le_bytes());
        buf.extend(0u32.to_le_bytes());
        for h in hashes {
            buf.extend(h.to_le_bytes());
            buf.extend([0u8; 48]);
        }
        std::fs::write(&p, &buf).unwrap();
        assert_eq!(archive_hashes(&p).unwrap(), hashes.to_vec());

        std::fs::write(&p, b"PK\x03\x04 not an archive at all, padding padding padding").unwrap();
        assert!(archive_hashes(&p).is_err());
    }

    #[test]
    fn scans_redscript() {
        let src = r#"
module Foo
// @replaceMethod(Commented) func Nope() {}
@replaceMethod(PlayerPuppet)
protected cb func OnAction(action: ListenerAction) -> Bool { return true; }
@wrapMethod(HUDManager) public func Update() { wrappedMethod(); }
@addField(PlayerPuppet) public let myFlag: Bool;
"#;
        let v = scan_redscript(src);
        assert_eq!(
            v,
            vec![
                (Kind::RedsReplaceMethod, "PlayerPuppet.OnAction".into()),
                (Kind::RedsWrapMethod, "HUDManager.Update".into()),
                (Kind::RedsAddField, "PlayerPuppet.myFlag".into()),
            ]
        );
    }

    #[test]
    fn scans_cet_and_tweaks_and_xl() {
        let lua = "Override('PlayerPuppet', 'OnDeath', function() end)\nObserveAfter(\"HUDManager\", \"Update\", f)\n-- Override('X','Y')";
        assert_eq!(
            scan_cet_lua(lua),
            vec![(Kind::CetOverride, "PlayerPuppet.OnDeath".into()), (Kind::CetObserve, "HUDManager.Update".into())]
        );

        let yaml = "Items.Preset_Yukimura:\n  quality: Quality.Legendary\n  statModifiers:\n    - !append Foo\n# comment\nVehicle.v_sport:\n  $type: Vehicle\n  topSpeed: 300\n";
        let v = scan_tweak_yaml(yaml);
        assert!(v.contains(&(Kind::TweakProperty, "Items.Preset_Yukimura.quality".into())));
        assert!(v.contains(&(Kind::TweakProperty, "Items.Preset_Yukimura.statModifiers".into())));
        assert!(v.contains(&(Kind::TweakProperty, "Vehicle.v_sport.topSpeed".into())));
        assert!(!v.iter().any(|(_, k)| k.contains("$type")));

        let xl = "resource:\n  patch:\n    mod/patch.mesh:\n      - base/characters/a.mesh\n      - base\\characters\\b.mesh\n    mod/p2.mesh: [base/c.mesh]\nfactories:\n  - x.csv\n";
        assert_eq!(
            scan_xl(xl).into_iter().map(|(_, k)| k).collect::<Vec<_>>(),
            vec!["base\\characters\\a.mesh", "base\\characters\\b.mesh", "base\\c.mesh"]
        );
    }

    #[test]
    fn reports_conflicts_and_missing_frameworks() {
        let t = |kind, key: &str, file: &str| Touch { kind, key: key.into(), file: file.into() };
        let a = vec![
            t(Kind::RedsReplaceMethod, "PlayerPuppet.OnAction", "r6/scripts/a.reds"),
            t(Kind::Requires, "redscript", "r6/scripts/a.reds"),
            t(Kind::Resource, "00000000000000aa", "archive/pc/mod/zzz_a.archive"),
            t(Kind::Resource, "00000000000000bb", "archive/pc/mod/zzz_a.archive"),
        ];
        let b = vec![
            t(Kind::RedsReplaceMethod, "PlayerPuppet.OnAction", "r6/scripts/b.reds"),
            t(Kind::Requires, "tweakxl", "r6/tweaks/b.yaml"),
            t(Kind::Resource, "00000000000000aa", "archive/pc/mod/aaa_b.archive"),
            t(Kind::Resource, "00000000000000bb", "archive/pc/mod/aaa_b.archive"),
        ];
        let mods = [ModIndex { mod_id: 1, name: "A", touches: &a }, ModIndex { mod_id: 2, name: "B", touches: &b }];
        let base: BTreeSet<u64> = [0xaa].into_iter().collect();
        let fw: BTreeSet<String> = ["redscript".to_string()].into_iter().collect();
        let r = analyze(&mods, &base, &fw);
        let errs: Vec<&str> = r.findings.iter().filter(|f| f.severity == Severity::Error).map(|f| f.key.as_str()).collect();
        assert!(errs.contains(&"PlayerPuppet.OnAction"));
        assert!(errs.contains(&"tweakxl"), "TweakXL missing");
        assert!(errs.contains(&"red4ext"), "TweakXL implies RED4ext");
        let res = r.findings.iter().find(|f| f.kind == Kind::Resource).unwrap();
        assert!(res.message.contains("2 game resources") && res.message.contains("aaa_b.archive loads first"), "{}", res.message);
        assert_eq!(r.mods[0].base_overrides, 1);

        let g = &r.graph;
        let has = |id: &str| g.nodes.iter().any(|n| n.id == id);
        assert!(has("mod:1") && has("mod:2") && has("class:PlayerPuppet") && has("base") && has("res:0"));
        assert!(g.nodes.iter().find(|n| n.id == "fw:tweakxl").unwrap().missing);
        let replace = g.edges.iter().find(|e| e.from == "mod:1" && e.to == "class:PlayerPuppet").unwrap();
        assert!(replace.conflict && replace.label == "replaces" && replace.detail == vec!["OnAction".to_string()]);
        assert!(g.edges.iter().all(|e| has(&e.to) && has(&e.from)), "no dangling edges");
    }
}
