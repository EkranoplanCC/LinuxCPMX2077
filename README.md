# CP2077 Mod Manager

A Linux-native mod manager for Cyberpunk 2077 running under Proton (Steam) or
Heroic (GOG), shipped as an AppImage.

![screenshot](docs/screenshot.png)

## What works in v0.1

- **Finds the game**: scans every Steam library (native, Flatpak, Snap) and
  Heroic's GOG installs, reads the Steam build id and the executable's version,
  locates the Proton prefix, and detects CET, RED4ext, redscript, ArchiveXL,
  TweakXL, Codeware and REDmod.
- **Proton checks**: warns when CET/RED4ext are installed but the
  `WINEDLLOVERRIDES="winmm,version=n,b" %command%` launch option is missing.
- **Tracks everything** in a SQLite library (`~/.local/share/cp2077-modmanager/`):
  each mod, its source, the game build it was installed on, the archive's
  SHA-256/MD5, and every file it put into the game with its SHA-256.
- **Installs archives** (`.zip`, `.7z`; `.rar` via system `bsdtar`/`unrar`)
  the way each mod expects:
  - **FOMOD installers** run as an in-app wizard: steps, option groups and
    their pick-one/pick-any rules, recommended/required/not-usable options
    (including ones that depend on files already in the game, e.g. "needs
    CET"), condition flags, hidden steps, conditional files and priorities.
    Installer images are shown; UTF-16 XML is handled.
  - Game-root layouts (even wrapped in a folder): CET, RED4ext, redscript,
    ArchiveXL/TweakXL, `engine/` configs.
  - Standalone CET mod folders (with `init.lua`) go to
    `bin/x64/plugins/cyber_engine_tweaks/mods/`.
  - REDmod folders (`info.json` + `archives/`, `scripts/`, …) go to `mods/`,
    with a warning if the `-modded` launch option is missing.
  - Loose `.archive` / `.xl` / `.reds` / tweak files.
- **Conflicts**: refuses to overwrite another mod's files unless you allow it;
  uninstalling the winner restores the loser's copy, and original game files
  are backed up and restored.
- **Enable/disable** without uninstalling: switching a mod off takes its files
  out of the game (bringing back whatever they replaced) and keeps a checked
  copy, so switching it on again needs no re-download. Files the mod changed
  after install, such as its own settings, stay in place and are kept when it
  comes back. Disabled mods are left out of the compatibility check and crash
  analysis.
- **Compatibility check** (no AI needed): every enabled mod is indexed for
  what it touches: game resources inside `.archive` files (and which of them
  replace base-game resources), redscript `@replaceMethod`/`@wrapMethod`/
  `@addField`/`@addMethod`, TweakXL records and properties, ArchiveXL resource
  patches, CET `Override`/`Observe` hooks and RED4ext plugins. The
  Compatibility tab lists hard clashes, overlaps (with which archive wins the
  load order) and frameworks a mod needs that aren't installed.
- **Agent access (read-only MCP)**: `CP2077-Mod-Manager.AppImage --mcp`
  serves the mod list, file hashes, the compatibility index and the game's
  logs to an agent such as Claude Code
  (`claude mcp add cp2077-mods -- /path/to/AppImage --mcp`). The database is
  opened read-only, there are no tools that change or run anything, and logs
  are only readable from a fixed list of known locations.
- **Crashes & logs**: reads the game's crash reports in the Proton prefix
  (`REDEngine/ReportQueue`), the CET, RED4ext, ArchiveXL, TweakXL, Codeware and
  redscript logs (plus per-mod CET logs and Proton's `steam-1091500.log`),
  lists errors and warnings, and names the installed mod each one mentions.
- **Graph view**: mods, the frameworks they need, the game classes, tweak
  records and resources they touch, with clashes highlighted.
- **Verify**: re-hashes a mod's files and reports missing, edited or
  overridden ones.
- **Nexus Mods sign-in**: “Sign in with Nexus Mods” uses Nexus SSO, so you
  log in on nexusmods.com in your browser and the app receives an API key; your
  password never goes through the app. Pasting a personal API key works too.
  Keys live only in the system keyring (Secret Service), never in a file.
- **Nexus downloads**: look up mods and files, download directly (Premium) or
  through “Mod Manager Download” `nxm://` links (free accounts; enable in
  Settings → Handle nxm:// links).

### Nexus SSO slug

Nexus only allows browser sign-in for applications it has registered, and
identifies them by a slug. Once Nexus issues one, either build with
`NEXUS_SSO_APP_SLUG=<slug>` or enter it under Settings. Until then the API key
field is the way to connect.

## Download safety

- Archive formats are identified by magic bytes, never by extension.
- Entry names with `..`, absolute paths, drive letters, `:` or NUL are
  rejected; symlink entries are refused; extraction writes with `create_new`
  and checks every parent directory resolves inside the target.
- Entry count and *actual* decompressed bytes are capped (zip-bomb defence);
  any failure deletes the partial extraction.
- Files are deployed with atomic temp-file + rename and never written through
  symlinks in the game directory.
- Nexus downloads are only fetched over HTTPS from `nexusmods.com` /
  `nexus-cdn.com` (redirects included), must match the advertised size, and
  their MD5 must be recognised by Nexus for that exact mod and file, otherwise
  the file is discarded.

## Building

```sh
# Debian/Ubuntu
sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev libayatana-appindicator3-dev libssl-dev patchelf
cargo install tauri-cli --version "^2" --locked

cargo test --workspace
cd src-tauri && cargo tauri build --bundles appimage
```

The AppImage lands in `target/release/bundle/appimage/`. CI builds it on every
push (see `.github/workflows/build.yml`).

## Layout

- `crates/core` – all logic (detection, database, extraction, install, Nexus
  client, secrets); no GUI dependency, fully unit-tested.
- `src-tauri` – Tauri 2 shell exposing core functions as commands.
- `ui` – plain HTML/CSS/JS frontend (no build step).

## Roadmap

- Browse and search Nexus inside the app, and Nexus collections (modpacks),
  via the v2 GraphQL API
- Load order for `.archive` files
