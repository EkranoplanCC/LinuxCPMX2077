//! Installing an archive into the game: extract to staging, work out where the
//! files belong, check for conflicts, deploy with backups, and record every
//! file's hash so the mod can be verified and cleanly removed later.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::archive::{self, Limits};
use crate::db::{Db, GameRow, ModFile, NewMod, STATUS_DISABLED, STATUS_ENABLED};
use crate::fomod;
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

#[derive(Debug, Clone, Default, Serialize)]
pub struct EnableReport {
    pub files_deployed: usize,
    /// Files changed after install that stayed in the game dir while the mod
    /// was disabled; they're kept rather than reset to the mod's copy.
    pub kept_in_place: Vec<String>,
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

    // 2. REDmod: a folder with info.json next to REDmod content folders.
    let redmod_dirs = ["archives", "customsounds", "scripts", "tweaks"];
    let redmod_info = files.iter().find(|f| {
        if !file_name(f).eq_ignore_ascii_case("info.json") {
            return false;
        }
        let dir = f.rsplit_once('/').map(|(d, _)| format!("{}/", d.to_lowercase())).unwrap_or_default();
        files.iter().any(|o| {
            o.to_lowercase()
                .strip_prefix(&dir)
                .and_then(|rest| rest.split_once('/'))
                .is_some_and(|(first, _)| redmod_dirs.contains(&first))
        })
    });
    if let Some(info) = redmod_info {
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

    // 3. Cyber Engine Tweaks mod folder packaged on its own (has init.lua).
    if let Some(init) = files
        .iter()
        .filter(|f| file_name(f).eq_ignore_ascii_case("init.lua"))
        .min_by_key(|f| f.matches('/').count())
    {
        let dir = init.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
        let folder = if dir.is_empty() { sanitize_folder(mod_name) } else { file_name(&dir).to_string() };
        let pfx = if dir.is_empty() { String::new() } else { format!("{dir}/") };
        let mut out = Plan { files: vec![], skipped: vec![], layout: "cet-mod".into() };
        for f in files {
            match f.strip_prefix(&pfx) {
                Some(rest) => out.files.push(PlannedFile {
                    staged: f.clone(),
                    target: format!("bin/x64/plugins/cyber_engine_tweaks/mods/{folder}/{rest}"),
                }),
                None => out.skipped.push(f.clone()),
            }
        }
        return Ok(out);
    }

    // 4. Loose archive / redscript / tweak files.
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

#[derive(Default)]
pub struct InstallOptions {
    pub meta: NewMod,
    /// Allow replacing files that another mod already installed.
    pub overwrite: bool,
    /// Answers for a FOMOD installer; `None` uses its defaults.
    pub fomod_choices: Option<fomod::Selections>,
}

/// An archive that has been hashed and extracted to staging but not yet
/// installed, so the user can answer installer questions first.
#[derive(Debug, Clone, Serialize)]
pub struct Prepared {
    /// Token identifying the staging directory.
    pub id: String,
    pub archive_name: String,
    pub archive_sha256: String,
    pub archive_md5: String,
    pub file_count: usize,
    pub fomod: Option<FomodInfo>,
    #[serde(skip)]
    pub dir: PathBuf,
    #[serde(skip)]
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FomodInfo {
    /// Folder inside the archive that holds `fomod/` (`""` or `"X/"`).
    pub root: String,
    pub installer: fomod::Installer,
    pub defaults: fomod::Selections,
}

/// FOMOD file conditions look at what's already in the game directory.
pub fn game_files(game_dir: &Path) -> impl Fn(&str) -> fomod::FileState + '_ {
    move |rel: &str| {
        if crate::game::exists_ci(game_dir, rel) { fomod::FileState::Active } else { fomod::FileState::Missing }
    }
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

    /// Hash and extract an archive, and read its FOMOD installer if it has one.
    pub fn prepare(&self, game: &GameRow, archive_path: &Path) -> Result<Prepared> {
        let (sha256, md5) = hash::file_digests(archive_path)?;
        std::fs::create_dir_all(&self.staging_root)?;
        let dir = tempfile::Builder::new().prefix("incoming-").tempdir_in(&self.staging_root)?.keep();
        let id = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let extracted = match archive::extract(archive_path, &dir.join("files"), self.limits) {
            Ok(x) => x,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e);
            }
        };
        let fomod = match fomod::find_config(&extracted.files) {
            Some((xml_path, root)) => {
                let read = std::fs::read(dir.join("files").join(&xml_path))
                    .map_err(Error::from)
                    .and_then(|b| fomod::decode_xml(&b))
                    .and_then(|x| fomod::parse(&x));
                match read {
                    Ok(installer) => {
                        let defaults = installer.default_selections(&game_files(Path::new(&game.path)));
                        Some(FomodInfo { root, installer, defaults })
                    }
                    Err(e) => {
                        let _ = std::fs::remove_dir_all(&dir);
                        return Err(e);
                    }
                }
            }
            None => None,
        };
        Ok(Prepared {
            id,
            archive_name: archive_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            archive_sha256: sha256,
            archive_md5: md5,
            file_count: extracted.files.len(),
            fomod,
            dir,
            files: extracted.files,
        })
    }

    /// Look up a prepared archive by its token (e.g. after an app restart).
    pub fn prepared_dir(&self, id: &str) -> Result<PathBuf> {
        if !id.starts_with("incoming-") || id.contains('/') || id.contains("..") {
            return Err(Error::Other("invalid install token".into()));
        }
        Ok(self.staging_root.join(id))
    }

    /// Work out where every file goes, answering FOMOD questions with
    /// `choices` (or the installer's defaults).
    pub fn plan_prepared(&self, game: &GameRow, p: &Prepared, name: &str, choices: Option<&fomod::Selections>) -> Result<Plan> {
        let Some(fm) = &p.fomod else { return plan(&p.files, name) };
        let rel: Vec<String> = p.files.iter().filter_map(|f| f.strip_prefix(&fm.root).map(String::from)).collect();
        let sel = choices.unwrap_or(&fm.defaults);
        let resolved = fm.installer.resolve(sel, &game_files(Path::new(&game.path)), &rel)?;
        if resolved.files.is_empty() {
            return Err(Error::Other("the chosen installer options don't install any files".into()));
        }
        Ok(Plan {
            files: resolved
                .files
                .into_iter()
                .map(|(src, dst)| PlannedFile { staged: format!("{}{}", fm.root, src), target: dst })
                .collect(),
            skipped: resolved.missing_sources.into_iter().map(|m| format!("{m} (named by installer, not in archive)")).collect(),
            layout: "fomod".into(),
        })
    }

    /// Install a prepared archive. On a conflict the prepared files are kept
    /// so the user can retry with overwriting allowed.
    pub fn finish(&self, game: &GameRow, p: &Prepared, opts: InstallOptions) -> Result<InstallReport> {
        let game_dir = PathBuf::from(&game.path);
        let mut meta = opts.meta;
        meta.archive_sha256 = p.archive_sha256.clone();
        meta.archive_md5 = p.archive_md5.clone();
        if meta.archive_name.is_empty() {
            meta.archive_name = p.archive_name.clone();
        }
        if meta.name.trim().is_empty() {
            meta.name = p
                .fomod
                .as_ref()
                .map(|f| f.installer.module_name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| p.archive_name.clone());
        }
        let plan = self.plan_prepared(game, p, &meta.name, opts.fomod_choices.as_ref())?;

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
        std::fs::rename(p.dir.join("files"), &stage_dir)?;
        let _ = std::fs::remove_dir_all(&p.dir);

        match self.deploy(game, &game_dir, mod_id, &stage_dir, &plan) {
            Ok(backed_up) => {
                // Indexing failures shouldn't undo a good install; the
                // analysis re-indexes anything missing.
                if let Err(e) = crate::analysis::index_mod(self.db, &self.staging_root, mod_id) {
                    log::warn!("indexing mod {mod_id} failed: {e}");
                }
                Ok(InstallReport {
                mod_id,
                name: meta.name,
                layout: plan.layout,
                files_installed: plan.files.len(),
                skipped: plan.skipped,
                overwritten_mods: conflicts,
                backed_up_game_files: backed_up,
                })
            }
            Err(e) => {
                // Roll back whatever was deployed.
                let _ = self.uninstall(mod_id);
                Err(e)
            }
        }
    }

    /// Install `p` as the new version of `old`. The old version is taken out
    /// first so its files don't count as conflicts, and is put back if the
    /// new one fails to install. A disabled old version stays disabled.
    pub fn finish_replacing(&self, game: &GameRow, p: &Prepared, opts: InstallOptions, old: i64) -> Result<InstallReport> {
        let old_mod = self.db.get_mod(old)?;
        if old_mod.game_id != game.id {
            return Err(Error::Other(format!("{} belongs to another game install", old_mod.name)));
        }
        let was_enabled = old_mod.enabled();
        if was_enabled {
            self.disable(old)?;
        }
        match self.finish(game, p, opts) {
            Ok(r) => {
                self.uninstall(old)?;
                if !was_enabled {
                    self.disable(r.mod_id)?;
                }
                Ok(r)
            }
            Err(e) => {
                if was_enabled && let Err(back) = self.enable(old, true) {
                    log::warn!("could not re-enable {} after a failed update: {back}", old_mod.name);
                }
                Err(e)
            }
        }
    }

    pub fn discard(&self, p: &Prepared) {
        let _ = std::fs::remove_dir_all(&p.dir);
    }

    /// Remove leftovers of installs that were never finished.
    pub fn cleanup_incoming(&self) {
        if let Ok(rd) = std::fs::read_dir(&self.staging_root) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().starts_with("incoming-") {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
    }

    /// Prepare and install in one go (FOMODs use `opts.fomod_choices` or
    /// their defaults).
    pub fn install(&self, game: &GameRow, archive_path: &Path, opts: InstallOptions) -> Result<InstallReport> {
        let p = self.prepare(game, archive_path)?;
        let r = self.finish(game, &p, opts);
        if r.is_err() {
            self.discard(&p);
        }
        r
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

    /// Take a mod's files out of the game dir. A file is only deleted if it
    /// still has the content this mod installed; then the next enabled
    /// owner's copy or the original game file is put back. Returns the files
    /// that were changed after install (and aren't another mod's copy), which
    /// are left alone, with their current hash.
    fn undeploy(&self, game: &GameRow, mod_id: i64) -> Result<Vec<(String, String)>> {
        let game_dir = PathBuf::from(&game.path);
        let mut kept = Vec::new();
        for f in self.db.mod_files(mod_id)? {
            let dst = resolve_ci(&game_dir, &f.rel_path);
            if !dst.starts_with(&game_dir) || !dst.is_file() {
                continue;
            }
            let cur = hash::sha256_file(&dst)?;
            if cur != f.sha256 {
                // Overridden by another mod, or edited by the user or the mod
                // itself (configs); either way not ours to delete.
                let other = self.db.owners_of(game.id, &f.rel_path, mod_id)?.iter().any(|o| o.sha256 == cur);
                if !other {
                    kept.push((f.rel_path.clone(), cur));
                }
                continue;
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
        Ok(kept)
    }

    /// Remove a mod's files from the game and forget the mod.
    pub fn uninstall(&self, mod_id: i64) -> Result<()> {
        let m = self.db.get_mod(mod_id)?;
        if m.enabled() {
            let game = self.db.game(m.game_id)?;
            self.undeploy(&game, mod_id)?;
        }
        self.db.delete_mod(mod_id)?;
        let stage = self.staging_root.join(mod_id.to_string());
        if stage.exists() {
            std::fs::remove_dir_all(stage)?;
        }
        Ok(())
    }

    /// Take a mod out of the game but keep its staged copy so it can be
    /// switched back on. Returns files left in place because they changed
    /// after install.
    pub fn disable(&self, mod_id: i64) -> Result<Vec<String>> {
        let m = self.db.get_mod(mod_id)?;
        if !m.enabled() {
            return Ok(vec![]);
        }
        let game = self.db.game(m.game_id)?;
        // Mark it first so it no longer counts as an owner of its files.
        self.db.set_mod_status(mod_id, STATUS_DISABLED)?;
        match self.undeploy(&game, mod_id) {
            Ok(kept) => {
                self.db.set_kept_files(mod_id, &kept)?;
                Ok(kept.into_iter().map(|(p, _)| p).collect())
            }
            Err(e) => {
                let _ = self.db.set_mod_status(mod_id, STATUS_ENABLED);
                Err(e)
            }
        }
    }

    /// Put a disabled mod back from its staged copy. Files it left behind
    /// when disabled are kept as they are, so changed settings survive.
    pub fn enable(&self, mod_id: i64, overwrite: bool) -> Result<EnableReport> {
        let m = self.db.get_mod(mod_id)?;
        let game = self.db.game(m.game_id)?;
        if m.enabled() {
            return Ok(EnableReport::default());
        }
        let game_dir = PathBuf::from(&game.path);
        let stage_dir = self.staging_root.join(mod_id.to_string());
        let kept: HashMap<String, String> = self.db.kept_files(mod_id)?.into_iter().collect();
        let mut plan = Plan { files: vec![], skipped: vec![], layout: String::new() };
        let mut kept_in_place = Vec::new();
        for f in self.db.mod_files(mod_id)? {
            // Check the stored copy before touching the game dir.
            let src = stage_dir.join(&f.staged_path);
            if !src.is_file() || hash::sha256_file(&src)? != f.sha256 {
                return Err(Error::Integrity(format!(
                    "the stored copy of {} is missing or changed; reinstall the mod",
                    f.rel_path
                )));
            }
            if let Some(sha) = kept.get(&f.rel_path) {
                let dst = resolve_ci(&game_dir, &f.rel_path);
                if dst.is_file() && hash::sha256_file(&dst)? == *sha {
                    kept_in_place.push(f.rel_path);
                    continue;
                }
            }
            plan.files.push(PlannedFile { staged: f.staged_path, target: f.rel_path });
        }
        let conflicts = self.conflicts(&game, &plan)?;
        if !conflicts.is_empty() && !overwrite {
            let list: Vec<String> =
                conflicts.iter().map(|c| format!("{} (from {})", c.path, c.other_mod_name)).collect();
            return Err(Error::Conflict(list.join(", ")));
        }
        match self.deploy(&game, &game_dir, mod_id, &stage_dir, &plan) {
            Ok(backed_up) => {
                self.db.set_mod_status(mod_id, STATUS_ENABLED)?;
                self.db.set_kept_files(mod_id, &[])?;
                Ok(EnableReport {
                    files_deployed: plan.files.len(),
                    kept_in_place,
                    overwritten_mods: conflicts,
                    backed_up_game_files: backed_up,
                })
            }
            Err(e) => {
                let _ = self.undeploy(&game, mod_id);
                Err(e)
            }
        }
    }

    /// Compare what's on disk with what was recorded at install time.
    pub fn verify(&self, mod_id: i64) -> Result<VerifyReport> {
        let m = self.db.get_mod(mod_id)?;
        if !m.enabled() {
            return Err(Error::Other(format!("{} is disabled", m.name)));
        }
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

        let p = plan(&s(&["Better HUD/init.lua", "Better HUD/modules/ui.lua", "Better HUD.txt"]), "x").unwrap();
        assert_eq!(p.layout, "cet-mod");
        assert_eq!(p.files[1].target, "bin/x64/plugins/cyber_engine_tweaks/mods/Better HUD/modules/ui.lua");

        // A stray info.json without REDmod folders isn't REDmod.
        let p = plan(&s(&["info.json", "a.archive"]), "x").unwrap();
        assert_eq!(p.layout, "loose");

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
        InstallOptions { meta: NewMod { name: name.into(), source: "manual".into(), ..Default::default() }, ..Default::default() }
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
    fn installs_fomod_with_choices() {
        let f = fixture();
        let inst = Installer {
            db: &f.db,
            staging_root: f.root.join("staging"),
            backups_root: f.root.join("backups"),
            limits: Limits::default(),
        };
        let xml = br#"<config><moduleName>Fancy Textures</moduleName>
          <installSteps order="Explicit"><installStep name="Pick">
            <optionalFileGroups><group name="Size" type="SelectExactlyOne"><plugins order="Explicit">
              <plugin name="Small"><description/><files><folder source="small" destination="archive\pc\mod"/></files>
                <typeDescriptor><type name="Recommended"/></typeDescriptor></plugin>
              <plugin name="Large"><description/><files><folder source="large" destination="archive\pc\mod"/></files>
                <typeDescriptor><type name="Optional"/></typeDescriptor></plugin>
            </plugins></group></optionalFileGroups>
          </installStep></installSteps></config>"#;
        let a = f.root.join("fomod.zip");
        zip_with(&a, &[
            ("Fancy/fomod/ModuleConfig.xml", xml),
            ("Fancy/small/tex.archive", b"small"),
            ("Fancy/large/tex.archive", b"large"),
        ]);
        let game = f.game.clone();
        let p = inst.prepare(&game, &a).unwrap();
        let fm = p.fomod.as_ref().expect("fomod detected");
        assert_eq!(fm.root, "Fancy/");
        assert_eq!(fm.defaults, vec![vec![vec![0]]]);
        let opts = InstallOptions { fomod_choices: Some(vec![vec![vec![1]]]), ..Default::default() };
        let r = inst.finish(&game, &p, opts).unwrap();
        assert_eq!(r.name, "Fancy Textures");
        assert_eq!(r.layout, "fomod");
        let game_dir = PathBuf::from(&game.path);
        assert_eq!(std::fs::read(game_dir.join("archive/pc/mod/tex.archive")).unwrap(), b"large");
        assert!(!p.dir.exists(), "prepared dir moved into mod staging");

        // Uninstall works from the staged copy like any other mod.
        inst.uninstall(r.mod_id).unwrap();
        assert!(!game_dir.join("archive/pc/mod/tex.archive").exists());
    }

    #[test]
    fn analysis_flags_conflicting_mods() {
        let f = fixture();
        let inst = Installer {
            db: &f.db,
            staging_root: f.root.join("staging"),
            backups_root: f.root.join("backups"),
            limits: Limits::default(),
        };
        let a = f.root.join("a.zip");
        zip_with(&a, &[("r6/scripts/a/a.reds", b"@replaceMethod(PlayerPuppet)\nfunc OnDeath() -> Bool { return false; }\n")]);
        let b = f.root.join("b.zip");
        zip_with(&b, &[("r6/scripts/b/b.reds", b"@replaceMethod(PlayerPuppet) func OnDeath() -> Bool { return true; }")]);
        inst.install(&f.game, &a, meta("Immortal")).unwrap();
        inst.install(&f.game, &b, meta("Hardcore")).unwrap();
        let r = crate::analysis::report_for_game(&f.db, &inst.staging_root, &f.game).unwrap();
        let hit = r.findings.iter().find(|x| x.key == "PlayerPuppet.OnDeath").expect("replace conflict found");
        assert_eq!(hit.severity, crate::analysis::Severity::Error);
        assert!(r.findings.iter().any(|x| x.key == "redscript"), "redscript isn't installed in the fixture");
    }

    #[test]
    fn disable_and_enable_restore_the_right_files() {
        let f = fixture();
        let inst = Installer {
            db: &f.db,
            staging_root: f.root.join("staging"),
            backups_root: f.root.join("backups"),
            limits: Limits::default(),
        };
        let game_dir = PathBuf::from(&f.game.path);
        let shared = game_dir.join("archive/pc/mod/shared.archive");
        let ini = game_dir.join("engine/config/base.ini");
        let cfg = game_dir.join("bin/x64/plugins/cyber_engine_tweaks/mods/A/config.json");

        let a = f.root.join("a.zip");
        zip_with(
            &a,
            &[
                ("archive/pc/mod/shared.archive", b"from-a"),
                ("engine/config/base.ini", b"mod-a-ini"),
                ("bin/x64/plugins/cyber_engine_tweaks/mods/A/init.lua", b"-- a"),
                ("bin/x64/plugins/cyber_engine_tweaks/mods/A/config.json", b"{}"),
            ],
        );
        let ra = inst.install(&f.game, &a, meta("A")).unwrap();
        let b = f.root.join("b.zip");
        zip_with(&b, &[("archive/pc/mod/shared.archive", b"from-b")]);
        let mut ob = meta("B");
        ob.overwrite = true;
        let rb = inst.install(&f.game, &b, ob).unwrap();

        // Disabling B brings A's copy back; B's files are gone from the game.
        assert!(inst.disable(rb.mod_id).unwrap().is_empty());
        assert_eq!(std::fs::read(&shared).unwrap(), b"from-a");
        assert_eq!(f.db.get_mod(rb.mod_id).unwrap().status, "disabled");
        assert!(inst.verify(rb.mod_id).is_err());

        // The game rewrote A's config. Disabling A restores the vanilla ini,
        // removes its files but leaves the changed config alone.
        std::fs::write(&cfg, b"{\"fov\": 90}").unwrap();
        let kept = inst.disable(ra.mod_id).unwrap();
        assert_eq!(kept, vec!["bin/x64/plugins/cyber_engine_tweaks/mods/A/config.json".to_string()]);
        assert_eq!(std::fs::read(&ini).unwrap(), b"vanilla");
        assert!(!shared.exists());
        assert!(!game_dir.join("bin/x64/plugins/cyber_engine_tweaks/mods/A/init.lua").exists());
        assert_eq!(std::fs::read(&cfg).unwrap(), b"{\"fov\": 90}");

        // Disabled mods don't take part in the analysis.
        let r = crate::analysis::report_for_game(&f.db, &inst.staging_root, &f.game).unwrap();
        assert!(r.mods.is_empty());

        // Enabling A puts its files back, backs up the ini again and keeps
        // the changed config.
        let ea = inst.enable(ra.mod_id, false).unwrap();
        assert_eq!(ea.files_deployed, 3);
        assert_eq!(ea.kept_in_place, vec!["bin/x64/plugins/cyber_engine_tweaks/mods/A/config.json".to_string()]);
        assert_eq!(ea.backed_up_game_files, vec!["engine/config/base.ini".to_string()]);
        assert_eq!(std::fs::read(&ini).unwrap(), b"mod-a-ini");
        assert_eq!(std::fs::read(&shared).unwrap(), b"from-a");
        assert_eq!(std::fs::read(&cfg).unwrap(), b"{\"fov\": 90}");

        // Enabling B now clashes with A unless overwriting is allowed.
        assert!(matches!(inst.enable(rb.mod_id, false), Err(Error::Conflict(_))));
        assert_eq!(f.db.get_mod(rb.mod_id).unwrap().status, "disabled");
        let eb = inst.enable(rb.mod_id, true).unwrap();
        assert_eq!(eb.overwritten_mods.len(), 1);
        assert_eq!(std::fs::read(&shared).unwrap(), b"from-b");

        // A disabled mod's stored copy is checked before it goes back in.
        inst.disable(rb.mod_id).unwrap();
        let staged = inst.staging_root.join(rb.mod_id.to_string()).join(&f.db.mod_files(rb.mod_id).unwrap()[0].staged_path);
        std::fs::write(&staged, b"tampered").unwrap();
        assert!(matches!(inst.enable(rb.mod_id, true), Err(Error::Integrity(_))));
        assert_eq!(std::fs::read(&shared).unwrap(), b"from-a");

        // Uninstalling a disabled mod doesn't touch the game.
        inst.uninstall(rb.mod_id).unwrap();
        assert_eq!(std::fs::read(&shared).unwrap(), b"from-a");
        assert!(!inst.staging_root.join(rb.mod_id.to_string()).exists());
        inst.uninstall(ra.mod_id).unwrap();
        assert_eq!(std::fs::read(&ini).unwrap(), b"vanilla");
        assert!(!shared.exists());
    }

    #[test]
    fn update_replaces_the_old_version_or_rolls_back() {
        let f = fixture();
        let inst = Installer {
            db: &f.db,
            staging_root: f.root.join("staging"),
            backups_root: f.root.join("backups"),
            limits: Limits::default(),
        };
        let game_dir = PathBuf::from(&f.game.path);
        let v1 = f.root.join("v1.zip");
        zip_with(&v1, &[("archive/pc/mod/x.archive", b"v1"), ("r6/scripts/m/old.reds", b"// old")]);
        let r1 = inst.install(&f.game, &v1, meta("Mod")).unwrap();

        let v2 = f.root.join("v2.zip");
        zip_with(&v2, &[("archive/pc/mod/x.archive", b"v2"), ("r6/scripts/m/new.reds", b"// new")]);
        let p = inst.prepare(&f.game, &v2).unwrap();
        let mut opts = meta("Mod");
        opts.meta.version = Some("2.0".into());
        let r2 = inst.finish_replacing(&f.game, &p, opts, r1.mod_id).unwrap();
        assert_eq!(std::fs::read(game_dir.join("archive/pc/mod/x.archive")).unwrap(), b"v2");
        assert!(!game_dir.join("r6/scripts/m/old.reds").exists(), "files the new version dropped are gone");
        assert!(game_dir.join("r6/scripts/m/new.reds").exists());
        let mods = f.db.mods(f.game.id).unwrap();
        assert_eq!(mods.len(), 1);
        assert_eq!(mods[0].id, r2.mod_id);
        assert_eq!(mods[0].version.as_deref(), Some("2.0"));

        // A failed update (here: a clash with another mod) puts v2 back.
        let other = f.root.join("other.zip");
        zip_with(&other, &[("r6/scripts/m/clash.reds", b"// other")]);
        inst.install(&f.game, &other, meta("Other")).unwrap();
        let v3 = f.root.join("v3.zip");
        zip_with(&v3, &[("archive/pc/mod/x.archive", b"v3"), ("r6/scripts/m/clash.reds", b"// v3")]);
        let p = inst.prepare(&f.game, &v3).unwrap();
        let err = inst.finish_replacing(&f.game, &p, meta("Mod"), r2.mod_id).unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "{err}");
        assert_eq!(std::fs::read(game_dir.join("archive/pc/mod/x.archive")).unwrap(), b"v2");
        assert!(f.db.get_mod(r2.mod_id).unwrap().enabled());
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
