//! Installing an archive into the game: extract to staging, work out where the
//! files belong, check for conflicts, deploy with backups, and record every
//! file's hash so the mod can be verified and cleanly removed later.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::archive::{self, Limits};
use crate::db::{Db, GameRow, ModFile, NewMod};
use crate::hash;
use crate::{Error, Result};

/// Top-level folders of the game directory that mods install into.
pub const KNOWN_ROOTS: &[&str] = &["archive", "bin", "engine", "r6", "red4ext", "mods", "tools"];

#[derive(Debug, Clone, Serialize)]
pub struct PlannedFile {
    /// Path inside the extracted archive.
    pub staged: String,
    /// Path inside the game directory.
    pub target: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub files: Vec<PlannedFile>,
    /// Files that don't belong in the game dir (readmes, screenshots).
    pub skipped: Vec<String>,
    pub layout: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Conflict {
    pub path: String,
    pub other_mod_id: i64,
    pub other_mod_name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstallReport {
    pub mod_id: i64,
    pub name: String,
    pub layout: String,
    pub files_installed: usize,
    pub skipped: Vec<String>,
    pub overwritten_mods: Vec<Conflict>,
    pub backed_up_game_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifyReport {
    pub ok: usize,
    pub missing: Vec<String>,
    pub modified: Vec<String>,
    /// Files whose current content belongs to another mod that overrode this one.
    pub overridden: Vec<String>,
}

fn is_known_root(name: &str) -> bool {
    KNOWN_ROOTS.iter().any(|r| r.eq_ignore_ascii_case(name))
}

fn lower_ext(p: &str) -> String {
    Path::new(p).extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default()
}

fn file_name(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// Decide where each extracted file goes in the game directory.
///
/// Handles the common Cyberpunk packaging styles: files already laid out from
/// the game root (optionally wrapped in one or more folders), loose `.archive`
/// files, loose redscript files, and REDmod folders with an `info.json`.
pub fn plan(files: &[String], mod_name: &str) -> Result<Plan> {
    // 1. Find the shallowest prefix whose children include a known root.
    let mut best: Option<(usize, String)> = None;
    for f in files {
        let parts: Vec<&str> = f.split('/').collect();
        let dirs = &parts[..parts.len().saturating_sub(1)];
        if let Some(depth) = dirs.iter().position(|p| is_known_root(p))
            && best.as_ref().is_none_or(|(d, _)| depth < *d) {
                best = Some((depth, parts[..depth].join("/")));
            }
    }
    if let Some((_, prefix)) = best {
        let mut out = Plan { files: vec![], skipped: vec![], layout: "game-root".into() };
        let pfx = if prefix.is_empty() { String::new() } else { format!("{prefix}/") };
        for f in files {
            match f.strip_prefix(&pfx) {
                Some(rest) if rest.split('/').next().is_some_and(is_known_root) && rest.contains('/') => {
                    out.files.push(PlannedFile { staged: f.clone(), target: rest.to_string() })
                }
                _ => out.skipped.push(f.clone()),
            }
        }
        if !prefix.is_empty() {
            out.layout = format!("game-root (inside '{prefix}')");
        }
        return Ok(out);
    }

    // 2. REDmod: a folder containing info.json.
    if let Some(info) = files.iter().find(|f| file_name(f).eq_ignore_ascii_case("info.json")) {
        let dir = info.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
        let folder = if dir.is_empty() { sanitize_folder(mod_name) } else { file_name(&dir).to_string() };
        let pfx = if dir.is_empty() { String::new() } else { format!("{dir}/") };
        let mut out = Plan { files: vec![], skipped: vec![], layout: "redmod".into() };
        for f in files {
            match f.strip_prefix(&pfx) {
                Some(rest) => out.files.push(PlannedFile { staged: f.clone(), target: format!("mods/{folder}/{rest}") }),
                None => out.skipped.push(f.clone()),
            }
        }
        return Ok(out);
    }

    // 3. Loose archive / redscript / tweak files.
    let mut out = Plan { files: vec![], skipped: vec![], layout: "loose".into() };
    let script_dir = sanitize_folder(mod_name);
    for f in files {
        let target = match lower_ext(f).as_str() {
            "archive" | "xl" => Some(format!("archive/pc/mod/{}", file_name(f))),
            "reds" => Some(format!("r6/scripts/{script_dir}/{}", file_name(f))),
            "yaml" | "yml" | "tweak" => Some(format!("r6/tweaks/{script_dir}/{}", file_name(f))),
            _ => None,
        };
        match target {
            Some(t) => out.files.push(PlannedFile { staged: f.clone(), target: t }),
            None => out.skipped.push(f.clone()),
        }
    }
    if out.files.is_empty() {
        return Err(Error::Other(
            "could not tell where this mod's files go (no archive/, bin/, r6/, red4ext/ folders or .archive files)".into(),
        ));
    }
    Ok(out)
}

fn sanitize_folder(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ' ' { c } else { '_' })
        .collect();
    let s = s.trim().to_string();
    if s.is_empty() { "mod".into() } else { s }
}

/// Map a game-relative path onto the real directory names on disk, matching
/// case-insensitively so `Archive/PC/Mod` lands in the existing `archive/pc/mod`.
pub fn resolve_ci(base: &Path, rel: &str) -> PathBuf {
    let mut cur = base.to_path_buf();
    let parts: Vec<&str> = rel.split('/').collect();
    for part in parts {
        let exact = cur.join(part);
        if exact.exists() {
            cur = exact;
            continue;
        }
        let hit = std::fs::read_dir(&cur)
            .ok()
            .and_then(|rd| rd.flatten().find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(part)));
        cur = match hit {
            Some(e) => e.path(),
            None => exact,
        };
    }
    cur
}

/// Copy via a temp file in the destination dir and rename, so a crash never
/// leaves a half-written game file.
fn atomic_copy(src: &Path, dst: &Path) -> Result<()> {
    let parent = dst.parent().ok_or_else(|| Error::Other("target has no parent".into()))?;
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    std::io::copy(&mut std::fs::File::open(src)?, tmp.as_file_mut())?;
    tmp.persist(dst).map_err(|e| Error::Io(e.error))?;
    Ok(())
}

pub struct Installer<'a> {
    pub db: &'a Db,
    pub staging_root: PathBuf,
    pub backups_root: PathBuf,
    pub limits: Limits,
}

pub struct InstallOptions {
    pub meta: NewMod,
    /// Allow replacing files that another mod already installed.
    pub overwrite: bool,
}

impl Installer<'_> {
    /// Files of `plan` that other installed mods already own.
    pub fn conflicts(&self, game: &GameRow, plan: &Plan) -> Result<Vec<Conflict>> {
        let mut names: HashMap<i64, String> = HashMap::new();
        let mut out = Vec::new();
        for f in &plan.files {
            for owner in self.db.owners_of(game.id, &f.target, -1)? {
                let name = match names.get(&owner.mod_id) {
                    Some(n) => n.clone(),
                    None => {
                        let n = self.db.get_mod(owner.mod_id)?.name;
                        names.insert(owner.mod_id, n.clone());
                        n
                    }
                };
                out.push(Conflict { path: f.target.clone(), other_mod_id: owner.mod_id, other_mod_name: name });
            }
        }
        Ok(out)
    }

    pub fn install(&self, game: &GameRow, archive_path: &Path, opts: InstallOptions) -> Result<InstallReport> {
        let game_dir = PathBuf::from(&game.path);
        let (sha256, md5) = hash::file_digests(archive_path)?;
        let mut meta = opts.meta;
        meta.archive_sha256 = sha256;
        meta.archive_md5 = md5;
        if meta.archive_name.is_empty() {
            meta.archive_name = archive_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        }

        // Extract to a temporary staging dir first; it's renamed once the
        // mod has an id.
        std::fs::create_dir_all(&self.staging_root)?;
        let tmp_stage = tempfile::Builder::new().prefix("incoming-").tempdir_in(&self.staging_root)?;
        let extract_dir = tmp_stage.path().join("files");
        let extracted = archive::extract(archive_path, &extract_dir, self.limits)?;
        let plan = plan(&extracted.files, &meta.name)?;

        let conflicts = self.conflicts(game, &plan)?;
        if !conflicts.is_empty() && !opts.overwrite {
            let list: Vec<String> =
                conflicts.iter().map(|c| format!("{} (from {})", c.path, c.other_mod_name)).collect();
            return Err(Error::Conflict(list.join(", ")));
        }

        let mod_id = self.db.insert_mod(game, &meta)?;
        let stage_dir = self.staging_root.join(mod_id.to_string());
        if stage_dir.exists() {
            std::fs::remove_dir_all(&stage_dir)?;
        }
        let tmp_path = tmp_stage.keep();
        std::fs::rename(tmp_path.join("files"), &stage_dir)?;
        let _ = std::fs::remove_dir_all(&tmp_path);

        let result = self.deploy(game, &game_dir, mod_id, &stage_dir, &plan);
        match result {
            Ok(backed_up) => Ok(InstallReport {
                mod_id,
                name: meta.name,
                layout: plan.layout,
                files_installed: plan.files.len(),
                skipped: plan.skipped,
                overwritten_mods: conflicts,
                backed_up_game_files: backed_up,
            }),
            Err(e) => {
                // Roll back whatever was deployed.
                let _ = self.uninstall(mod_id);
                Err(e)
            }
        }
    }

    fn deploy(&self, game: &GameRow, game_dir: &Path, mod_id: i64, stage_dir: &Path, plan: &Plan) -> Result<Vec<String>> {
        let mut backed_up = Vec::new();
        for f in &plan.files {
            let src = stage_dir.join(&f.staged);
            let dst = resolve_ci(game_dir, &f.target);
            if !dst.starts_with(game_dir) {
                return Err(Error::UnsafeArchive(format!("{} resolves outside the game dir", f.target)));
            }
            if dst.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(Error::UnsafeArchive(format!("refusing to write through symlink {}", dst.display())));
            }
            let sha = hash::sha256_file(&src)?;
            if dst.is_file() {
                let owned = !self.db.owners_of(game.id, &f.target, mod_id)?.is_empty();
                if !owned {
                    // An original game file (or something we didn't install):
                    // keep a copy so uninstall restores it.
                    let cur_sha = hash::sha256_file(&dst)?;
                    let backup = self.backups_root.join(game.id.to_string()).join(&f.target);
                    atomic_copy(&dst, &backup)?;
                    self.db.add_backup(game.id, &f.target, &backup.to_string_lossy(), &cur_sha)?;
                    backed_up.push(f.target.clone());
                }
            }
            atomic_copy(&src, &dst)?;
            let size = std::fs::metadata(&src)?.len() as i64;
            self.db.add_mod_file(&ModFile {
                mod_id,
                rel_path: f.target.clone(),
                staged_path: f.staged.clone(),
                sha256: sha,
                size,
            })?;
        }
        Ok(backed_up)
    }

    /// Remove a mod's files. A file is only deleted if it still has the
    /// content this mod installed; then the next owner's copy or the original
    /// game file is put back.
    pub fn uninstall(&self, mod_id: i64) -> Result<()> {
        let m = self.db.get_mod(mod_id)?;
        let game = self.db.game(m.game_id)?;
        let game_dir = PathBuf::from(&game.path);
        for f in self.db.mod_files(mod_id)? {
            let dst = resolve_ci(&game_dir, &f.rel_path);
            if !dst.starts_with(&game_dir) {
                continue;
            }
            let ours = dst.is_file() && hash::sha256_file(&dst)? == f.sha256;
            if !ours {
                continue; // overridden by another mod or edited by the user
            }
            std::fs::remove_file(&dst)?;
            if let Some(next) = self.db.owners_of(game.id, &f.rel_path, mod_id)?.into_iter().next() {
                let src = self.staging_root.join(next.mod_id.to_string()).join(&next.staged_path);
                if src.is_file() {
                    atomic_copy(&src, &dst)?;
                    continue;
                }
            }
            if let Some((backup, _)) = self.db.take_backup(game.id, &f.rel_path)? {
                atomic_copy(Path::new(&backup), &dst)?;
                let _ = std::fs::remove_file(&backup);
            } else {
                remove_empty_parents(&dst, &game_dir);
            }
        }
        self.db.delete_mod(mod_id)?;
        let stage = self.staging_root.join(mod_id.to_string());
        if stage.exists() {
            std::fs::remove_dir_all(stage)?;
        }
        Ok(())
    }

    /// Compare what's on disk with what was recorded at install time.
    pub fn verify(&self, mod_id: i64) -> Result<VerifyReport> {
        let m = self.db.get_mod(mod_id)?;
        let game = self.db.game(m.game_id)?;
        let game_dir = PathBuf::from(&game.path);
        let mut r = VerifyReport { ok: 0, missing: vec![], modified: vec![], overridden: vec![] };
        for f in self.db.mod_files(mod_id)? {
            let dst = resolve_ci(&game_dir, &f.rel_path);
            if !dst.is_file() {
                r.missing.push(f.rel_path);
                continue;
            }
            let cur = hash::sha256_file(&dst)?;
            if cur == f.sha256 {
                r.ok += 1;
            } else if self.db.owners_of(game.id, &f.rel_path, mod_id)?.iter().any(|o| o.sha256 == cur) {
                r.overridden.push(f.rel_path);
            } else {
                r.modified.push(f.rel_path);
            }
        }
        Ok(r)
    }
}

fn remove_empty_parents(file: &Path, stop: &Path) {
    let mut dir = file.parent();
    while let Some(d) = dir {
        if d == stop || !d.starts_with(stop) {
            break;
        }
        // Only removes empty dirs; stops at the first non-empty one.
        if std::fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::{GameInstall, Store};
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn plans_common_layouts() {
        let p = plan(&s(&["MyMod/archive/pc/mod/x.archive", "MyMod/readme.txt"]), "My Mod").unwrap();
        assert_eq!(p.files[0].target, "archive/pc/mod/x.archive");
        assert_eq!(p.skipped, s(&["MyMod/readme.txt"]));

        let p = plan(&s(&["cool.archive", "cool.xl", "pic.png"]), "Cool").unwrap();
        assert_eq!(p.layout, "loose");
        assert_eq!(p.files.len(), 2);

        let p = plan(&s(&["Foo/info.json", "Foo/archives/a.archive"]), "Foo").unwrap();
        assert_eq!(p.layout, "redmod");
        assert_eq!(p.files[1].target, "mods/Foo/archives/a.archive");

        let p = plan(&s(&["bin/x64/plugins/cyber_engine_tweaks/mods/m/init.lua"]), "m").unwrap();
        assert_eq!(p.files[0].target, "bin/x64/plugins/cyber_engine_tweaks/mods/m/init.lua");

        assert!(plan(&s(&["readme.md"]), "x").is_err());
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        db: Db,
        game: GameRow,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let game_dir = root.join("game");
        std::fs::create_dir_all(game_dir.join("bin/x64")).unwrap();
        std::fs::write(game_dir.join("bin/x64/Cyberpunk2077.exe"), b"exe").unwrap();
        std::fs::create_dir_all(game_dir.join("engine/config")).unwrap();
        std::fs::write(game_dir.join("engine/config/base.ini"), b"vanilla").unwrap();
        let db = Db::open_in_memory().unwrap();
        let gi = GameInstall {
            path: game_dir,
            store: Store::Manual,
            proton_prefix: None,
            build_id: Some("1".into()),
            exe_file_version: None,
            exe_product_version: Some("2.3".into()),
            frameworks: vec![],
            launch_options: None,
            warnings: vec![],
        };
        let id = db.upsert_game(&gi).unwrap();
        let game = db.game(id).unwrap();
        Fixture { _tmp: tmp, root, db, game }
    }

    fn zip_with(path: &Path, entries: &[(&str, &[u8])]) {
        let mut z = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        for (n, d) in entries {
            z.start_file(*n, SimpleFileOptions::default()).unwrap();
            z.write_all(d).unwrap();
        }
        z.finish().unwrap();
    }

    fn meta(name: &str) -> InstallOptions {
        InstallOptions { meta: NewMod { name: name.into(), source: "manual".into(), ..Default::default() }, overwrite: false }
    }

    #[test]
    fn install_conflict_override_and_uninstall_restore() {
        let f = fixture();
        let inst = Installer {
            db: &f.db,
            staging_root: f.root.join("staging"),
            backups_root: f.root.join("backups"),
            limits: Limits::default(),
        };
        let game_dir = PathBuf::from(&f.game.path);

        let a = f.root.join("a.zip");
        zip_with(&a, &[("A/archive/pc/mod/shared.archive", b"from-a"), ("A/Engine/Config/base.ini", b"mod-a-ini")]);
        let ra = inst.install(&f.game, &a, meta("A")).unwrap();
        assert_eq!(ra.files_installed, 2);
        assert_eq!(ra.backed_up_game_files, vec!["Engine/Config/base.ini".to_string()]);
        // Case-insensitive merge into the existing engine/config dir.
        assert_eq!(std::fs::read(game_dir.join("engine/config/base.ini")).unwrap(), b"mod-a-ini");
        assert!(!game_dir.join("Engine").exists());

        let b = f.root.join("b.zip");
        zip_with(&b, &[("archive/pc/mod/shared.archive", b"from-b")]);
        let err = inst.install(&f.game, &b, meta("B")).unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "{err}");

        let mut ob = meta("B");
        ob.overwrite = true;
        let rb = inst.install(&f.game, &b, ob).unwrap();
        assert_eq!(rb.overwritten_mods.len(), 1);
        assert_eq!(std::fs::read(game_dir.join("archive/pc/mod/shared.archive")).unwrap(), b"from-b");
        assert_eq!(inst.verify(ra.mod_id).unwrap().overridden.len(), 1);

        // Removing B brings A's copy back.
        inst.uninstall(rb.mod_id).unwrap();
        assert_eq!(std::fs::read(game_dir.join("archive/pc/mod/shared.archive")).unwrap(), b"from-a");

        // Removing A restores the vanilla ini and deletes its archive.
        inst.uninstall(ra.mod_id).unwrap();
        assert_eq!(std::fs::read(game_dir.join("engine/config/base.ini")).unwrap(), b"vanilla");
        assert!(!game_dir.join("archive").exists(), "empty dirs cleaned up");
        assert!(f.db.mods(f.game.id).unwrap().is_empty());
    }

    #[test]
    fn verify_detects_tampering() {
        let f = fixture();
        let inst = Installer {
            db: &f.db,
            staging_root: f.root.join("staging"),
            backups_root: f.root.join("backups"),
            limits: Limits::default(),
        };
        let a = f.root.join("a.zip");
        zip_with(&a, &[("x.archive", b"one"), ("y.archive", b"two")]);
        let r = inst.install(&f.game, &a, meta("Loose")).unwrap();
        let game_dir = PathBuf::from(&f.game.path);
        std::fs::write(game_dir.join("archive/pc/mod/x.archive"), b"tampered").unwrap();
        std::fs::remove_file(game_dir.join("archive/pc/mod/y.archive")).unwrap();
        let v = inst.verify(r.mod_id).unwrap();
        assert_eq!(v.modified, vec!["archive/pc/mod/x.archive"]);
        assert_eq!(v.missing, vec!["archive/pc/mod/y.archive"]);
    }
}
