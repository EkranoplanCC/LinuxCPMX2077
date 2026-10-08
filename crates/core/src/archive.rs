//! Safe extraction of mod archives downloaded from the internet.
//!
//! Every entry name is validated before anything touches the disk: absolute
//! paths, `..` components, drive letters and NUL bytes are rejected, symlinks
//! are refused, and both the entry count and the number of bytes actually
//! written are capped so a zip bomb can't fill the disk. Formats are chosen by
//! magic bytes, never by file extension.

use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Zip,
    SevenZ,
    Rar,
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_entries: usize,
    pub max_total_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        // Large texture packs reach several GB; anything beyond this is
        // treated as hostile.
        Self { max_entries: 100_000, max_total_bytes: 32 * 1024 * 1024 * 1024 }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Extracted {
    pub format: Format,
    /// Relative paths of extracted regular files, `/`-separated.
    pub files: Vec<String>,
    pub total_bytes: u64,
}

pub fn detect_format(path: &Path) -> Result<Format> {
    let mut magic = [0u8; 8];
    let mut f = std::fs::File::open(path)?;
    let n = f.read(&mut magic)?;
    let m = &magic[..n];
    if m.starts_with(b"PK\x03\x04") || m.starts_with(b"PK\x05\x06") {
        Ok(Format::Zip)
    } else if m.starts_with(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C]) {
        Ok(Format::SevenZ)
    } else if m.starts_with(b"Rar!\x1A\x07") {
        Ok(Format::Rar)
    } else {
        Err(Error::UnsupportedArchive(format!("{} is not a zip, 7z or rar archive", path.display())))
    }
}

/// Turn an archive entry name into a safe relative path, or reject it.
/// Returns `Ok(None)` for names that are only `.`/empty (the archive root).
pub fn sanitize_entry_name(name: &str) -> Result<Option<PathBuf>> {
    if name.contains('\0') {
        return Err(Error::UnsafeArchive(format!("NUL byte in entry name {name:?}")));
    }
    // Windows archivers often use backslashes.
    let normalized = name.replace('\\', "/");
    if normalized.starts_with('/') {
        return Err(Error::UnsafeArchive(format!("absolute path {name:?}")));
    }
    if normalized.contains(':') {
        // Drive letters (C:) and NTFS alternate data streams.
        return Err(Error::UnsafeArchive(format!("':' in entry name {name:?}")));
    }
    let mut out = PathBuf::new();
    for comp in Path::new(&normalized).components() {
        match comp {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(Error::UnsafeArchive(format!("path traversal in {name:?}")));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(Error::UnsafeArchive(format!("absolute path {name:?}")));
            }
        }
    }
    Ok((!out.as_os_str().is_empty()).then_some(out))
}

struct Writer<'a> {
    dest: &'a Path,
    limits: Limits,
    files: Vec<String>,
    total: u64,
    entries: usize,
}

impl Writer<'_> {
    fn count_entry(&mut self) -> Result<()> {
        self.entries += 1;
        if self.entries > self.limits.max_entries {
            return Err(Error::UnsafeArchive(format!("more than {} entries", self.limits.max_entries)));
        }
        Ok(())
    }

    fn write_file(&mut self, rel: &Path, reader: &mut dyn Read) -> Result<()> {
        let target = self.dest.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        ensure_inside(self.dest, &target)?;
        // create_new: a duplicate entry name must not silently overwrite, and
        // it refuses to follow a pre-existing symlink at the target.
        let mut out = std::fs::OpenOptions::new().write(true).create_new(true).open(&target).map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                Error::UnsafeArchive(format!("duplicate entry {}", rel.display()))
            } else {
                e.into()
            }
        })?;
        // Count real bytes rather than trusting header sizes.
        let remaining = self.limits.max_total_bytes - self.total;
        let mut limited = reader.take(remaining + 1);
        let mut buf = vec![0u8; 1 << 16];
        let mut written = 0u64;
        loop {
            let n = limited.read(&mut buf)?;
            if n == 0 {
                break;
            }
            written += n as u64;
            if written > remaining {
                drop(out);
                let _ = std::fs::remove_file(&target);
                return Err(Error::UnsafeArchive(format!(
                    "archive expands beyond {} bytes",
                    self.limits.max_total_bytes
                )));
            }
            out.write_all(&buf[..n])?;
        }
        self.total += written;
        self.files.push(rel.to_string_lossy().replace('\\', "/"));
        Ok(())
    }
}

/// Make sure `target`'s parent resolves inside `dest` (guards against a
/// directory having been swapped for a symlink).
fn ensure_inside(dest: &Path, target: &Path) -> Result<()> {
    let root = dest.canonicalize()?;
    let parent = target.parent().unwrap_or(target).canonicalize()?;
    if !parent.starts_with(&root) {
        return Err(Error::UnsafeArchive(format!("{} escapes the extraction directory", target.display())));
    }
    Ok(())
}

/// Extract `archive` into `dest`, which must be empty or not exist yet.
pub fn extract(archive: &Path, dest: &Path, limits: Limits) -> Result<Extracted> {
    let format = detect_format(archive)?;
    std::fs::create_dir_all(dest)?;
    if std::fs::read_dir(dest)?.next().is_some() {
        return Err(Error::Other(format!("extraction directory {} is not empty", dest.display())));
    }
    crate::activity::record_path(
        crate::activity::Kind::Extract,
        format!("Extracting {} ({format:?}) to", archive.display()),
        dest,
    );
    let result = match format {
        Format::Zip => extract_zip(archive, dest, limits),
        Format::SevenZ => extract_7z(archive, dest, limits),
        Format::Rar => extract_external(archive, dest, limits),
    };
    match result {
        Ok((files, total_bytes)) => {
            crate::activity::record_path(
                crate::activity::Kind::Extract,
                format!("Extracted {} files ({})", files.len(), crate::activity::size(total_bytes)),
                dest,
            );
            Ok(Extracted { format, files, total_bytes })
        }
        Err(e) => {
            // Never leave half-extracted, possibly hostile content behind.
            let _ = std::fs::remove_dir_all(dest);
            crate::activity::record_path(
                crate::activity::Kind::Error,
                format!("Extraction stopped, partial files deleted: {e}"),
                dest,
            );
            Err(e)
        }
    }
}

fn extract_zip(archive: &Path, dest: &Path, limits: Limits) -> Result<(Vec<String>, u64)> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(archive)?)?;
    let mut w = Writer { dest, limits, files: Vec::new(), total: 0, entries: 0 };
    for i in 0..zip.len() {
        w.count_entry()?;
        let mut entry = zip.by_index(i)?;
        let name = entry.name().to_string();
        if entry.is_symlink() {
            return Err(Error::UnsafeArchive(format!("symlink entry {name:?}")));
        }
        if entry.encrypted() {
            return Err(Error::UnsupportedArchive(format!("encrypted entry {name:?}")));
        }
        let Some(rel) = sanitize_entry_name(&name)? else { continue };
        if entry.is_dir() {
            std::fs::create_dir_all(dest.join(&rel))?;
            continue;
        }
        w.write_file(&rel, &mut entry)?;
    }
    Ok((w.files, w.total))
}

fn extract_7z(archive: &Path, dest: &Path, limits: Limits) -> Result<(Vec<String>, u64)> {
    let mut reader = sevenz_rust2::ArchiveReader::open(archive, sevenz_rust2::Password::empty())
        .map_err(|e| Error::SevenZ(e.to_string()))?;
    let mut w = Writer { dest, limits, files: Vec::new(), total: 0, entries: 0 };
    let mut failure: Option<Error> = None;
    let res = reader.for_each_entries(|entry, data| {
        let step = (|| -> Result<()> {
            w.count_entry()?;
            if entry.is_anti_item() {
                return Ok(());
            }
            let Some(rel) = sanitize_entry_name(entry.name())? else { return Ok(()) };
            if entry.is_directory() {
                std::fs::create_dir_all(dest.join(&rel))?;
                return Ok(());
            }
            w.write_file(&rel, data)
        })();
        match step {
            Ok(()) => Ok(true),
            Err(e) => {
                failure = Some(e);
                Ok(false) // stop iterating
            }
        }
    });
    if let Some(e) = failure {
        return Err(e);
    }
    res.map_err(|e| Error::SevenZ(e.to_string()))?;
    Ok((w.files, w.total))
}

/// RAR has no pure-Rust decoder, so use the system's `bsdtar` (libarchive) or
/// `unrar` (Windows 10 and later ship bsdtar as `tar.exe`). Names are listed
/// and validated first, then the tree is walked after extraction to make sure
/// nothing escaped or became a link.
fn extract_external(archive: &Path, dest: &Path, limits: Limits) -> Result<(Vec<String>, u64)> {
    let bsdtar = if cfg!(windows) { "tar" } else { "bsdtar" };
    let (list_cmd, extract_cmd): (Vec<&str>, Vec<&str>) = if which(bsdtar) {
        (vec![bsdtar, "-tf"], vec![bsdtar, "--no-same-owner", "--no-same-permissions", "-xf"])
    } else if which("unrar") {
        (vec!["unrar", "lb"], vec!["unrar", "x", "-o-", "-ol-", "-y"])
    } else {
        return Err(Error::UnsupportedArchive(
            "RAR archives need bsdtar (libarchive) or unrar installed on the system".into(),
        ));
    };
    let listing = Command::new(list_cmd[0]).args(&list_cmd[1..]).arg(archive).output()?;
    if !listing.status.success() {
        return Err(Error::UnsupportedArchive(String::from_utf8_lossy(&listing.stderr).into_owned()));
    }
    let names = String::from_utf8_lossy(&listing.stdout);
    let mut count = 0usize;
    for name in names.lines().filter(|l| !l.is_empty()) {
        sanitize_entry_name(name)?;
        count += 1;
        if count > limits.max_entries {
            return Err(Error::UnsafeArchive(format!("more than {} entries", limits.max_entries)));
        }
    }
    let mut cmd = Command::new(extract_cmd[0]);
    cmd.args(&extract_cmd[1..]).arg(archive);
    if extract_cmd[0] == bsdtar {
        cmd.arg("-C").arg(dest);
    } else {
        // unrar takes the destination as a trailing argument ending in '/'.
        cmd.arg(format!("{}/", dest.display()));
    }
    let status = cmd.output()?;
    if !status.status.success() {
        return Err(Error::UnsupportedArchive(String::from_utf8_lossy(&status.stderr).into_owned()));
    }
    validate_tree(dest, limits)
}

/// Walk an extracted tree: no symlinks, no special files, within limits.
pub fn validate_tree(dest: &Path, limits: Limits) -> Result<(Vec<String>, u64)> {
    let mut files = Vec::new();
    let mut total = 0u64;
    for entry in walkdir::WalkDir::new(dest).follow_links(false) {
        let entry = entry.map_err(|e| Error::Other(e.to_string()))?;
        let ft = entry.file_type();
        if ft.is_symlink() {
            return Err(Error::UnsafeArchive(format!("symlink {}", entry.path().display())));
        }
        if ft.is_file() {
            total += entry.metadata().map_err(|e| Error::Other(e.to_string()))?.len();
            if total > limits.max_total_bytes {
                return Err(Error::UnsafeArchive("archive expands beyond the size limit".into()));
            }
            let rel = entry.path().strip_prefix(dest).unwrap_or(entry.path());
            files.push(rel.to_string_lossy().into_owned());
        } else if !ft.is_dir() {
            return Err(Error::UnsafeArchive(format!("special file {}", entry.path().display())));
        }
    }
    Ok((files, total))
}

fn which(bin: &str) -> bool {
    let file = if cfg!(windows) { format!("{bin}.exe") } else { bin.to_string() };
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(&file).is_file()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zip::write::SimpleFileOptions;

    fn make_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let mut z = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        for (name, data) in entries {
            z.start_file(*name, SimpleFileOptions::default()).unwrap();
            z.write_all(data).unwrap();
        }
        z.finish().unwrap();
    }

    #[test]
    fn rejects_bad_names() {
        for bad in ["../evil", "a/../../evil", "/etc/passwd", "C:\\windows\\x", "a\\..\\..\\b", "x\0y", "file.txt:ads"] {
            assert!(sanitize_entry_name(bad).is_err(), "{bad:?} should be rejected");
        }
        assert_eq!(
            sanitize_entry_name("archive\\pc\\mod\\x.archive").unwrap(),
            Some(PathBuf::from("archive/pc/mod/x.archive"))
        );
        assert_eq!(sanitize_entry_name("./").unwrap(), None);
    }

    #[test]
    fn extracts_zip_and_blocks_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let good = tmp.path().join("good.zip");
        make_zip(&good, &[("archive/pc/mod/a.archive", b"aaa"), ("r6\\scripts\\b.reds", b"bb")]);
        let out = extract(&good, &tmp.path().join("out"), Limits::default()).unwrap();
        assert_eq!(out.format, Format::Zip);
        assert_eq!(out.total_bytes, 5);
        assert!(tmp.path().join("out/r6/scripts/b.reds").is_file());

        let bad = tmp.path().join("bad.zip");
        make_zip(&bad, &[("ok.txt", b"1"), ("../../escape.txt", b"2")]);
        let err = extract(&bad, &tmp.path().join("out2"), Limits::default()).unwrap_err();
        assert!(matches!(err, Error::UnsafeArchive(_)));
        assert!(!tmp.path().join("escape.txt").exists());
        assert!(!tmp.path().join("out2").exists(), "partial output is cleaned up");
    }

    #[test]
    fn enforces_size_limit_on_real_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let z = tmp.path().join("bomb.zip");
        let big = vec![0u8; 1 << 20];
        make_zip(&z, &[("a.bin", &big)]);
        let limits = Limits { max_entries: 10, max_total_bytes: 1000 };
        assert!(matches!(extract(&z, &tmp.path().join("o"), limits), Err(Error::UnsafeArchive(_))));
    }

    #[test]
    fn rejects_symlink_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("link.zip");
        let mut z = zip::ZipWriter::new(std::fs::File::create(&p).unwrap());
        z.add_symlink("link", "/etc/passwd", SimpleFileOptions::default()).unwrap();
        z.finish().unwrap();
        assert!(matches!(extract(&p, &tmp.path().join("o"), Limits::default()), Err(Error::UnsafeArchive(_))));
    }

    #[test]
    fn detects_by_magic_not_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("looks.zip");
        std::fs::write(&p, b"#!/bin/sh\necho hi").unwrap();
        assert!(matches!(detect_format(&p), Err(Error::UnsupportedArchive(_))));
    }
}
