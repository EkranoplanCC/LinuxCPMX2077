# CPMX2077

**A Linux-native mod manager for Cyberpunk 2077**, for the game running under
Proton (Steam) or Heroic (GOG). It finds your game, installs mods the way
each one expects, keeps track of every file, and tells you which mod broke
your game when something goes wrong.

[![build](https://github.com/EkranoplanCC/LinuxCPMX2077/actions/workflows/build.yml/badge.svg)](https://github.com/EkranoplanCC/LinuxCPMX2077/actions/workflows/build.yml)
[![latest release](https://img.shields.io/github/v/release/EkranoplanCC/LinuxCPMX2077?include_prereleases&label=release)](https://github.com/EkranoplanCC/LinuxCPMX2077/releases)
[![license: GPL-3.0](https://img.shields.io/badge/license-GPL--3.0-blue)](LICENSE)

![Installed mods: every mod with its category, version, source and the game version it was installed on](docs/images/installed-mods.png)

## Download

Get the AppImage (`CPMX2077_<version>_amd64.AppImage`) from
[Releases](https://github.com/EkranoplanCC/LinuxCPMX2077/releases), make it
executable (`chmod +x`) and run it. Each release has a `SHA256SUMS` file to
check the download against. A Windows installer
(`CPMX2077_<version>_x64-setup.exe`) is built too; see
[Windows](#windows-preview).

- **New here?** Follow [Getting started](docs/getting-started.md): install,
  connect Nexus, set up the frameworks and Proton, and install your first mod.
- **Reference:** the [user guide](docs/user-guide.md) explains how the app
  works and what every tab and button does, plus troubleshooting.

## Highlights

- **Installs mods properly**: FOMOD installers as an in-app wizard, REDmod,
  CET, RED4ext, redscript, ArchiveXL and TweakXL layouts, loose files. Never
  overwrites another mod's files without asking, and uninstalling puts back
  what was there before.
- **Fixes the Linux side for you**: the Visual C++ runtime in the Proton
  prefix and the Steam launch options mods need, each with an Undo.
- **Nexus Mods and GitHub in the app**: browse, download, verify and install,
  with a download queue and update checks.
- **Netrunner tab**: one place to see what's wrong. A file map of everything
  mods installed, a graph of how mods connect, a compatibility check that
  needs no AI, and crash and log analysis that names the mod behind an error.
- **Safe downloads**: Nexus files must match the checksum Nexus knows for
  them, GitHub files the SHA-256 GitHub publishes, and archives are unpacked
  with zip-bomb and path checks.

| | |
|---|---|
| ![Netrunner: a summary of crashes, log errors and compatibility problems](docs/images/netrunner.png) | ![File map: every file mods installed, by game folder, with the mod it came from and its status](docs/images/file-map.png) |
| **Netrunner** sums up what needs attention. | **File map** shows which mod owns each file. |
| ![Graph: mods, the frameworks they need and what they touch, as a flowchart with clashes in red](docs/images/graph.png) | ![Crashes and logs: the last game start step by step, with the first step that failed marked](docs/images/crashes.png) |
| **Graph** shows how mods connect and where they clash. | **Crashes & logs** walks through the last start and marks where it broke. |

Screenshots show sample data, with file locations blurred.

## What it does

- **Finds the game**: scans every Steam library (native, Flatpak, Snap) and
  Heroic's GOG installs, reads the Steam build id and the executable's version,
  locates the Proton prefix, and detects CET, RED4ext, redscript, ArchiveXL,
  TweakXL, Codeware and REDmod.
- **Linux setup, fixed for you**: checks what mods need from Proton/Wine and
  fixes it after one confirmation, each with an Undo: installs the Visual C++
  2015-2022 runtime into the prefix (`vcrun2022` via protontricks, or
  winetricks with the game's own Proton/Wine build), sets Steam's launch
  options (`WINEDLLOVERRIDES="winmm,version=n,b" %command%`, plus `-modded`
  for REDmod; only while Steam is closed), sets the same DLL overrides in a
  Heroic prefix, and merges mod folders that exist under two spellings.
  Installing a mod that needs one of these asks to set it up straight away.
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
    with `-modded` added to the launch options by the Linux setup.
  - Loose `.archive` / `.xl` / `.reds` / tweak files.
- **Conflicts**: refuses to overwrite another mod's files unless you allow it;
  uninstalling the winner restores the loser's copy, and original game files
  are backed up and restored.
- **Switching versions**: installing another version of an installed mod
  (from an archive, the Downloads tab, Nexus or GitHub) replaces it in place:
  the old files come out, the new ones go in, and a disabled mod stays
  disabled. It counts as the same mod when it comes from the same GitHub
  repository, or installs some of the same files and comes from the same
  Nexus page, has the same name apart from version numbers, or mostly
  overlaps. An optional file from the same Nexus page that shares no files
  with the main one is installed next to it.
- **Modpacks tab**: browse and search Nexus collections, open one to see
  which of its mods you have, which are missing, turned off or on another
  version, and install the missing ones through the download queue.
  Installing from a collection follows it: "Check for updates" asks Nexus
  for new revisions, and opening one shows what the new revision adds,
  changes and drops, with one button to update. Your own tags (with colors,
  as many per mod as you like) sort installed mods your way and stay with a
  mod through updates. "Export mod list" saves your mods, tags and followed
  collections to a JSON file; "Import mod list" applies its tags and offers
  to get the mods you don't have.
- **Dependencies**: "Show dependencies" in Installed mods lists what each mod
  needs indented under it, from its Nexus page and from the frameworks its
  files use (redscript for `.reds`, ArchiveXL for `.xl`, …), marked
  installed, turned off, already in the game folder or missing, with a button
  to get what's missing (or "Get missing" for all of them), plus which mods
  need it. A mod's page in Get mods marks its requirements the same way.
- **Mod page** button in Installed mods opens a mod's Nexus or GitHub page
  in Get mods.
- **Ultra+ recommendations**: with the Ultra+ path tracing mod installed, a
  card in Installed mods lists what the Ultra+ team's page recommends next to
  it and what conflicts with it, marks what you have, and queues the missing
  ones. Installed conflicts also show as known problems in Netrunner.
- **ReShade**: a card in Installed mods installs ReShade from reshade.me
  (or a setup file you downloaded), checks the DLL inside the setup before
  using it, loads it as `dxgi.dll` (or `d3d12.dll` when that name is taken),
  checks for updates, and uninstalls it without leftovers. Presets stay. On
  Linux it adds ReShade's DLL override to the launch options fix and offers
  Microsoft's shader compiler (`d3dcompiler_47`) for the prefix.
- **Enable/disable** without uninstalling: switching a mod off takes its files
  out of the game (bringing back whatever they replaced) and keeps a checked
  copy, so switching it on again needs no re-download. Files the mod changed
  after install, such as its own settings, stay in place and are kept when it
  comes back. Disabled mods are left out of the compatibility check and crash
  analysis.
- **Netrunner tab** (formerly Diagnostics): one place for overview and
  diagnosis. It opens with a summary of what needs attention (game setup
  warnings, recent crashes, mods named in log errors, compatibility
  problems), followed by the file map, the graph, the compatibility findings
  and the crash/log check.
- **File map**: a registry-editor style tree of every file mods installed,
  by game folder or by mod, showing which mod owns each file, where it is
  installed and whether it is in use, overridden by a newer mod, missing or
  switched off. “Show in graph” next to a
  finding or a crash suspect lights up the mods involved and what they share.
- **Compatibility check** (no AI needed): every enabled mod is indexed for
  what it touches: game resources inside `.archive` files (and which of them
  replace base-game resources), redscript `@replaceMethod`/`@wrapMethod`/
  `@addField`/`@addMethod`, TweakXL records and properties, ArchiveXL resource
  patches, CET `Override`/`Observe` hooks and RED4ext plugins. Netrunner
  lists hard clashes, overlaps (with which archive wins the
  load order) and frameworks a mod needs that aren't installed.
- **Agent access (read-only MCP)**: `CPMX2077_<version>_amd64.AppImage --mcp`
  serves the mod list, file hashes, the compatibility index and the game's
  logs to an agent such as Claude Code
  (`claude mcp add cp2077-mods -- /path/to/AppImage --mcp`). The database is
  opened read-only, there are no tools that change or run anything, and logs
  are only readable from a fixed list of known locations.
- **Crashes & logs**: reads the game's crash reports in the Proton prefix
  (`REDEngine/ReportQueue`), the CET, RED4ext, ArchiveXL, TweakXL, Codeware and
  redscript logs (plus per-mod CET logs and Proton's `steam-1091500.log`),
  sorts errors by game session (the last one first), names the installed mod
  each one mentions, and explains known messages in plain words. A startup
  timeline shows each stage of the last start (RED4ext, its plugins, script
  mods, CET, ArchiveXL, TweakXL, Codeware, crash) and marks the first one that
  failed. A list of known problem mods from the modding wiki's troubleshooting
  guide (`crates/core/data/known_issues.json`) is checked too.
- **Graph view**: mods, the frameworks they need, the game classes, tweak
  records and resources they touch, with clashes highlighted. "Group by"
  rings or heads the nodes by node type, mod type, tag, Nexus category or
  source.
- **Verify**: re-hashes a mod's files and reports missing, edited or
  overridden ones.
- **Nexus Mods sign-in**: “Sign in with Nexus Mods” uses Nexus SSO, so you
  log in on nexusmods.com in your browser and the app receives an API key; your
  password never goes through the app. Pasting a personal API key works too.
  Keys live only in the system keyring (Secret Service), never in a file.
- **Browse Nexus Mods in the app**: search by name, sort by best match,
  endorsements, downloads or date (paged), and open Nexus' Trending, Latest
  added and Latest updated lists. A mod's page shows its description as plain
  text, stats and files grouped as on the website (old versions folded away),
  with mods you already have marked “installed”. Click an author's name to list
  all their mods. Adult-flagged mods are hidden unless you turn them on in
  Settings.
- **Nexus downloads**: Premium accounts download and install straight from a
  mod's file list. Free accounts click “Get from Nexus”: the file's page opens
  in a Nexus window inside the app, you sign in and click “Slow download”
  there yourself, and the app catches the `nxm://` link the page sends and
  downloads, verifies and installs the file. Nexus doesn't let free accounts
  download through its API, so that one click on their page is required. “Or
  use your browser” opens the page in your normal browser instead; the app
  offers to register itself as the `nxm://` handler first (also in
  Settings). Pasting a mod URL, ID or `nxm://` link still works. The ⬇ button
  on a mod's card offers its two newest main files, and the download's
  progress shows right in Get mods. A file you already downloaded is never
  fetched twice: the app installs the copy you have.
- **GitHub as a second source**: the core frameworks (CET, RED4ext,
  redscript, ArchiveXL, TweakXL, Codeware) and many mods ship as GitHub
  releases. Switch “Get mods” to GitHub to search, or paste `owner/repo` or a
  repository URL, see its releases and download an asset. A download is only
  accepted from GitHub's own hosts and must match the SHA-256 digest GitHub
  publishes for the asset; it then goes through the same installer. Sources
  sit behind one `ModSource` interface (`crates/core/src/sources/`), so more
  can be added. Release lists are cached for 10 minutes to stay within
  GitHub's 60 requests an hour without a token, so a brand-new release can
  take that long to show up.
- **Download queue**: “Select” on the Get mods page lets you pick many mods,
  then “Download all” queues them. GitHub mods and Premium Nexus downloads run
  straight through. For a free account one Nexus window steps through the
  file pages in turn (“Mod 3 of 12”): you click the download button on each,
  and the app downloads and installs that mod in the background while moving
  on to the next page. Skip and Cancel are in the window's “Download queue”
  menu and in the queue panel. Update all uses the same queue.
- **Tags, versions and updates**: the Installed tab can be filtered by tag
  or Nexus category and sorted by name, tag/category, version or source. Downloads show
  the mod version, the game version they were downloaded on, the source and
  how the file was verified. “Check for updates” (also run at start-up) asks
  Nexus and GitHub for newer files; outdated mods get an Update button in the
  Installed and Downloads tabs, and an update replaces the old version in
  place.
- **API quota**: the app reads Nexus' `X-RL-*` rate-limit headers, shows how
  many requests are left, stops sending when Nexus says the quota is used up
  (or returns 429) until it resets, and pauses browsing when fewer than 25
  requests remain so downloads and checksum checks still work. Lists and mod
  pages are cached for a few minutes.
- **Debug terminal**: Settings → *Debug mode* opens a terminal, docked in
  the sidebar or popped out into its own window, that streams every Nexus and GitHub API request and every operation on
  your machine (downloads, checksum checks, extractions, files copied,
  backed up, moved or deleted, Linux setup fixes), with filters, Copy and
  Save…. API keys and signed download links are never shown.

### Windows (preview)

Linux is the main target; the Windows build is an early preview working
towards full support. Run `CPMX2077_<version>_x64-setup.exe` from Releases.
It installs for your user only (no admin rights) and isn't code-signed yet,
so SmartScreen asks once: More info, Run anyway. What differs from Linux:

- **Finding the game**: Steam via the registry and its library folders, GOG
  Galaxy via the registry, and the Epic Games launcher's install manifests.
  Picking the folder by hand works as on Linux.
- **No Linux setup panel**: the game runs natively, so there's no prefix,
  runtime or DLL override to fix. Install the Visual C++ runtime yourself if
  CET or RED4ext ask for it.
- **API key** is kept in Windows Credential Manager.
- **`nxm://` links**: installing doesn't take them over from Vortex or Mod
  Organizer. “Handle nxm:// links” in Settings points them at CPMX2077 (a
  per-user registry entry).
- **Crash reports** are read from `%LOCALAPPDATA%\REDEngine\ReportQueue`.
- **`.rar` archives** use the `tar.exe` that ships with Windows 10 and 11.
- The library lives in `%APPDATA%\cp2077-modmanager\`.

Not yet tested on a real Windows machine: CI builds and lints the Windows
code, but the unit tests run on Linux only.

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
- GitHub downloads are only fetched over HTTPS from `github.com` and its
  release-asset hosts and must match the asset's published SHA-256 digest.
  Older assets without a digest are marked unverified.
- The in-app Nexus window has no access to the app's commands. It only hands
  over `nxm://` links; links to other sites open in your browser.

## Building

```sh
# Debian/Ubuntu
sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev libayatana-appindicator3-dev libssl-dev patchelf
cargo install tauri-cli --version "^2" --locked

cargo test --workspace
cd src-tauri && cargo tauri build --bundles appimage
```

On Windows (with the WebView2 runtime, which Windows 10 and 11 include), run
`cargo tauri build --bundles nsis` in `src-tauri`; the installer lands in
`target/release/bundle/nsis/`.

The AppImage lands in `target/release/bundle/appimage/`. CI builds it on every
push (see `.github/workflows/build.yml`). Pushing a version tag that matches
the version in `src-tauri/tauri.conf.json` (e.g. `v0.1.0`), or running the
workflow from the Actions tab with "release" ticked, publishes the AppImage
and the Windows installer as a release with a SHA256SUMS file.

## Layout

- `crates/core` – all logic (detection, database, extraction, install, Nexus
  client, secrets); no GUI dependency, fully unit-tested.
- `src-tauri` – Tauri 2 shell exposing core functions as commands.
- `ui` – plain HTML/CSS/JS frontend (no build step).

## Roadmap

- Load order for `.archive` files
- Applying a collection's own load order and settings files
- Full Windows support

## Contributing

Bug reports and pull requests are welcome. See
[CONTRIBUTING.md](CONTRIBUTING.md): run the tests and clippy before pushing,
and update the user docs with every change users can see.

## License and disclaimer

CPMX2077 is free software under the [GNU GPL v3](LICENSE). It is a fan-made
tool and is not made by, endorsed by or affiliated with CD PROJEKT RED or
Nexus Mods. Cyberpunk 2077 is a trademark of CD PROJEKT S.A.
