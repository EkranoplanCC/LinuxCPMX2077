// ReShade card in Installed mods: install ReShade from reshade.me (or a setup
// file the user downloaded), update it, and show where it stands in the game.
// ReShade itself is a tracked mod, so Disable and Uninstall reuse the mod
// table's actions. Loaded after app.js and built from its helpers.

let reshadeOpen = loadPref("reshadeOpen", false);
let reshadeLatest = null;
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
  const s = await invoke("reshade_status", { gameId });
  if (currentGame?.id !== gameId) return;
  box.classList.remove("hidden");
  renderReShade(s);
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

async function reshadeCheckLatest() {
  reshadeLatest = await invoke("reshade_latest");
  loadReShade();
}

function reshadeSummary(s) {
  const i = s.installed;
  if (i) return `${i.version || "?"} as ${i.dll || "?"}${i.enabled ? "" : " · disabled"}`;
  if (s.unmanaged) return `not managed: ${s.unmanaged} is ReShade copied in outside CPMX2077`;
  return "not installed";
}

function renderReShade(s) {
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
    !i && s.unmanaged ? el("p", { class: "small" },
      `bin/x64/${s.unmanaged} is ReShade installed outside CPMX2077. Installing here backs it up, and Uninstall puts it back.`) : null,
    !i ? el("p", { class: "small" }, where) : null,
    el("p", { id: "reshade-progress", class: "muted small" }, reshadeProgress),
    el("div", { class: "row" },
      newer ? el("button", { class: "update", onclick: (e) => busy(e.target, () => reshadeInstall({})) }, `Update to ${reshadeLatest}`) : null,
      i ? el("button", { title: "Read the newest version from reshade.me",
        onclick: (e) => busy(e.target, reshadeCheckLatest) }, "Check for update") : null,
      fromSite, fromFile,
      m ? el("button", { onclick: (e) => busy(e.target, () => setEnabled(m, !i.enabled)).finally(loadMods) },
        i.enabled ? "Disable" : "Enable") : null,
      m ? el("button", { class: "danger", onclick: (e) => busy(e.target, () => uninstall(m)) }, "Uninstall") : null,
      docLink("https://reshade.me/", ["Open reshade.me"])));
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
