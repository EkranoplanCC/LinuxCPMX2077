//! The file tree in the Netrunner tab: every file the manager installed, laid
//! out like the game folder (or grouped by mod), with the mod that owns each
//! file and whether it is really there. Built from the install tracking in
//! `mod_files`, so it shows only what mods put in, never base-game files.

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::db::{Db, GameRow, ModRow};
use crate::install::resolve_ci;
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Group {
    /// Folders as they are in the game directory.
    Location,
    /// One branch per mod, each with its own folders.
    Mod,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileState {
    /// In the game folder and this mod's copy is the one in use.
    Active,
    /// A newer enabled mod ships the same path and replaced this copy.
    Overridden,
    /// The mod is switched off; its files are kept outside the game.
    Disabled,
    /// The mod is enabled but the file is gone from the game folder.
    Missing,
}

#[derive(Debug, Clone, Serialize)]
pub struct TreeFile {
    pub name: String,
    /// Path inside the game folder.
    pub path: String,
    pub mod_id: i64,
    pub size: i64,
    pub state: FileState,
    /// The mod whose copy is in use, when this one is overridden.
    pub overridden_by: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TreeNode {
    pub name: String,
    /// Folder path inside the game folder ("" for the root and mod branches).
    pub path: String,
    /// Set on a mod branch when grouping by mod.
    pub mod_id: Option<i64>,
    /// Files anywhere below this node.
    pub file_count: usize,
    pub size: i64,
    /// Mods with files anywhere below this node.
    pub mods: Vec<i64>,
    /// Missing or overridden files anywhere below this node.
    pub problems: usize,
    pub folders: Vec<TreeNode>,
    pub files: Vec<TreeFile>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TreeMod {
    pub id: i64,
    pub name: String,
    pub version: Option<String>,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileTree {
    pub game_path: String,
    pub mods: Vec<TreeMod>,
    pub root: TreeNode,
}

/// Build the tree for one game.
pub fn build(db: &Db, game: &GameRow, group: Group) -> Result<FileTree> {
    let mods = db.mods(game.id)?;
    let mut files = Vec::new();
    for m in &mods {
        for f in db.mod_files(m.id)? {
            files.push((m, f.rel_path, f.size));
        }
    }
    let game_dir = Path::new(&game.path);

    // The copy in use for each path is the newest enabled mod's, the same
    // rule the installer uses (`Db::owners_of`). Paths match case-insensitively.
    let mut winner: HashMap<String, &ModRow> = HashMap::new();
    for (m, path, _) in &files {
        if !m.enabled() {
            continue;
        }
        let w = winner.entry(path.to_lowercase()).or_insert(m);
        if (m.installed_at.as_str(), m.id) > (w.installed_at.as_str(), w.id) {
            *w = m;
        }
    }

    let entries: Vec<TreeFile> = files
        .iter()
        .map(|(m, path, size)| {
            let (state, overridden_by) = if !m.enabled() {
                (FileState::Disabled, None)
            } else {
                match winner.get(&path.to_lowercase()) {
                    Some(w) if w.id != m.id => (FileState::Overridden, Some(w.id)),
                    _ if !resolve_ci(game_dir, path).is_file() => (FileState::Missing, None),
                    _ => (FileState::Active, None),
                }
            };
            TreeFile {
                name: path.rsplit('/').next().unwrap_or(path).to_string(),
                path: path.clone(),
                mod_id: m.id,
                size: *size,
                state,
                overridden_by,
            }
        })
        .collect();

    let root = match group {
        Group::Location => assemble(String::new(), entries),
        Group::Mod => {
            let mut by_mod: BTreeMap<i64, Vec<TreeFile>> = BTreeMap::new();
            for e in entries {
                by_mod.entry(e.mod_id).or_default().push(e);
            }
            let mut folders: Vec<TreeNode> = mods
                .iter()
                .filter_map(|m| {
                    let mut n = assemble(m.name.clone(), by_mod.remove(&m.id)?);
                    n.mod_id = Some(m.id);
                    Some(n)
                })
                .collect();
            folders.sort_by_key(|n| n.name.to_lowercase());
            let mut root = TreeNode { folders, ..Default::default() };
            total(&mut root);
            root
        }
    };

    Ok(FileTree {
        game_path: game.path.clone(),
        mods: mods
            .iter()
            .map(|m| TreeMod { id: m.id, name: m.name.clone(), version: m.version.clone(), enabled: m.enabled() })
            .collect(),
        root,
    })
}

/// Lay files out in folders. Folder names merge case-insensitively, keeping
/// the first spelling seen, since the game treats paths that way.
fn assemble(name: String, files: Vec<TreeFile>) -> TreeNode {
    #[derive(Default)]
    struct Dir {
        name: String,
        path: String,
        dirs: BTreeMap<String, Dir>,
        files: Vec<TreeFile>,
    }
    fn into_node(d: Dir) -> TreeNode {
        let mut files = d.files;
        files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.mod_id.cmp(&b.mod_id)));
        TreeNode { name: d.name, path: d.path, folders: d.dirs.into_values().map(into_node).collect(), files, ..Default::default() }
    }

    let mut top = Dir { name, ..Default::default() };
    for f in files {
        let mut cur = &mut top;
        let parts: Vec<&str> = f.path.split('/').filter(|p| !p.is_empty()).collect();
        for part in &parts[..parts.len().saturating_sub(1)] {
            let path = if cur.path.is_empty() { part.to_string() } else { format!("{}/{part}", cur.path) };
            cur = cur.dirs.entry(part.to_lowercase()).or_insert_with(|| Dir { name: part.to_string(), path, ..Default::default() });
        }
        cur.files.push(f);
    }
    let mut node = into_node(top);
    total(&mut node);
    node
}

/// Fill in the counts of a node from everything below it.
fn total(n: &mut TreeNode) {
    let mut mods: Vec<i64> = n.files.iter().map(|f| f.mod_id).collect();
    n.file_count = n.files.len();
    n.size = n.files.iter().map(|f| f.size).sum();
    n.problems = n.files.iter().filter(|f| matches!(f.state, FileState::Missing | FileState::Overridden)).count();
    for c in &mut n.folders {
        total(c);
        n.file_count += c.file_count;
        n.size += c.size;
        n.problems += c.problems;
        mods.extend(&c.mods);
    }
    mods.sort_unstable();
    mods.dedup();
    n.mods = mods;
}

/// The folder to open for a path inside the game folder: the deepest part of
/// it that exists. Refuses anything that would leave the game folder.
pub fn folder_in_game(game_dir: &Path, rel: &str) -> Result<PathBuf> {
    if Path::new(rel).components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(Error::Other(format!("not a path inside the game folder: {rel}")));
    }
    let mut dir = resolve_ci(game_dir, rel.trim_matches('/'));
    while !dir.is_dir() {
        match dir.parent() {
            Some(p) if p.starts_with(game_dir) => dir = p.to_path_buf(),
            _ => return Err(Error::Other("the game folder is missing".into())),
        }
    }
    let (real, base) = (dir.canonicalize()?, game_dir.canonicalize()?);
    if !real.starts_with(&base) {
        return Err(Error::Other(format!("{rel} leads outside the game folder")));
    }
    Ok(real)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{ModFile, NewMod, STATUS_DISABLED};
    use crate::game::{GameInstall, Store};

    fn setup() -> (tempfile::TempDir, Db, GameRow) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_in_memory().unwrap();
        let gid = db
            .upsert_game(&GameInstall {
                path: dir.path().to_path_buf(),
                store: Store::Steam,
                proton_prefix: None,
                build_id: None,
                exe_file_version: None,
                exe_product_version: None,
                frameworks: vec![],
                launch_options: None,
                warnings: vec![],
            })
            .unwrap();
        let game = db.game(gid).unwrap();
        (dir, db, game)
    }

    fn add(db: &Db, game: &GameRow, name: &str, files: &[&str], on_disk: bool) -> i64 {
        let id = db.insert_mod(game, &NewMod { name: name.into(), source: "manual".into(), ..Default::default() }).unwrap();
        for f in files {
            db.add_mod_file(&ModFile { mod_id: id, rel_path: f.to_string(), staged_path: f.to_string(), sha256: String::new(), size: 10 })
                .unwrap();
            if on_disk {
                let p = Path::new(&game.path).join(f);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(p, "x").unwrap();
            }
        }
        id
    }

    fn find<'a>(n: &'a TreeNode, path: &str) -> &'a TreeNode {
        n.folders.iter().find(|c| c.path.eq_ignore_ascii_case(path)).unwrap_or_else(|| panic!("no folder {path}"))
    }

    #[test]
    fn lays_files_out_by_folder_with_owners_and_states() {
        let (_dir, db, game) = setup();
        let a = add(&db, &game, "Neon", &["archive/pc/mod/neon.archive", "r6/scripts/neon/a.reds"], true);
        // Same folder spelled differently, and a file that overrides Neon's.
        let b = add(&db, &game, "Glow", &["Archive/PC/mod/glow.archive", "archive/pc/mod/NEON.archive"], true);
        let c = add(&db, &game, "Off", &["archive/pc/mod/off.archive"], false);
        db.set_mod_status(c, STATUS_DISABLED).unwrap();
        let d = add(&db, &game, "Gone", &["bin/x64/gone.dll"], false);

        let t = build(&db, &game, Group::Location).unwrap();
        assert_eq!(t.root.file_count, 6);
        assert_eq!(t.root.size, 60);
        assert_eq!(t.root.mods, vec![a, b, c, d]);
        assert_eq!(t.root.problems, 2, "one overridden, one missing");
        let names: Vec<&str> = t.root.folders.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["archive", "bin", "r6"]);

        let modf = find(find(find(&t.root, "archive"), "archive/pc"), "archive/pc/mod");
        assert_eq!(modf.path, "archive/pc/mod", "first spelling kept");
        assert_eq!(modf.file_count, 4);
        let st = |name: &str, m: i64| modf.files.iter().find(|f| f.name.eq_ignore_ascii_case(name) && f.mod_id == m).unwrap();
        assert_eq!(st("neon.archive", a).state, FileState::Overridden);
        assert_eq!(st("neon.archive", a).overridden_by, Some(b));
        assert_eq!(st("neon.archive", b).state, FileState::Active);
        assert_eq!(st("glow.archive", b).state, FileState::Active);
        assert_eq!(st("off.archive", c).state, FileState::Disabled);
        let bin = find(find(&t.root, "bin"), "bin/x64");
        assert_eq!(bin.files[0].state, FileState::Missing);
    }

    #[test]
    fn groups_by_mod() {
        let (_dir, db, game) = setup();
        let z = add(&db, &game, "zeta", &["archive/pc/mod/z.archive"], true);
        add(&db, &game, "Alpha", &["r6/scripts/a.reds", "r6/tweaks/a.yaml"], true);
        add(&db, &game, "Empty", &[], true);
        let t = build(&db, &game, Group::Mod).unwrap();
        let names: Vec<&str> = t.root.folders.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "zeta"], "mods without files are left out");
        assert_eq!(t.root.folders[1].mod_id, Some(z));
        assert_eq!(t.root.folders[0].file_count, 2);
        assert_eq!(find(&t.root.folders[0], "r6").folders.len(), 2);
        assert_eq!(t.root.file_count, 3);
        assert_eq!(t.mods.len(), 3);
    }

    #[test]
    fn opens_only_folders_inside_the_game() {
        let (dir, ..) = setup();
        std::fs::create_dir_all(dir.path().join("archive/pc/mod")).unwrap();
        let real = dir.path().canonicalize().unwrap();
        assert_eq!(folder_in_game(dir.path(), "Archive/PC/mod").unwrap(), real.join("archive/pc/mod"));
        assert_eq!(folder_in_game(dir.path(), "archive/pc/mod/missing/deeper").unwrap(), real.join("archive/pc/mod"));
        assert_eq!(folder_in_game(dir.path(), "").unwrap(), real);
        assert!(folder_in_game(dir.path(), "../etc").is_err());
        assert!(folder_in_game(dir.path(), "/etc").is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/", dir.path().join("out")).unwrap();
            assert!(folder_in_game(dir.path(), "out/etc").is_err());
        }
    }
}
