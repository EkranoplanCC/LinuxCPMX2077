// ReShade card in Installed mods: install ReShade from reshade.me (or a setup
// file the user downloaded), update it, and show where it stands in the game.
// ReShade itself is a tracked mod, so Disable and Uninstall reuse the mod
// table's actions. Loaded after app.js and built from its helpers.

let reshadeOpen = loadPref("reshadeOpen", false);
let reshadeLatest = null;
// Pack id → newest commit sha, after Check for updates.
let reshadePackLatest = {};
let reshadeProgress = "";

listen("reshade-progress", (e) => {
  const { done, total } = e.payload;
  reshadeProgress = total ? `Downloading ${fmtSize(done)} of ${fmtSize(total)}` : `Downloading ${fmtSize(done)}`;
  const p = $("#reshade-progress");
  if (p) p.textContent = reshadeProgress;
});

async function loadReShade() {
  const box = $("#reshade");
  if (!currentGame) return box.classList.add("hidden");
  const gameId = currentGame.id;
  const [s, launch, packs, presets] = await Promise.all([
    invoke("reshade_status", { gameId }),
    invoke("reshade_launch", { gameId }).catch(() => null),
    invoke("reshade_packs", { gameId }),
    invoke("reshade_presets", { gameId }),
  ]);
  if (currentGame?.id !== gameId) return;
  box.classList.remove("hidden");
  renderReShade(s, launch, packs, presets);
}

async function reshadeInstall(args) {
  reshadeProgress = "";
  try {
    const r = await invoke(args.path ? "reshade_install_file" : "reshade_install",
      { gameId: currentGame.id, version: null, dll: null, path: args.path });
    reportInstall(r, "In game, the Home key opens ReShade's overlay.");
  } finally {
    reshadeProgress = "";
    loadMods();
  }
}

// ReShade on reshade.me, and the installed packs on GitHub.
async function reshadeCheckLatest(packs) {
  const errors = [];
  try { reshadeLatest = await invoke("reshade_latest"); } catch (e) { errors.push(`reshade.me: ${e}`); }
  for (const p of packs.filter((x) => x.mod_id)) {
    try { reshadePackLatest[p.id] = (await invoke("reshade_pack_latest", { id: p.id })).sha; }
    catch (e) { errors.push(`${p.name}: ${e}`); }
  }
  if (errors.length) toast(errors.join("\n"), true);
  loadReShade();
}

async function reshadePackInstall(p) {
  reshadeProgress = "";
  try {
    const r = await invoke("reshade_pack_install", { gameId: currentGame.id, id: p.id });
    delete reshadePackLatest[p.id];
    reportInstall(r, `Shaders in bin/x64/reshade-shaders/Shaders/${p.id}`);
  } finally {
    reshadeProgress = "";
    loadMods();
  }
}

function reshadePacks(packs) {
  return el("table", { class: "req-table" }, el("tbody", {}, ...packs.map((p) => {
    const m = p.mod_id ? mods.find((x) => x.id === p.mod_id) : null;
    const latest = reshadePackLatest[p.id];
    const newer = p.commit && latest && latest !== p.commit;
    const state = !p.mod_id ? el("span", { class: "badge" }, "not installed")
      : el("span", { class: `badge ${p.enabled ? "ok" : "bad"}` }, p.enabled ? p.version || "installed" : "disabled");
    return el("tr", {},
      el("td", {}, docLink(`https://github.com/${p.repo}`, [p.name])),
      el("td", {}, state, newer ? el("span", { class: "badge bad" }, `newer commit ${latest.slice(0, 7)}`) : null),
      el("td", { class: "muted" }, p.what),
      el("td", { class: "actions" },
        !p.mod_id || newer ? el("button", { class: newer ? "update" : null,
          title: `Download the newest commit of ${p.repo} from GitHub; only shader and texture files are installed`,
          onclick: (e) => busy(e.target, () => reshadePackInstall(p)) }, newer ? "Update" : "Install") : null,
        m ? el("button", { onclick: (e) => busy(e.target, () => setEnabled(m, !p.enabled)).finally(loadMods) }, p.enabled ? "Disable" : "Enable") : null,
        m ? el("button", { class: "danger", onclick: (e) => busy(e.target, () => uninstall(m)) }, "Uninstall") : null));
  })));
}

function reshadePresets(presets) {
  if (!presets.length) {
    return el("p", { class: "muted small" }, "No presets in bin/x64. ReShade saves one there (ReShadePreset.ini) the first time you change effects in its overlay; preset mods from Nexus install there too.");
  }
  return el("table", { class: "req-table" }, el("tbody", {}, ...presets.map((p) => el("tr", {},
    el("td", {}, p.file, p.mod_name ? el("div", { class: "muted small" }, `from ${p.mod_name}`) : null),
    el("td", {}, `${p.techniques} technique${p.techniques === 1 ? "" : "s"}`),
    el("td", {}, p.missing.length
      ? [el("span", { class: "badge bad" }, `${p.missing.length} effect file${p.missing.length === 1 ? "" : "s"} missing`),
        el("div", { class: "muted small" }, p.missing.join(", "))]
      : el("span", { class: "badge ok" }, "all effect files found")),
    el("td", { class: "actions" }, p.active ? el("span", { class: "badge ok" }, "active")
      : el("button", { title: "Set PresetPath in ReShade.ini; ReShade loads it at the next game start",
        onclick: (e) => busy(e.target, async () => {
          await invoke("reshade_set_preset", { gameId: currentGame.id, file: p.file });
          toast(`ReShade loads ${p.file} at the next game start`);
          loadReShade();
        }) }, "Use"))))));
}

function reshadeSummary(s) {
  const i = s.installed;
  if (i) return `${i.version || "?"} as ${i.dll || "?"}${i.enabled ? "" : " · disabled"}`;
  if (s.unmanaged) return `not managed: ${s.unmanaged} is ReShade copied in outside CPMX2077`;
  return "not installed";
}

// Proton: ReShade's DLL override and the shader compiler (Linux only).
function reshadeProton(launch) {
  if (!launch) return null;
  const rows = [];
  if (launch.ok) {
    rows.push(el("tr", {}, el("td", {}, launch.steam ? "Launch options" : "DLL overrides"),
      el("td", {}, el("span", { class: "badge ok" }, "set"))));
  } else {
    rows.push(el("tr", {}, el("td", {}, launch.steam ? "Launch options" : "DLL overrides"),
      el("td", {}, el("span", { class: "badge bad" }, "not set"), " ",
        launch.steam ? "Steam launch options for Cyberpunk 2077 must load ReShade's DLL native first:"
          : "Set this in the launcher's environment variables for the game, or use Set overrides in the Game panel:",
        el("div", { class: "mono small" }, launch.line),
        el("div", { class: "row" },
          el("button", { onclick: async () => {
            try { await navigator.clipboard.writeText(launch.line); toast(launch.steam ? "Copied. In Steam, open Cyberpunk 2077 › Properties › Launch options and paste it." : "Copied."); }
            catch { toast(`Copy this:\n${launch.line}`, true); }
          } }, "Copy"),
          launch.steam ? el("span", { class: "muted small" }, "or Set launch options in the Game panel (Steam closed)") : null))));
  }
  if (launch.compiler !== null) {
    rows.push(el("tr", {}, el("td", {}, "Shader compiler"),
      el("td", {}, launch.compiler ? el("span", { class: "badge ok" }, "d3dcompiler_47 native")
        : [el("span", { class: "badge bad" }, "Wine builtin"), " Install d3dcompiler_47 in the Game panel; many effects don't compile without it."])));
  }
  return el("table", { class: "req-table" }, el("tbody", {}, ...rows));
}

function renderReShade(s, launch, packs, presets) {
  const i = s.installed;
  const m = i ? mods.find((x) => x.id === i.mod_id) : null;
  const newer = i && reshadeLatest && compareVersions(reshadeLatest, i.version || "0") > 0;
  const where = s.dll_choice.Ok ? `Installs to bin/x64/${s.dll_choice.Ok}.` : `No DLL name free: ${s.dll_choice.Err}.`;
  const fromSite = el("button", { class: i ? null : "primary", disabled: s.dll_choice.Ok ? null : "",
    title: "Download the newest standard ReShade setup from reshade.me, check the DLL inside it, and install it",
    onclick: (e) => busy(e.target, () => reshadeInstall({})) }, i ? "Reinstall from reshade.me" : "Install ReShade");
  const fromFile = el("button", { disabled: s.dll_choice.Ok ? null : "",
    title: "Use a ReShade_Setup_<version>.exe downloaded from reshade.me with a browser",
    onclick: (e) => busy(e.target, async () => {
      const path = await dialog.open({ title: "Choose the ReShade setup from reshade.me", filters: [{ name: "ReShade setup", extensions: ["exe"] }] });
      if (path) await reshadeInstall({ path });
    }) }, "Install from setup file…");
  const details = el("details", { open: reshadeOpen ? "" : null },
    el("summary", {}, el("b", {}, "ReShade"), el("span", { class: "muted" }, ` · ${reshadeSummary(s)}`)),
    el("p", { class: "muted small" },
      "Post-processing injector (sharpening, colour grading, presets). Downloaded from reshade.me only; ",
      "the setup's ReShade64.dll is checked as a 64-bit ReShade DLL of the expected version before install. ",
      "reshade.me publishes no checksums."),
    i ? el("table", { class: "req-table" }, el("tbody", {},
      el("tr", {}, el("td", {}, "Version"), el("td", {}, i.version || "?",
        reshadeLatest ? el("span", { class: `badge ${newer ? "bad" : "ok"}` }, newer ? `${reshadeLatest} available` : "latest") : null)),
      el("tr", {}, el("td", {}, "Loaded as"), el("td", {}, `bin/x64/${i.dll || "?"}`, " ",
        el("span", { class: `badge ${i.enabled ? "ok" : "bad"}` }, i.enabled ? "enabled" : "disabled"))),
      el("tr", {}, el("td", {}, "Setup SHA-256"), el("td", { class: "mono small" }, i.setup_sha256)))) : null,
    reshadeProton(launch),
    !i && s.unmanaged ? el("p", { class: "small" },
      `bin/x64/${s.unmanaged} is ReShade installed outside CPMX2077. Installing here backs it up, and Uninstall puts it back.`) : null,
    !i ? el("p", { class: "small" }, where) : null,
    el("p", { id: "reshade-progress", class: "muted small" }, reshadeProgress),
    el("div", { class: "row" },
      newer ? el("button", { class: "update", onclick: (e) => busy(e.target, () => reshadeInstall({})) }, `Update to ${reshadeLatest}`) : null,
      i ? el("button", { title: "Read the newest ReShade version from reshade.me and the newest commit of each installed shader pack",
        onclick: (e) => busy(e.target, () => reshadeCheckLatest(packs)) }, "Check for updates") : null,
      fromSite, fromFile,
      m ? el("button", { onclick: (e) => busy(e.target, () => setEnabled(m, !i.enabled)).finally(loadMods) },
        i.enabled ? "Disable" : "Enable") : null,
      m ? el("button", { class: "danger", onclick: (e) => busy(e.target, () => uninstall(m)) }, "Uninstall") : null,
      docLink("https://reshade.me/", ["Open reshade.me"])),
    el("h3", {}, "Shader packs"),
    el("p", { class: "muted small" },
      "From each author's GitHub repository, pinned to the newest commit. Only shader (.fx, .fxh) and texture files are installed, each pack in its own folder under bin/x64/reshade-shaders. GitHub publishes no checksums for these archives."),
    reshadePacks(packs),
    el("h3", {}, "Presets"),
    reshadePresets(presets));
  details.addEventListener("toggle", () => { reshadeOpen = details.open; savePref("reshadeOpen", reshadeOpen); });
  $("#reshade").replaceChildren(details);
}

function compareVersions(a, b) {
  const pa = String(a).split(".").map((x) => parseInt(x, 10) || 0);
  const pb = String(b).split(".").map((x) => parseInt(x, 10) || 0);
  for (let k = 0; k < Math.max(pa.length, pb.length); k++) {
    const d = (pa[k] || 0) - (pb[k] || 0);
    if (d) return d;
  }
  return 0;
}
