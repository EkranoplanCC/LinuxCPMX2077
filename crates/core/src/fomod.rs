//! FOMOD installers (`fomod/ModuleConfig.xml`): mods that ask the user to pick
//! options and install different files depending on the answers.
//!
//! Implements the parts of the FOMOD 5.x schema that real installers use:
//! install steps with visibility conditions, option groups with their
//! selection rules, plugin types (static or condition-dependent), condition
//! flags, required and conditional file installs, file priorities, and the
//! `order` attribute. Paths in the XML come from the archive, so every
//! destination is validated like any other archive entry.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::archive::sanitize_entry_name;
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PluginType {
    Required,
    Recommended,
    Optional,
    CouldBeUsable,
    NotUsable,
}

impl PluginType {
    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "required" => Self::Required,
            "recommended" => Self::Recommended,
            "notusable" => Self::NotUsable,
            "couldbeusable" => Self::CouldBeUsable,
            _ => Self::Optional,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GroupKind {
    SelectExactlyOne,
    SelectAtMostOne,
    SelectAtLeastOne,
    SelectAll,
    SelectAny,
}

impl GroupKind {
    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "selectexactlyone" => Self::SelectExactlyOne,
            "selectatmostone" => Self::SelectAtMostOne,
            "selectatleastone" => Self::SelectAtLeastOne,
            "selectall" => Self::SelectAll,
            _ => Self::SelectAny,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileState {
    Missing,
    Inactive,
    Active,
}

#[derive(Debug, Clone)]
pub enum Dependency {
    All(Vec<Dependency>),
    Any(Vec<Dependency>),
    Flag { name: String, value: String },
    File { path: String, state: FileState },
    /// gameDependency / fommDependency and friends: we can't meaningfully
    /// check mod-manager versions, so they pass.
    Always,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileEntry {
    pub source: String,
    pub destination: String,
    pub is_folder: bool,
    pub priority: i64,
    #[serde(skip)]
    pub always_install: bool,
    #[serde(skip)]
    pub install_if_usable: bool,
}

#[derive(Debug, Clone)]
pub enum TypeDescriptor {
    Static(PluginType),
    Dependent { default: PluginType, patterns: Vec<(Dependency, PluginType)> },
}

#[derive(Debug, Clone, Serialize)]
pub struct Plugin {
    pub name: String,
    pub description: String,
    /// Image path relative to the installer root.
    pub image: Option<String>,
    #[serde(skip)]
    pub files: Vec<FileEntry>,
    #[serde(skip)]
    pub flags: Vec<(String, String)>,
    #[serde(skip)]
    pub type_desc: TypeDescriptor,
}

#[derive(Debug, Clone, Serialize)]
pub struct Group {
    pub name: String,
    pub kind: GroupKind,
    pub plugins: Vec<Plugin>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Step {
    pub name: String,
    pub groups: Vec<Group>,
    #[serde(skip)]
    pub visible: Option<Dependency>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Installer {
    pub module_name: String,
    pub module_image: Option<String>,
    pub steps: Vec<Step>,
    #[serde(skip)]
    pub module_dependencies: Option<Dependency>,
    #[serde(skip)]
    pub required_files: Vec<FileEntry>,
    #[serde(skip)]
    pub conditional: Vec<(Dependency, Vec<FileEntry>)>,
}

/// `[step][group]` → indices of the chosen plugins.
pub type Selections = Vec<Vec<Vec<usize>>>;

/// What the options wizard needs after every change.
#[derive(Debug, Clone, Serialize)]
pub struct Evaluation {
    pub visible: Vec<bool>,
    pub plugin_types: Vec<Vec<Vec<PluginType>>>,
}

/// Lets conditions ask about files in the game directory.
pub trait GameFiles {
    fn state(&self, rel_path: &str) -> FileState;
}

impl<F: Fn(&str) -> FileState> GameFiles for F {
    fn state(&self, rel_path: &str) -> FileState {
        self(rel_path)
    }
}

// ---------------------------------------------------------------------------
// Parsing

/// ModuleConfig.xml is frequently UTF-16 with a BOM.
pub fn decode_xml(bytes: &[u8]) -> Result<String> {
    let text = if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let units: Vec<u16> = rest.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&units)
    } else if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        let units: Vec<u16> = rest.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&units)
    } else {
        let rest = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
        String::from_utf8_lossy(rest).into_owned()
    };
    // roxmltree checks the declared encoding against nothing, but some files
    // declare UTF-16 after we've already decoded them; drop the declaration.
    let trimmed = text.trim_start();
    if trimmed.starts_with("<?xml")
        && let Some(end) = trimmed.find("?>") {
            return Ok(trimmed[end + 2..].to_string());
        }
    Ok(trimmed.to_string())
}

type Node<'a, 'i> = roxmltree::Node<'a, 'i>;

fn is(n: &Node, name: &str) -> bool {
    n.is_element() && n.tag_name().name().eq_ignore_ascii_case(name)
}

fn child<'a, 'i>(n: &Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    n.children().find(|c| is(c, name))
}

fn children<'a, 'i>(n: &Node<'a, 'i>, name: &'a str) -> impl Iterator<Item = Node<'a, 'i>> + 'a {
    n.children().filter(move |c| is(c, name))
}

fn attr(n: &Node, name: &str) -> Option<String> {
    n.attributes().find(|a| a.name().eq_ignore_ascii_case(name)).map(|a| a.value().to_string())
}

fn text(n: &Node) -> String {
    n.text().map(|t| t.trim().to_string()).unwrap_or_default()
}

fn norm_path(p: &str) -> String {
    p.replace('\\', "/").trim_matches('/').to_string()
}

fn parse_composite(n: &Node) -> Dependency {
    let items: Vec<Dependency> = n.children().filter(|c| c.is_element()).filter_map(|c| parse_dep(&c)).collect();
    if attr(n, "operator").is_some_and(|o| o.eq_ignore_ascii_case("or")) {
        Dependency::Any(items)
    } else {
        Dependency::All(items)
    }
}

fn parse_dep(n: &Node) -> Option<Dependency> {
    let name = n.tag_name().name().to_ascii_lowercase();
    Some(match name.as_str() {
        "flagdependency" => Dependency::Flag {
            name: attr(n, "flag").unwrap_or_default(),
            value: attr(n, "value").unwrap_or_default(),
        },
        "filedependency" => Dependency::File {
            path: norm_path(&attr(n, "file").unwrap_or_default()),
            state: match attr(n, "state").unwrap_or_default().to_ascii_lowercase().as_str() {
                "missing" => FileState::Missing,
                "inactive" => FileState::Inactive,
                _ => FileState::Active,
            },
        },
        "dependencies" => parse_composite(n),
        n if n.ends_with("dependency") => Dependency::Always,
        _ => return None,
    })
}

fn parse_files(n: Option<Node>) -> Vec<FileEntry> {
    let Some(n) = n else { return vec![] };
    n.children()
        .filter(|c| is(c, "file") || is(c, "folder"))
        .map(|c| {
            let source = norm_path(&attr(&c, "source").unwrap_or_default());
            // A missing destination means "same as source"; an empty one
            // means the game root.
            let destination = match attr(&c, "destination") {
                Some(d) => norm_path(&d),
                None => source.clone(),
            };
            let flag = |a: &str| attr(&c, a).is_some_and(|v| v.eq_ignore_ascii_case("true"));
            FileEntry {
                source,
                destination,
                is_folder: is(&c, "folder"),
                priority: attr(&c, "priority").and_then(|p| p.trim().parse().ok()).unwrap_or(0),
                always_install: flag("alwaysInstall"),
                install_if_usable: flag("installIfUsable"),
            }
        })
        .collect()
}

fn sort_by_order<T>(items: &mut [T], order: Option<String>, name: impl Fn(&T) -> String) {
    match order.as_deref().map(str::to_ascii_lowercase).as_deref() {
        Some("explicit") => {}
        Some("descending") => items.sort_by_key(|b| std::cmp::Reverse(name(b).to_lowercase())),
        _ => items.sort_by_key(|a| name(a).to_lowercase()),
    }
}

fn parse_type(n: Option<Node>) -> TypeDescriptor {
    let Some(td) = n else { return TypeDescriptor::Static(PluginType::Optional) };
    if let Some(t) = child(&td, "type") {
        return TypeDescriptor::Static(PluginType::parse(&attr(&t, "name").unwrap_or_default()));
    }
    if let Some(dt) = child(&td, "dependencyType") {
        let default = child(&dt, "defaultType")
            .map(|d| PluginType::parse(&attr(&d, "name").unwrap_or_default()))
            .unwrap_or(PluginType::Optional);
        let patterns = child(&dt, "patterns")
            .map(|ps| {
                children(&ps, "pattern")
                    .filter_map(|p| {
                        let deps = child(&p, "dependencies").map(|d| parse_composite(&d))?;
                        let t = child(&p, "type").map(|t| PluginType::parse(&attr(&t, "name").unwrap_or_default()))?;
                        Some((deps, t))
                    })
                    .collect()
            })
            .unwrap_or_default();
        return TypeDescriptor::Dependent { default, patterns };
    }
    TypeDescriptor::Static(PluginType::Optional)
}

pub fn parse(xml: &str) -> Result<Installer> {
    let doc = roxmltree::Document::parse_with_options(xml, roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() })
        .map_err(|e| Error::Other(format!("invalid FOMOD ModuleConfig.xml: {e}")))?;
    let root = doc.root_element();
    if !is(&root, "config") {
        return Err(Error::Other("ModuleConfig.xml has no <config> root".into()));
    }
    let module_name = child(&root, "moduleName").map(|n| text(&n)).unwrap_or_default();
    let module_image = child(&root, "moduleImage").and_then(|n| attr(&n, "path")).map(|p| norm_path(&p));
    let module_dependencies = child(&root, "moduleDependencies").map(|n| parse_composite(&n));
    let required_files = parse_files(child(&root, "requiredInstallFiles"));

    let mut steps = Vec::new();
    if let Some(is_node) = child(&root, "installSteps") {
        for s in children(&is_node, "installStep") {
            let mut groups = Vec::new();
            if let Some(gs) = child(&s, "optionalFileGroups") {
                for g in children(&gs, "group") {
                    let mut plugins = Vec::new();
                    if let Some(ps) = child(&g, "plugins") {
                        for p in children(&ps, "plugin") {
                            plugins.push(Plugin {
                                name: attr(&p, "name").unwrap_or_default(),
                                description: child(&p, "description").map(|d| text(&d)).unwrap_or_default(),
                                image: child(&p, "image").and_then(|i| attr(&i, "path")).map(|p| norm_path(&p)),
                                files: parse_files(child(&p, "files")),
                                flags: child(&p, "conditionFlags")
                                    .map(|cf| {
                                        children(&cf, "flag")
                                            .map(|f| (attr(&f, "name").unwrap_or_default(), text(&f)))
                                            .collect()
                                    })
                                    .unwrap_or_default(),
                                type_desc: parse_type(child(&p, "typeDescriptor")),
                            });
                        }
                        sort_by_order(&mut plugins, attr(&ps, "order"), |p| p.name.clone());
                    }
                    groups.push(Group {
                        name: attr(&g, "name").unwrap_or_default(),
                        kind: GroupKind::parse(&attr(&g, "type").unwrap_or_default()),
                        plugins,
                    });
                }
                sort_by_order(&mut groups, attr(&gs, "order"), |g| g.name.clone());
            }
            steps.push(Step {
                name: attr(&s, "name").unwrap_or_default(),
                groups,
                visible: child(&s, "visible").map(|v| parse_composite(&v)),
            });
        }
        sort_by_order(&mut steps, attr(&is_node, "order"), |s| s.name.clone());
    }

    let mut conditional = Vec::new();
    if let Some(cfi) = child(&root, "conditionalFileInstalls")
        && let Some(ps) = child(&cfi, "patterns") {
            for p in children(&ps, "pattern") {
                if let Some(d) = child(&p, "dependencies") {
                    conditional.push((parse_composite(&d), parse_files(child(&p, "files"))));
                }
            }
        }

    Ok(Installer { module_name, module_image, steps, module_dependencies, required_files, conditional })
}

// ---------------------------------------------------------------------------
// Evaluation

impl Dependency {
    pub fn eval(&self, flags: &HashMap<String, String>, files: &dyn GameFiles) -> bool {
        match self {
            Dependency::All(v) => v.iter().all(|d| d.eval(flags, files)),
            Dependency::Any(v) => v.is_empty() || v.iter().any(|d| d.eval(flags, files)),
            // An unset flag compares equal to "".
            Dependency::Flag { name, value } => flags.get(name).map(String::as_str).unwrap_or("") == value,
            Dependency::File { path, state } => files.state(path) == *state,
            Dependency::Always => true,
        }
    }
}

impl Plugin {
    pub fn plugin_type(&self, flags: &HashMap<String, String>, files: &dyn GameFiles) -> PluginType {
        match &self.type_desc {
            TypeDescriptor::Static(t) => *t,
            TypeDescriptor::Dependent { default, patterns } => patterns
                .iter()
                .find(|(d, _)| d.eval(flags, files))
                .map(|(_, t)| *t)
                .unwrap_or(*default),
        }
    }
}

fn selected(sel: &Selections, s: usize, g: usize) -> &[usize] {
    sel.get(s).and_then(|x| x.get(g)).map(Vec::as_slice).unwrap_or(&[])
}

impl Installer {
    /// Walk the steps in order, applying the flags of chosen plugins, to work
    /// out which steps are shown and what type each plugin currently has.
    pub fn evaluate(&self, sel: &Selections, files: &dyn GameFiles) -> (Evaluation, HashMap<String, String>) {
        let mut flags = HashMap::new();
        let mut visible = Vec::new();
        let mut types = Vec::new();
        for (si, step) in self.steps.iter().enumerate() {
            let vis = step.visible.as_ref().is_none_or(|d| d.eval(&flags, files));
            visible.push(vis);
            let step_types: Vec<Vec<PluginType>> = step
                .groups
                .iter()
                .map(|g| g.plugins.iter().map(|p| p.plugin_type(&flags, files)).collect())
                .collect();
            if vis {
                for (gi, g) in step.groups.iter().enumerate() {
                    for &pi in selected(sel, si, gi) {
                        if let Some(p) = g.plugins.get(pi) {
                            for (k, v) in &p.flags {
                                flags.insert(k.clone(), v.clone());
                            }
                        }
                    }
                }
            }
            types.push(step_types);
        }
        (Evaluation { visible, plugin_types: types }, flags)
    }

    /// Reasonable starting choices: required and recommended options, and
    /// the first usable option where exactly one or at least one is needed.
    pub fn default_selections(&self, files: &dyn GameFiles) -> Selections {
        let mut sel: Selections = self.steps.iter().map(|s| vec![vec![]; s.groups.len()]).collect();
        // Defaults for later steps can depend on flags from earlier ones.
        for si in 0..self.steps.len() {
            let (ev, _) = self.evaluate(&sel, files);
            for (gi, g) in self.steps[si].groups.iter().enumerate() {
                let types = &ev.plugin_types[si][gi];
                let usable: Vec<usize> = (0..g.plugins.len()).filter(|&i| types[i] != PluginType::NotUsable).collect();
                let mut pick: Vec<usize> = match g.kind {
                    GroupKind::SelectAll => (0..g.plugins.len()).collect(),
                    _ => usable
                        .iter()
                        .copied()
                        .filter(|&i| matches!(types[i], PluginType::Required | PluginType::Recommended))
                        .collect(),
                };
                if matches!(g.kind, GroupKind::SelectExactlyOne | GroupKind::SelectAtMostOne) && pick.len() > 1 {
                    pick.truncate(1);
                }
                if pick.is_empty() && matches!(g.kind, GroupKind::SelectExactlyOne | GroupKind::SelectAtLeastOne) {
                    pick.extend(usable.first());
                }
                sel[si][gi] = pick;
            }
        }
        sel
    }

    /// Check the choices against each group's rules.
    pub fn validate(&self, sel: &Selections, files: &dyn GameFiles) -> Result<()> {
        let (ev, _) = self.evaluate(sel, files);
        for (si, step) in self.steps.iter().enumerate() {
            if !ev.visible[si] {
                continue;
            }
            for (gi, g) in step.groups.iter().enumerate() {
                let chosen = selected(sel, si, gi);
                let label = || format!("“{}” in step “{}”", g.name, step.name);
                if chosen.iter().any(|&i| i >= g.plugins.len()) {
                    return Err(Error::Other(format!("invalid option index for {}", label())));
                }
                let types = &ev.plugin_types[si][gi];
                if chosen.iter().any(|&i| types[i] == PluginType::NotUsable) {
                    return Err(Error::Other(format!("an option in {} can't be used", label())));
                }
                if let Some(i) = (0..g.plugins.len()).find(|&i| types[i] == PluginType::Required && !chosen.contains(&i)) {
                    return Err(Error::Other(format!("“{}” is required in {}", g.plugins[i].name, label())));
                }
                let n = chosen.len();
                let ok = match g.kind {
                    GroupKind::SelectExactlyOne => n == 1,
                    GroupKind::SelectAtMostOne => n <= 1,
                    GroupKind::SelectAtLeastOne => n >= 1,
                    GroupKind::SelectAll => n == g.plugins.len(),
                    GroupKind::SelectAny => true,
                };
                if !ok {
                    return Err(Error::Other(format!("{} needs {:?}", label(), g.kind)));
                }
            }
        }
        Ok(())
    }

    /// Turn choices into `(source in archive, destination in game)` pairs.
    /// `archive_files` are the extracted files relative to the installer
    /// root (the folder that contains `fomod/`).
    pub fn resolve(&self, sel: &Selections, files: &dyn GameFiles, archive_files: &[String]) -> Result<Resolved> {
        if let Some(d) = &self.module_dependencies
            && !d.eval(&HashMap::new(), files) {
                return Err(Error::Other(format!(
                    "“{}” requires other mods or files that aren't installed",
                    self.module_name
                )));
            }
        self.validate(sel, files)?;
        let (ev, flags) = self.evaluate(sel, files);

        // (entry, order) so equal priorities keep document order.
        let mut entries: Vec<&FileEntry> = self.required_files.iter().collect();
        for (si, step) in self.steps.iter().enumerate() {
            for (gi, g) in step.groups.iter().enumerate() {
                let chosen = if ev.visible[si] { selected(sel, si, gi) } else { &[] };
                for (pi, p) in g.plugins.iter().enumerate() {
                    let usable = ev.plugin_types[si][gi][pi] != PluginType::NotUsable;
                    for f in &p.files {
                        if chosen.contains(&pi) || f.always_install || (f.install_if_usable && usable) {
                            entries.push(f);
                        }
                    }
                }
            }
        }
        for (dep, fs) in &self.conditional {
            if dep.eval(&flags, files) {
                entries.extend(fs.iter());
            }
        }
        let mut ordered: Vec<(usize, &FileEntry)> = entries.into_iter().enumerate().collect();
        ordered.sort_by_key(|(i, f)| (f.priority, *i));

        let by_lower: HashMap<String, &String> = archive_files.iter().map(|f| (f.to_lowercase(), f)).collect();
        let mut out: HashMap<String, (String, String)> = HashMap::new();
        let mut missing = Vec::new();
        for (_, f) in ordered {
            let mut matched = false;
            if f.is_folder {
                let prefix = if f.source.is_empty() { String::new() } else { format!("{}/", f.source.to_lowercase()) };
                for (lower, orig) in &by_lower {
                    if let Some(rest) = lower.strip_prefix(&prefix) {
                        // Keep the original case of the part below the folder.
                        let rest_orig = if orig.len() == lower.len() { &orig[orig.len() - rest.len()..] } else { rest };
                        let dest = if f.destination.is_empty() { rest_orig.to_string() } else { format!("{}/{}", f.destination, rest_orig) };
                        insert_target(&mut out, orig, &dest)?;
                        matched = true;
                    }
                }
            } else if let Some(orig) = by_lower.get(&f.source.to_lowercase()) {
                let dest = if f.destination.is_empty() {
                    f.source.rsplit('/').next().unwrap_or(&f.source).to_string()
                } else {
                    f.destination.clone()
                };
                insert_target(&mut out, orig, &dest)?;
                matched = true;
            }
            if !matched {
                missing.push(f.source.clone());
            }
        }
        let mut files: Vec<(String, String)> = out.into_values().collect();
        files.sort_by(|a, b| a.1.cmp(&b.1));
        Ok(Resolved { files, missing_sources: missing })
    }
}

fn insert_target(out: &mut HashMap<String, (String, String)>, source: &str, dest: &str) -> Result<()> {
    let safe = sanitize_entry_name(dest)?
        .ok_or_else(|| Error::UnsafeArchive(format!("FOMOD destination {dest:?} is empty")))?;
    let dest = safe.to_string_lossy().replace('\\', "/");
    // Later (higher-priority) entries win; matching ignores case.
    out.insert(dest.to_lowercase(), (source.to_string(), dest));
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct Resolved {
    /// `(source relative to installer root, destination in game dir)`
    pub files: Vec<(String, String)>,
    /// Sources the XML names that aren't in the archive.
    pub missing_sources: Vec<String>,
}

/// Find `fomod/ModuleConfig.xml` among extracted files; returns the path of
/// the XML and the installer root prefix (`""` or `"Some Folder/"`).
pub fn find_config(files: &[String]) -> Option<(String, String)> {
    files
        .iter()
        .filter_map(|f| {
            let lower = f.to_lowercase();
            if lower == "fomod/moduleconfig.xml" {
                Some((f.clone(), String::new()))
            } else {
                lower
                    .strip_suffix("/fomod/moduleconfig.xml")
                    .map(|p| (f.clone(), format!("{}/", &f[..p.len()])))
            }
        })
        .min_by_key(|(_, root)| root.matches('/').count())
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<?xml version="1.0" encoding="UTF-16"?>
<config xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <moduleName>Test Mod</moduleName>
  <requiredInstallFiles>
    <folder source="Core" destination="" />
  </requiredInstallFiles>
  <installSteps order="Explicit">
    <installStep name="Main">
      <optionalFileGroups order="Explicit">
        <group name="Resolution" type="SelectExactlyOne">
          <plugins order="Explicit">
            <plugin name="1080p">
              <description>Full HD</description>
              <image path="images\1080.png" />
              <files><folder source="1080" destination="archive\pc\mod" priority="0" /></files>
              <conditionFlags><flag name="res">1080</flag></conditionFlags>
              <typeDescriptor><type name="Optional"/></typeDescriptor>
            </plugin>
            <plugin name="4K">
              <description>Ultra</description>
              <files><folder source="4k" destination="archive\pc\mod" priority="1" /></files>
              <conditionFlags><flag name="res">4k</flag></conditionFlags>
              <typeDescriptor><type name="Recommended"/></typeDescriptor>
            </plugin>
          </plugins>
        </group>
      </optionalFileGroups>
    </installStep>
    <installStep name="4K extras">
      <visible><flagDependency flag="res" value="4k"/></visible>
      <optionalFileGroups>
        <group name="Extras" type="SelectAny">
          <plugins>
            <plugin name="Needs CET">
              <description>CET addon</description>
              <files><file source="extras\addon.lua" destination="bin\x64\plugins\cyber_engine_tweaks\mods\t\init.lua"/></files>
              <typeDescriptor>
                <dependencyType>
                  <defaultType name="NotUsable"/>
                  <patterns>
                    <pattern>
                      <dependencies><fileDependency file="bin\x64\plugins\cyber_engine_tweaks.asi" state="Active"/></dependencies>
                      <type name="Optional"/>
                    </pattern>
                  </patterns>
                </dependencyType>
              </typeDescriptor>
            </plugin>
          </plugins>
        </group>
      </optionalFileGroups>
    </installStep>
  </installSteps>
  <conditionalFileInstalls>
    <patterns>
      <pattern>
        <dependencies operator="And"><flagDependency flag="res" value="4k"/></dependencies>
        <files><file source="patch\4k.archive" destination="archive\pc\mod\zz_4k_patch.archive"/></files>
      </pattern>
    </patterns>
  </conditionalFileInstalls>
</config>"#;

    fn utf16(s: &str) -> Vec<u8> {
        let mut v = vec![0xFF, 0xFE];
        for u in s.encode_utf16() {
            v.extend(u.to_le_bytes());
        }
        v
    }

    fn archive() -> Vec<String> {
        ["Core/r6/scripts/t/core.reds", "1080/tex.archive", "4K/tex.archive", "extras/addon.lua", "patch/4k.archive", "fomod/ModuleConfig.xml"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn parses_utf16_and_resolves_choices() {
        let inst = parse(&decode_xml(&utf16(XML)).unwrap()).unwrap();
        assert_eq!(inst.module_name, "Test Mod");
        assert_eq!(inst.steps.len(), 2);
        assert_eq!(inst.steps[0].groups[0].plugins[0].image.as_deref(), Some("images/1080.png"));

        let no_cet = |_: &str| FileState::Missing;
        let with_cet = |p: &str| if p.eq_ignore_ascii_case("bin/x64/plugins/cyber_engine_tweaks.asi") { FileState::Active } else { FileState::Missing };

        // Defaults pick the recommended 4K option, which reveals step 2.
        let sel = inst.default_selections(&no_cet);
        assert_eq!(sel[0][0], vec![1]);
        let (ev, _) = inst.evaluate(&sel, &no_cet);
        assert_eq!(ev.visible, vec![true, true]);
        assert_eq!(ev.plugin_types[1][0][0], PluginType::NotUsable);
        let (ev, _) = inst.evaluate(&sel, &with_cet);
        assert_eq!(ev.plugin_types[1][0][0], PluginType::Optional);

        let r = inst.resolve(&sel, &no_cet, &archive()).unwrap();
        let dests: Vec<&str> = r.files.iter().map(|(_, d)| d.as_str()).collect();
        assert_eq!(dests, vec!["archive/pc/mod/tex.archive", "archive/pc/mod/zz_4k_patch.archive", "r6/scripts/t/core.reds"]);
        assert_eq!(r.files[0].0, "4K/tex.archive", "case-insensitive folder match keeps real path");

        // Choosing 1080p hides step 2 and drops the conditional patch.
        let sel = vec![vec![vec![0]], vec![vec![0]]];
        let (ev, _) = inst.evaluate(&sel, &with_cet);
        assert_eq!(ev.visible, vec![true, false]);
        let r = inst.resolve(&sel, &with_cet, &archive()).unwrap();
        assert_eq!(r.files.len(), 2);
        assert_eq!(r.files[0].0, "1080/tex.archive");

        // Rules are enforced.
        assert!(inst.validate(&vec![vec![vec![0, 1]], vec![vec![]]], &no_cet).is_err());
        assert!(inst.validate(&vec![vec![vec![1]], vec![vec![0]]], &no_cet).is_err(), "NotUsable chosen");
    }

    #[test]
    fn rejects_traversal_in_destination() {
        let xml = r#"<config><moduleName>x</moduleName><requiredInstallFiles>
            <file source="a.txt" destination="..\..\.bashrc"/></requiredInstallFiles></config>"#;
        let inst = parse(xml).unwrap();
        let files = |_: &str| FileState::Missing;
        let err = inst.resolve(&vec![], &files, &["a.txt".to_string()]).unwrap_err();
        assert!(matches!(err, Error::UnsafeArchive(_)));
    }

    #[test]
    fn finds_config_in_wrapper_folder() {
        let files = vec!["My Mod/fomod/ModuleConfig.xml".to_string(), "My Mod/fomod/info.xml".to_string()];
        assert_eq!(find_config(&files), Some(("My Mod/fomod/ModuleConfig.xml".into(), "My Mod/".into())));
    }
}
