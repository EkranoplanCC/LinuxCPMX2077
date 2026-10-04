//! Mod sources other than Nexus, behind one interface so the browse page,
//! downloads and update checks work the same for each of them.
//!
//! A source lists mods (featured and search), shows one mod's details with
//! its downloadable files, downloads a file with whatever verification the
//! source offers, and says whether an installed mod has a newer version.
//! Adding a source means implementing [`ModSource`] in a new module here and
//! listing it in [`Registry::new`]; the UI picks it up from
//! [`Registry::list`].
//!
//! Nexus keeps its own module ([`crate::nexus`], [`crate::nexus_browse`])
//! because of its quota handling and the nxm:// flow free accounts need.

pub mod github;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// What the UI needs to show a source in its picker.
#[derive(Debug, Clone, Serialize)]
pub struct SourceInfo {
    /// Stored as `mods.source`, e.g. `"github"`.
    pub id: &'static str,
    pub label: &'static str,
    /// One line for the browse page.
    pub description: &'static str,
    /// Placeholder for the search box, e.g. "Search, or paste owner/repo".
    pub search_hint: &'static str,
    /// What `Listing::popularity` counts ("stars", "endorsements").
    pub popularity_label: &'static str,
}

/// One mod in a list.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Listing {
    pub source: String,
    /// The source's own id for the mod (`owner/repo` on GitHub).
    pub id: String,
    pub name: String,
    pub author: Option<String>,
    pub summary: Option<String>,
    pub version: Option<String>,
    pub popularity: Option<i64>,
    pub downloads: Option<i64>,
    pub updated: Option<i64>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    /// Names of frameworks it needs, when the source knows.
    pub requires: Vec<String>,
    /// The mod's web page (opened with [`crate::desktop::open_url`]).
    pub url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ListingPage {
    pub listings: Vec<Listing>,
    pub total: Option<i64>,
    pub page: u32,
    pub has_more: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SourceQuery {
    #[serde(default)]
    pub text: String,
    /// 1-based.
    #[serde(default)]
    pub page: u32,
}

/// A downloadable file of a mod (a release asset on GitHub).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SourceFile {
    pub id: String,
    pub file_name: String,
    pub version: Option<String>,
    /// Heading the file is listed under, e.g. "1.21.0 (latest)".
    pub group: String,
    pub size: u64,
    pub uploaded: Option<i64>,
    /// An archive the installer can take.
    pub installable: bool,
    /// The source publishes a checksum to verify the download against.
    pub verifiable: bool,
    pub prerelease: bool,
    pub downloads: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Details {
    pub listing: Listing,
    /// Plain text (release notes, description). Never render as HTML.
    pub description: String,
    pub files: Vec<SourceFile>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Downloaded {
    pub path: PathBuf,
    pub file_name: String,
    pub sha256: String,
    pub md5: String,
    pub size: u64,
    pub verified: bool,
    /// How it was verified, for the Downloads tab ("SHA-256 matches GitHub").
    pub check: String,
}

/// What we know about an installed mod from this source.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstalledRef {
    /// `mods.source_ref`: the listing id.
    pub source_ref: String,
    /// `mods.source_file`: the file id that was installed.
    pub file_id: Option<String>,
    /// `mods.archive_name`: helps find the same file in a newer version.
    pub file_name: Option<String>,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateOffer {
    pub version: String,
    /// The file that replaces the installed one; `None` when the new version
    /// has several candidates and the user has to pick.
    pub file: Option<SourceFile>,
    /// The installed version is a pre-release and this is the stable release
    /// to go back to (its version can be lower).
    pub to_stable: bool,
}

/// Download progress callback: (bytes done, bytes total).
pub type Progress<'a> = &'a mut dyn FnMut(u64, u64);

pub trait ModSource: Send + Sync {
    fn info(&self) -> SourceInfo;
    /// Shown before the user searches.
    fn featured(&self) -> Result<Vec<Listing>> {
        Ok(vec![])
    }
    fn search(&self, query: &SourceQuery) -> Result<ListingPage>;
    /// A listing id if `input` (a URL or id the user pasted) belongs here.
    fn parse_ref(&self, input: &str) -> Option<String>;
    fn details(&self, id: &str) -> Result<Details>;
    /// Download one of `details(id).files` into `dest_dir`, verified as far
    /// as the source allows. Must refuse anything not served by the source.
    fn download(&self, id: &str, file_id: &str, dest_dir: &Path, progress: Progress) -> Result<Downloaded>;
    fn check_update(&self, installed: &InstalledRef) -> Result<Option<UpdateOffer>>;
}

pub struct Registry {
    sources: Vec<Box<dyn ModSource>>,
}

impl Registry {
    pub fn new() -> Result<Self> {
        Ok(Self { sources: vec![Box::new(github::GitHubSource::new()?)] })
    }

    pub fn with_sources(sources: Vec<Box<dyn ModSource>>) -> Self {
        Self { sources }
    }

    pub fn list(&self) -> Vec<SourceInfo> {
        self.sources.iter().map(|s| s.info()).collect()
    }

    pub fn get(&self, id: &str) -> Result<&dyn ModSource> {
        self.sources
            .iter()
            .find(|s| s.info().id == id)
            .map(|s| s.as_ref())
            .ok_or_else(|| Error::Other(format!("unknown mod source {id}")))
    }
}
