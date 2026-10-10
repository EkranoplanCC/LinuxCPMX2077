# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

CPMX2077 is a mod manager for Cyberpunk 2077: Linux-native under Proton
first, with a Windows preview build. Rust (edition 2024) + Tauri 2, plain
HTML/CSS/JS frontend.

## Commands

```sh
rustup update stable                                   # CI uses latest stable
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings  # CI fails on any warning

cargo test -p cp2077mm-core install::                  # one module's tests
cargo test -p cp2077mm-core some_test_name -- --nocapture

# Run / bundle the app (needs the webkit2gtk/GTK dev packages listed in README)
cargo install tauri-cli --version "^2" --locked
cd src-tauri && cargo tauri dev
cd src-tauri && cargo tauri build --bundles appimage   # Windows: --bundles nsis

cargo run -p cp2077-modmanager -- --mcp                # read-only MCP server on stdio, no window
```

Both checks must pass before pushing.

## Architecture

- `crates/core` (`cp2077mm-core`): all logic, no GUI dependency, so
  everything is testable headless. One module per feature (`install`,
  `archive`, `fomod`, `nexus`, `nexus_browse`, `analysis`, `crash`,
  `linux_setup`, `reshade`, `modpacks`, ...). Errors go through
  `crate::Error` / `crate::Result`.
- `src-tauri` (`cp2077-modmanager`): thin shell. `main.rs` holds `AppState`
  (the `Db` behind a mutex, pending FOMOD installs, in-flight downloads) and
  the `#[tauri::command]`s registered in `generate_handler!`; larger areas
  have their own `*_cmd.rs`. Commands that touch disk or network run on a
  blocking thread. The same binary serves `--mcp` (see `core::mcp`).
- `ui/`: no build step, no framework. `index.html` loads `app.js` plus one
  script per tab (`graph.js`, `modpacks.js`, `netrunner.js`, ...), all
  calling the backend via `window.__TAURI__` `invoke(...)`. `debug.html` is
  the debug terminal window. Styling lives in `theme.css`. The CSP in
  `src-tauri/tauri.conf.json` blocks inline scripts and remote content
  except Nexus image hosts; adding a remote image host or window means
  editing the CSP or `src-tauri/capabilities/default.json`.

Things that span several files:

- **Install pipeline** (`core::install`): extract to staging
  (`archive::extract` with `Limits`), plan target paths under the game's
  `KNOWN_ROOTS`, optional FOMOD step (prepared install waits in
  `AppState.pending` for the user's choices), deploy with backups of
  overwritten game files, and record every file's hash in the db so mods can
  be verified, disabled and uninstalled.
- **Database** (`core::db`): SQLite at `paths::db_path()`. Schema is
  `CREATE TABLE IF NOT EXISTS` in `SCHEMA`; new columns go in the
  `MIGRATIONS` list (added via `ALTER TABLE` when missing), bigger reshapes
  are hand-written in `Db::init`. Existing user databases must keep opening.
- **Analysis index** (`core::analysis`): what each mod touches (archive
  resources, redscript targets, TweakXL records, CET hooks). Feeds the
  compatibility tab, the graph view and the MCP server.
- **Storage paths**: always go through `core::paths` (XDG on Linux,
  `dirs` equivalents on Windows). Secrets go through `core::secrets` (system
  keyring).
- **Bundled data**: `crates/core/data/*.json` (known problem mods,
  Ultra+ recommendations) is compiled in with `include_str!`.
- **Network tests**: no live calls. `core::testutil::serve` runs a local
  HTTP server answering from canned responses; clients take a base URL
  (e.g. `nexus::Client::with_base`). Fixtures are in
  `crates/core/tests/fixtures/`.

## Rules for changes

### Security of downloads and installs

The guarantees listed under "Download safety" in `README.md` (magic-byte
format detection, path/symlink checks, size caps, host allowlists, MD5 /
SHA-256 verification, the Nexus window having no command access) are
product requirements. Don't weaken them. New mod sources only via official
APIs or terms-permitted access; no automated or hidden download clicking
on Nexus.

### Windows

The manager must eventually run on Windows. Keep new code free of
Linux-only assumptions, or put them behind `cfg` / platform checks (see
`core::winsys`, `core::linux_setup`).

### Keep the user docs current

`docs/getting-started.md` (startup tutorial) and `docs/user-guide.md`
(reference for every tab, button and behaviour) describe what the app does
for users. Every functional change must update them in the same commit or
PR:

- New or changed buttons, tabs, settings, labels, install rules, sources,
  storage paths or safety checks: update the matching section of
  `docs/user-guide.md`.
- Anything that changes the first-run steps (game detection, Nexus sign-in,
  frameworks, Linux setup fixes, first install): update
  `docs/getting-started.md`.
- Renamed UI text: search both files for the old name.
- Keep the README's feature list in step when a feature is added or removed.

Write for players, not developers: what they see and click, in plain words,
using the exact button labels from `ui/`. Pure refactors, tests and CI
changes need no docs update.

### UI wording

Precise, terse, technical. No human-like phrasing ("the app knows / thinks /
can't tell"); say what was checked and against which source. Status-style
labels ("loading not confirmed", "manual download required").

## Releases

Version lives in `Cargo.toml` (workspace), `crates/core/Cargo.toml`,
`src-tauri/tauri.conf.json` and the two workspace entries in `Cargo.lock`;
bump all of them together. Write `docs/releases/v<version>.md` first; the
release job puts it above the install notes and generated changelog.
Pushing a `vX.Y.Z` tag that matches `tauri.conf.json`, or running the
`build` workflow with "release" ticked, publishes the AppImage, the NSIS
installer and `SHA256SUMS` (0.x versions as pre-releases). Only cut a
release when the maintainer asks for one.
