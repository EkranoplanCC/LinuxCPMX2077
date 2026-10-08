//! Core logic for the CP2077 Linux mod manager: game detection, mod database,
//! safe archive installation and the Nexus Mods API client. The Tauri app is a
//! thin shell over this crate so everything here is testable without a GUI.

pub mod activity;
pub mod analysis;
pub mod archive;
pub mod crash;
pub mod db;
pub mod dependencies;
pub mod desktop;
pub mod downloads;
pub mod error;
pub mod file_tree;
pub mod fomod;
pub mod game;
pub mod game_versions;
pub mod hash;
pub mod install;
pub mod known_issues;
pub mod linux_setup;
pub mod mcp;
pub mod modpacks;
pub mod nexus;
pub mod nexus_browse;
pub mod nexus_cache;
pub mod nexus_markup;
pub mod paths;
pub mod reshade;
pub mod secrets;
pub mod sources;
pub mod sso;
pub mod startup;
#[cfg(test)]
mod testutil;
pub mod ultraplus;
pub mod updates;
pub mod vdf;
pub mod winsys;

pub use error::{Error, Result};

/// Steam app id of Cyberpunk 2077.
pub const STEAM_APP_ID: &str = "1091500";
/// Nexus Mods game domain.
pub const NEXUS_GAME_DOMAIN: &str = "cyberpunk2077";
pub const APP_NAME: &str = "cp2077-modmanager";
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
