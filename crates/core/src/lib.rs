//! Core logic for the CP2077 Linux mod manager: game detection, mod database,
//! safe archive installation and the Nexus Mods API client. The Tauri app is a
//! thin shell over this crate so everything here is testable without a GUI.

pub mod analysis;
pub mod archive;
pub mod db;
pub mod error;
pub mod fomod;
pub mod game;
pub mod hash;
pub mod install;
pub mod nexus;
pub mod paths;
pub mod secrets;
pub mod sso;
pub mod vdf;

pub use error::{Error, Result};

/// Steam app id of Cyberpunk 2077.
pub const STEAM_APP_ID: &str = "1091500";
/// Nexus Mods game domain.
pub const NEXUS_GAME_DOMAIN: &str = "cyberpunk2077";
pub const APP_NAME: &str = "cp2077-modmanager";
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
