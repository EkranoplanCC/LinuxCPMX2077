# Getting started with CPMX2077

This walks you from a fresh download to a modded Cyberpunk 2077 that starts
cleanly. It takes about fifteen minutes, most of it waiting for downloads.
For what every button does, see the [user guide](user-guide.md).

## Before you start

- Cyberpunk 2077 installed through **Steam** (Proton) or **Heroic** (GOG), and
  started at least once, so its Proton/Wine prefix exists.
- A **Nexus Mods account** (free is fine). Premium only saves clicks.
- For the Visual C++ runtime fix: `protontricks` (recommended) or
  `winetricks`, from your distribution's packages or Flatpak.

## 1. Run the app

1. Download `CPMX2077_<version>_amd64.AppImage` and `SHA256SUMS` from
   [Releases](https://github.com/EkranoplanCC/LinuxCPMX2077/releases).
2. Optionally check the download: `sha256sum -c SHA256SUMS --ignore-missing`.
3. Make it executable and start it:

   ```sh
   chmod +x CPMX2077_*_amd64.AppImage
   ./CPMX2077_*_amd64.AppImage
   ```

The app keeps its library in `~/.local/share/cp2077-modmanager/`, so you can
replace the AppImage with a newer one at any time without losing anything.

## 2. Check the game was found

The **Game** panel on the left lists every Cyberpunk 2077 install it found in
your Steam libraries (native, Flatpak and Snap Steam) and in Heroic. It shows
the game version, the Steam build and the Proton prefix.

- If nothing is found, click **Add path…** and pick the folder that contains
  `bin/x64/Cyberpunk2077.exe`.
- If you have more than one install, pick the one to mod in the drop-down.
  Everything else in the app works on the selected install.

Under **Frameworks** you see which of CET, RED4ext, redscript, ArchiveXL,
TweakXL, Codeware and REDmod are already in the game folder. Highlighted
names are installed.

## 3. Connect your Nexus account

Open **Get mods**.

1. On nexusmods.com go to *Site preferences → API Keys* and copy your
   personal API key.
2. Paste it into **Or use an API key** and click **Connect**.

The key is checked with Nexus and stored in your system keyring (GNOME
Keyring, KWallet, or another Secret Service provider), never in a file. Your
name and account type (Free or Premium) then appear at the top of the tab.

When CPMX2077 asks whether to **Send Nexus downloads to CPMX2077**, say yes
if you also want the "Mod Manager Download" buttons in your normal browser
to land in the app. You can change this later in Settings.

> "Sign in with Nexus Mods" (browser sign-in) only works once Nexus has
> registered the app. Until then, the API key is the way in.

## 4. Install the core frameworks

Most mods need some of the frameworks, so install them first.

1. In **Get mods**, switch the source at the top right to **GitHub**.
2. The featured list shows the six core frameworks. Click
   **Install missing frameworks**.

Each one is downloaded from its official GitHub release, checked against the
SHA-256 digest GitHub publishes, and installed in the right order (for
example RED4ext before the plugins that need it).

## 5. Let the app fix the Linux side

After the frameworks go in, CPMX2077 checks what they need from Proton and
offers to fix it in one go. The same items stay listed in the **Game** panel,
each with a button that fixes it and, once fixed, an **Undo** button:

| Item (button) | What the fix does |
|---|---|
| **Visual C++ runtime** (*Install vcrun2022*) | Installs `vcrun2022` into the game's prefix with protontricks (or winetricks with the game's own Proton). CET and RED4ext do not start without it. This can take a few minutes. |
| **Steam launch options** (*Set launch options*) | Sets `WINEDLLOVERRIDES="winmm,version=n,b" %command%` (plus `-modded` if you use REDmod mods). **Close Steam first**: Steam overwrites the setting while it runs. |
| **DLL overrides**, Heroic only (*Set overrides*) | Sets the same `winmm` and `version` overrides in the Heroic prefix. |
| **Folder names** (*Merge folders*) | Merges mod folders that exist under two spellings, such as `Mods` and `mods`. |

Accept the fixes. If Steam is open, the launch options fix waits; close
Steam, then click **Set launch options** in the Game panel. **Copy** next to
it copies the launch option if you would rather paste it into Steam
yourself.

## 6. Install your first mod

1. In **Get mods**, search for a mod or click **Trending**, **Most
   endorsed** and so on. Click a mod to open its page.
2. Open **Files** and pick the file you want (usually under *Main files*).
3. Then:
   - **Premium**: click **Download & install**. Done.
   - **Free account**: click **Get from Nexus**. The file's page opens in a
     Nexus window inside the app. Sign in there once, then click **Slow
     download**. The app catches the download link, fetches the file, checks
     it with Nexus, and installs it.

If the mod ships a FOMOD installer, a wizard opens and asks which options you
want, the same way it would on Windows.

You can also install an archive you already have: **Installed mods →
Install from archive…** and pick a `.zip`, `.7z` or `.rar`.

## 7. Get several mods at once

1. In **Get mods**, click **Select several**, then click each mod you want.
2. Click **Download all**.

GitHub mods and Premium downloads run straight through. With a free account,
one Nexus window steps through the file pages ("Mod 3 of 12"): click the
download button on each page and the app installs that mod in the background
while it moves on to the next. The queue bar at the top of the window shows
progress and has **Skip this mod** and **Cancel**.

Prefer a ready-made setup? The **Modpacks** tab lists Nexus collections. Open
one to see which of its mods you already have, then **Install required
mods** to queue the rest.

## 8. Start the game and check

Launch Cyberpunk 2077 from Steam or Heroic as usual. Then open the
**Diagnostics** tab in CPMX2077 and click **Check again**. It shows, at the
top, anything that needs attention: setup problems, new crash reports, mods
named in log errors, and compatibility problems between your mods.

If the game crashes or a framework does not load, see
[Troubleshooting](user-guide.md#troubleshooting).

## Next steps

- Turn mods off and on with the switch in the **On** column instead of
  uninstalling them.
- Click **Check for updates** in **Installed mods** now and then (it also
  runs at start-up).
- Read the [user guide](user-guide.md) for everything else.
