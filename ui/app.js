// UI for the mod manager. All data from Nexus, GitHub and other sources is
// untrusted, so nothing is ever inserted as HTML: elements are built with
// textContent only.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const dialog = window.__TAURI__.dialog;

const $ = (sel) => document.querySelector(sel);
let games = [];
let currentGame = null;
let nexusUser = null;

function el(tag, props = {}, ...children) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (k === "class") e.className = v;
    else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
    else if (v !== undefined && v !== null) e.setAttribute(k, v);
  }
  for (const c of children.flat()) {
    if (c === null || c === undefined) continue;
    e.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return e;
}

let toastTimer;
function toast(msg, isError = false) {
  const t = $("#toast");
  t.textContent = msg;
  t.className = isError ? "error" : "";
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => t.classList.add("hidden"), isError ? 9000 : 4500);
}

async function busy(button, fn) {
  if (button) button.disabled = true;
  try { return await fn(); }
  catch (e) { toast(String(e), true); }
  finally { if (button) button.disabled = false; }
}

// Per-viewer conveniences (sort order, last source). Storage may be missing.
function loadPref(key, fallback) {
  try { return JSON.parse(localStorage.getItem(key)) ?? fallback; } catch { return fallback; }
}
function savePref(key, value) {
  try { localStorage.setItem(key, JSON.stringify(value)); } catch { /* not kept */ }
}

function fmtSize(n) {
  if (!n) return "—";
  const u = ["B", "KB", "MB", "GB"];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return `${n.toFixed(i ? 1 : 0)} ${u[i]}`;
}

// ---- tabs ---------------------------------------------------------------
document.querySelectorAll("nav button").forEach((b) =>
  b.addEventListener("click", () => showTab(b.dataset.tab)));
function showTab(name) {
  document.querySelectorAll("nav button").forEach((b) => b.classList.toggle("active", b.dataset.tab === name));
  document.querySelectorAll(".tab").forEach((t) => t.classList.toggle("active", t.id === `tab-${name}`));
  if (name === "downloads") busy(null, loadDownloads);
  if (name === "nexus") busy(null, () => selectSource(currentSource));
  if (name === "netrunner") busy(null, runDiagnostics);
  if (name === "modpacks") busy(null, loadModpacks);
  if (name === "settings") refreshCacheInfo().catch(() => {});
}

// ---- game ---------------------------------------------------------------
async function detect() {
  games = await invoke("detect_games");
  const sel = $("#game-select");
  sel.replaceChildren(...games.map((g) =>
    el("option", { value: g.id }, `${g.install.store.toUpperCase()} · ${g.install.path}`)));
  if (!games.length) {
    $("#game-info").textContent = "No Cyberpunk 2077 install found. Use “Add path…” to pick the game folder.";
    currentGame = null;
  } else {
    selectGame(currentGame?.id ?? games[0].id);
  }
}

function selectGame(id) {
  if (currentGame?.id !== Number(id)) { ribbonPick = null; gameVersions = []; }
  currentGame = games.find((g) => g.id === Number(id)) || games[0];
  $("#game-select").value = currentGame.id;
  renderGame();
  loadMods();
}

// Framework list and launch option warnings change as mods go in and out,
// so they're re-read after every change rather than only at startup.
async function refreshGame() {
  if (!currentGame) return;
  const id = currentGame.id;
  games = await invoke("detect_games");
  const g = games.find((x) => x.id === id);
  if (!g || currentGame?.id !== id) return;
  currentGame = g;
  renderGame(false);
  await loadSetup(true);
}

function copyLaunchOption(line) {
  return el("button", {
    title: "Paste it in Steam: Cyberpunk 2077 › Properties › General › Launch options",
    onclick: async () => {
      try {
        await navigator.clipboard.writeText(line);
        toast("Copied. In Steam, open Cyberpunk 2077 › Properties › Launch options and paste it.");
      } catch {
        toast(`Copy this into Steam's launch options for Cyberpunk 2077:\n${line}`, true);
      }
    },
  }, "Copy");
}

function renderGame(withSetup = true) {
  const g = currentGame.install;
  $("#game-info").replaceChildren(
    el("div", {}, "Version: ", el("b", {}, g.exe_product_version || g.exe_file_version || "unknown")),
    g.build_id ? el("div", {}, "Steam build: ", el("span", { class: "mono" }, g.build_id)) : null,
    g.proton_prefix ? el("div", { class: "muted mono" }, "Prefix: " + g.proton_prefix) : null,
  );
  renderWarnings();
  $("#frameworks").replaceChildren(...g.frameworks.map((f) =>
    el("li", { class: f.installed ? "on" : "" }, f.name)));
  if (withSetup) loadSetup().catch(() => {});
}

// ---- Linux setup (runtime, DLL overrides, -modded, folder spellings) -----
// Problems that need something changed outside the mod files are fixed by
// the app after one confirmation, and each fix can be undone.
let setupChecks = [];
let setupSeen = null; // problem ids already shown for this game, this session
let setupGameId = null;

function renderWarnings() {
  const items = setupChecks.map(setupItem);
  items.push(...currentGame.install.warnings.map((w) => el("li", {}, w)));
  $("#game-warnings").replaceChildren(...items);
}

function setupItem(c) {
  const ok = c.state === "ok";
  const buttons = [];
  if (c.fix) buttons.push(el("button", { class: "primary", title: c.fix.blocked || "", onclick: (e) => runSetup(e.target, c, c.fix, "setup_fix") }, c.fix.label));
  if (c.copy) buttons.push(copyLaunchOption(c.copy));
  if (c.undo) buttons.push(el("button", { onclick: (e) => runSetup(e.target, c, c.undo, "setup_undo") }, c.undo.label));
  return el("li", { class: ok ? "ok" : "" },
    el("b", {}, ok ? `✓ ${c.title}` : c.title), " ",
    ok && !c.undo ? null : c.detail,
    c.fix?.blocked ? el("div", { class: "muted" }, c.fix.blocked) : null,
    buttons.length ? el("div", { class: "row" }, ...buttons) : null);
}

async function runSetup(button, c, action, command) {
  if (action.blocked) return toast(action.blocked, true);
  const go = await dialog.ask(action.confirm, { title: c.title, kind: "warning" });
  if (!go) return;
  await busy(button, async () => {
    if (c.id === "vc-runtime" && command === "setup_fix") toast("Installing the Visual C++ runtime. This can take a few minutes…");
    if (c.id === "d3dcompiler" && command === "setup_fix") toast("Installing d3dcompiler_47. This can take a few minutes…");
    toast(await invoke(command, { gameId: currentGame.id, id: c.id }));
  });
  await loadSetup();
}

async function loadSetup(offer = false) {
  if (!currentGame) return;
  const id = currentGame.id;
  const checks = await invoke("setup_checks", { gameId: id });
  if (currentGame?.id !== id) return;
  setupChecks = checks;
  renderWarnings();
  const problems = checks.filter((c) => c.state === "problem" && c.fix);
  if (setupGameId !== id || !setupSeen) {
    setupGameId = id;
    setupSeen = new Set(problems.map((c) => c.id));
    return;
  }
  const fresh = problems.filter((c) => !setupSeen.has(c.id));
  fresh.forEach((c) => setupSeen.add(c.id));
  if (offer && fresh.length) await offerSetup(fresh);
}

// After a mod goes in, set up what it needs in one go.
async function offerSetup(list) {
  const ready = list.filter((c) => !c.fix.blocked);
  const lines = list.map((c) => `• ${c.title}: ${c.detail}${c.fix.blocked ? `\n  (Not yet: ${c.fix.blocked})` : ""}`);
  const ask = `The mods you installed need this set up on Linux:\n\n${lines.join("\n\n")}\n\n`
    + (ready.length ? `Fix ${ready.length > 1 ? "these" : "it"} now? Each one can be undone from the game panel.` : "Use the buttons in the game panel once that's done.");
  if (!ready.length) return dialog.message(ask, { title: "Linux setup", kind: "info" });
  if (!(await dialog.ask(ask, { title: "Linux setup", kind: "warning" }))) return;
  for (const c of ready) {
    if (c.id === "vc-runtime") toast("Installing the Visual C++ runtime. This can take a few minutes…");
    if (c.id === "d3dcompiler") toast("Installing d3dcompiler_47. This can take a few minutes…");
    try { toast(await invoke("setup_fix", { gameId: currentGame.id, id: c.id })); }
    catch (e) { toast(String(e), true); }
  }
  await loadSetup();
}

$("#game-select").addEventListener("change", (e) => selectGame(e.target.value));
$("#rescan").addEventListener("click", (e) => busy(e.target, detect));
$("#add-path").addEventListener("click", (e) => busy(e.target, async () => {
  const path = await dialog.open({ directory: true, title: "Select the Cyberpunk 2077 folder" });
  if (!path) return;
  const g = await invoke("add_game_path", { path });
  await detect();
  selectGame(g.id);
}));

// ---- mods ---------------------------------------------------------------
let mods = [];
let updatesByMod = new Map(); // installed mod id -> update from check_updates
// The user's own tags and which installed mod has which (modpacks.js):
// { tags: [{ name, color }], mods: { modId: [tag names] } }.
let modTags = { tags: [], mods: {} };
let modSort = loadPref("modSort", { key: "name", dir: 1 });
const NO_CATEGORY = "__none__";

function sourceLabel(m) {
  if (m.source === "nexus" && m.nexus_mod_id) return `Nexus #${m.nexus_mod_id}`;
  if (m.source === "reshade") return "reshade.me";
  const info = sourceInfos.find((s) => s.id === m.source);
  if (info) return m.source_ref ? `${info.label} ${m.source_ref}` : info.label;
  return "Manual";
}

const SORT_KEYS = {
  name: (m) => m.name,
  // Your first tag, then Nexus' category.
  category: (m) => modTags.mods[m.id]?.[0] || m.category || "",
  version: (m) => m.version || "",
  source: (m) => sourceLabel(m),
};

function sortMods(list) {
  const key = SORT_KEYS[modSort.key] || SORT_KEYS.name;
  const cmp = (a, b) => a.localeCompare(b, undefined, { numeric: true, sensitivity: "base" });
  return [...list].sort((a, b) => {
    const [x, y] = [key(a), key(b)];
    // Mods without a value stay at the bottom either way.
    if (!x !== !y) return x ? -1 : 1;
    return modSort.dir * cmp(x, y) || cmp(a.name, b.name);
  });
}

async function loadMods() {
  if (!currentGame) return;
  mods = await invoke("list_mods", { gameId: currentGame.id });
  // Tags and dependencies (modpacks.js).
  await loadModExtras().catch((e) => toast(String(e), true));
  await loadVersionRibbon().catch(() => {});
  renderMods();
  showLoadout().catch(() => {});
  loadUltraPlus().catch(() => {});
  loadReShade().catch(() => {});
  refreshGame().catch(() => {});
}

function renderMods() {
  const sel = $("#mods-category");
  const keep = sel.value;
  const cats = [...new Set(mods.map((m) => m.category).filter(Boolean))].sort((a, b) => a.localeCompare(b));
  const own = modTags.tags;
  sel.replaceChildren(el("option", { value: "" }, "All tags and categories"),
    own.length ? el("optgroup", { label: "Your tags" },
      ...own.map((c) => el("option", { value: `tag:${c.name}` }, c.name)),
      el("option", { value: `tag:${NO_CATEGORY}` }, "No tags")) : null,
    el("optgroup", { label: "Nexus categories" },
      ...cats.map((c) => el("option", { value: c }, c)),
      cats.length && mods.some((m) => !m.category) ? el("option", { value: NO_CATEGORY }, "No category") : null));
  sel.value = [...sel.options].some((o) => o.value === keep) ? keep : "";
  const f = sel.value;
  const tagsOf = (m) => modTags.mods[m.id] || [];
  const shown = sortMods(mods.filter((m) => (!f
    || (f.startsWith("tag:") ? (f === `tag:${NO_CATEGORY}` ? !tagsOf(m).length : tagsOf(m).includes(f.slice(4)))
      : f === NO_CATEGORY ? !m.category : m.category === f)) && ribbonFilter(m)));
  document.querySelectorAll("#tab-mods th.sortable").forEach((th) => {
    th.classList.toggle("asc", th.dataset.sort === modSort.key && modSort.dir > 0);
    th.classList.toggle("desc", th.dataset.sort === modSort.key && modSort.dir < 0);
  });
  $("#mods-empty").classList.toggle("hidden", mods.length > 0);
  $("#mods-body").replaceChildren(...shown.flatMap((m) => [modRow(m), ...dependencyRows(m)]));
}

// The mod's page in Get mods (Nexus or another source); none for manual installs.
function modPageButton(m) {
  const nexus = m.source === "nexus" && m.nexus_mod_id;
  if (!nexus && !(m.source_ref && sourceInfos.some((s) => s.id === m.source))) return null;
  return el("button", {
    title: "Open this mod's page in Get mods: description, files and requirements",
    onclick: (e) => busy(e.target, async () => {
      await selectSource(nexus ? "nexus" : m.source, false);
      showTab("nexus");
      if (nexus) await showMod(m.nexus_mod_id);
      else await showSourceDetails(m.source, m.source_ref);
    }),
  }, "Mod page");
}

function modRow(m) {
  const stale = m.game_build_id && currentGame.install.build_id && m.game_build_id !== currentGame.install.build_id;
  const on = m.status === "installed";
  const up = updatesByMod.get(m.id);
  const sw = el("input", { type: "checkbox", class: "switch", title: on ? "Enabled: click to take its files out of the game" : "Disabled: click to put its files back" });
  sw.checked = on;
  sw.addEventListener("change", () => busy(sw, () => setEnabled(m, sw.checked)).finally(loadMods));
  return el("tr", { class: on ? "" : "off" },
    el("td", {}, sw),
    el("td", {}, el("b", {}, m.name), on ? null : el("span", { class: "badge" }, "disabled"),
      el("div", { class: "muted mono" }, m.archive_name)),
    el("td", {}, tagEditor(m), el("div", { class: "muted small" }, m.category || "")),
    el("td", {}, m.version || "—", up ? el("div", { class: "badge ok" }, `${up.to_stable ? "stable " : ""}${up.latest} available`) : null),
    el("td", {}, el("span", { class: "badge" }, sourceLabel(m))),
    el("td", {}, m.file_count),
    el("td", {}, m.game_version || "—",
      stale ? el("div", { class: "badge bad", title: `Current game build is ${currentGame.install.build_id}` }, "game updated since") : null),
    el("td", { class: "actions" },
      up ? el("button", {
        class: "update",
        title: up.to_stable ? `${m.version} is a pre-release (a test build). ${up.latest} is the release most mods are built against.` : "",
        onclick: (e) => busy(e.target, () => applyUpdate(up)),
      }, up.to_stable ? `Switch to stable ${up.latest}` : `Update to ${up.latest}`) : null, " ",
      modPageButton(m), " ",
      on ? el("button", { onclick: (e) => busy(e.target, () => verify(m)) }, "Verify") : null, " ",
      el("button", { class: "danger", onclick: (e) => busy(e.target, () => uninstall(m)) }, "Uninstall")),
  );
}

document.querySelectorAll("#tab-mods th.sortable").forEach((th) => th.addEventListener("click", () => {
  modSort = { key: th.dataset.sort, dir: modSort.key === th.dataset.sort ? -modSort.dir : 1 };
  savePref("modSort", modSort);
  renderMods();
}));
$("#mods-category").addEventListener("change", renderMods);

// ---- updates ------------------------------------------------------------
async function checkUpdates(quiet = false) {
  if (!currentGame) return;
  const gameId = currentGame.id;
  $("#updates-note").textContent = "Checking for updates…";
  let r;
  try {
    r = await invoke("check_updates", { gameId });
  } catch (e) {
    $("#updates-note").textContent = "";
    throw e;
  }
  if (currentGame?.id !== gameId) return;
  updatesByMod = new Map(r.updates.map((u) => [u.mod_id, u]));
  lastCheck = r;
  const note = updatesChanged();
  if ($("#tab-downloads").classList.contains("active")) loadDownloads().catch(() => {});
  if (!quiet) toast([note, ...r.errors.slice(0, 5)].join("\n"), r.errors.length > 0 && !r.updates.length);
}

let lastCheck = null;

// Redraw what depends on the known updates; returns the summary line.
function updatesChanged() {
  if (!lastCheck) return "";
  const n = updatesByMod.size;
  const parts = [n ? `${n} update${n === 1 ? "" : "s"} available`
    : lastCheck.checked ? `All ${lastCheck.checked} mod${lastCheck.checked === 1 ? "" : "s"} from Nexus or GitHub are up to date`
    : "No mods from Nexus or GitHub to check"];
  if (!nexusUser && mods.some((m) => m.source === "nexus")) parts.push("connect Nexus to check Nexus mods");
  if (lastCheck.errors.length) parts.push(`${lastCheck.errors.length} couldn't be checked`);
  $("#updates-note").textContent = parts.join(" · ");
  $("#updates-note").title = lastCheck.errors.join("\n");
  $("#update-all").classList.toggle("hidden", n < 2);
  $("#update-all").textContent = `Update all (${n})`;
  renderMods();
  return parts.join(" · ");
}
$("#check-updates").addEventListener("click", (e) => busy(e.target, () => checkUpdates()));
$("#update-all").addEventListener("click", () => busy(null, async () => {
  enqueue([...updatesByMod.values()].map(updateSeed));
  $("#update-all").classList.add("hidden");
}));

// A queue entry that replaces the installed mod with its update.
function updateSeed(u) {
  return u.source === "nexus"
    ? { kind: "nexus", name: u.name, modId: u.nexus_mod_id, fileId: u.nexus_file_id, replaces: u.mod_id }
    : { kind: "source", name: u.name, source: u.source, ref: u.source_ref, file: u.file || undefined, replaces: u.mod_id };
}

async function applyUpdate(u) {
  if (!currentGame) throw "Select a game first";
  if (u.source === "nexus") {
    if (nexusUser?.is_premium) {
      await download(u.nexus_mod_id, u.nexus_file_id, null, null, u.mod_id);
      return;
    }
    // Free account: one click in the Nexus window, through the queue.
    enqueue([updateSeed(u)]);
    return;
  }
  if (u.file) {
    await sourceDownload(u.source, u.source_ref, u.file, u.mod_id);
    return;
  }
  // The new version has several files: let the user pick the right one.
  await selectSource(u.source, false);
  showTab("nexus");
  await showSourceDetails(u.source, u.source_ref, { modId: u.mod_id, name: u.name });
}

async function setEnabled(m, on) {
  if (!on) {
    const kept = await invoke("disable_mod", { modId: m.id });
    const lines = [`Disabled ${m.name}. Its files are out of the game until you turn it back on.`];
    if (kept.length) lines.push(`Left in place because they changed after install: ${kept.join(", ")}`);
    toast(lines.join("\n"));
    return;
  }
  let r;
  try {
    r = await invoke("enable_mod", { modId: m.id, overwrite: false });
  } catch (e) {
    const msg = String(e);
    if (!msg.startsWith("file conflict:")) throw e;
    const ok = await dialog.ask(`Other enabled mods install the same files as “${m.name}”:\n\n${msg.slice(15).trim().split(", ").join("\n")}\n\nLet “${m.name}” replace them? Turning it off again puts their copies back.`,
      { title: "Enable mod", kind: "warning" });
    if (!ok) return;
    r = await invoke("enable_mod", { modId: m.id, overwrite: true });
  }
  const lines = [`Enabled ${m.name}: ${r.files_deployed} file${r.files_deployed === 1 ? "" : "s"} put back`];
  if (r.kept_in_place.length) lines.push(`Kept your changed copies of: ${r.kept_in_place.join(", ")}`);
  if (r.overwritten_mods.length) lines.push(`Overrode files from: ${[...new Set(r.overwritten_mods.map((c) => c.other_mod_name))].join(", ")}`);
  toast(lines.join("\n"));
}

async function verify(m) {
  const r = await invoke("verify_mod", { modId: m.id });
  const lines = [`${m.name}: ${r.ok} files OK`];
  if (r.missing.length) lines.push(`Missing: ${r.missing.join(", ")}`);
  if (r.modified.length) lines.push(`Changed on disk: ${r.modified.join(", ")}`);
  if (r.overridden.length) lines.push(`Overridden by other mods: ${r.overridden.join(", ")}`);
  toast(lines.join("\n"), r.missing.length + r.modified.length > 0);
}

async function uninstall(m) {
  const ok = await dialog.ask(`Uninstall “${m.name}”? Its files are removed and any game files it replaced are restored.`,
    { title: "Uninstall mod", kind: "warning" });
  if (!ok) return;
  await invoke("uninstall_mod", { modId: m.id });
  toast(`Uninstalled ${m.name}`);
  loadMods();
}

function reportInstall(r, note) {
  const lines = [`Installed ${r.name}: ${r.files_installed} files (${r.layout})`];
  if (r.replaced) lines.push(`Replaced the installed ${r.replaced}`);
  if (r.overwritten_mods.length) lines.push(`Overrode files from: ${[...new Set(r.overwritten_mods.map((c) => c.other_mod_name))].join(", ")}`);
  if (r.backed_up_game_files.length) lines.push(`Backed up ${r.backed_up_game_files.length} original game files`);
  if (r.skipped.length) lines.push(`Skipped: ${r.skipped.slice(0, 5).join(", ")}${r.skipped.length > 5 ? "…" : ""}`);
  if (note) lines.push(note);
  toast(lines.join("\n"));
}

$("#install-file").addEventListener("click", (e) => busy(e.target, async () => {
  if (!currentGame) throw "Select a game first";
  const path = await dialog.open({ title: "Choose a mod archive", filters: [{ name: "Archives", extensions: ["zip", "7z", "rar"] }] });
  if (!path) return;
  const r = await invoke("install_archive", { gameId: currentGame.id, path, name: null, overwrite: $("#overwrite").checked });
  await handleOutcome(r);
  loadMods();
}));

// ---- compatibility ------------------------------------------------------
const FRAMEWORK_NAMES = { cet: "CET", red4ext: "RED4ext", redscript: "redscript", archivexl: "ArchiveXL", tweakxl: "TweakXL", codeware: "Codeware", redmod: "REDmod" };

// Overview and diagnosis in one tab (Netrunner): a summary of what needs
// attention, the file map, the graph, the compatibility findings and the
// crash/log check. A finding or a
// crash suspect can be shown in the graph with the mods it names lit up.
async function runDiagnostics() {
  if (!currentGame) return;
  $("#diag-problems").replaceChildren(el("p", { class: "muted" }, "Checking…"));
  window.graphPending();
  window.loadFileTree();
  const [analysis, crash] = await Promise.allSettled([
    invoke("analyze_game", { gameId: currentGame.id }),
    invoke("crash_analysis", { gameId: currentGame.id }),
  ]);
  if (analysis.status === "fulfilled") {
    window.showGraph(analysis.value, crash.value);
    renderCompat(analysis.value);
  } else {
    window.graphError(String(analysis.reason));
  }
  if (crash.status === "fulfilled") renderCrash(crash.value);
  renderProblems(analysis.value, crash.value);
  const failed = [analysis, crash].filter((r) => r.status === "rejected").map((r) => String(r.reason));
  if (failed.length) toast(failed.join("\n"), true);
}
$("#run-diagnostics").addEventListener("click", (e) => busy(e.target, runDiagnostics));
document.querySelectorAll("[data-jump]").forEach((b) => b.addEventListener("click", () =>
  document.getElementById(b.dataset.jump).scrollIntoView({ behavior: "smooth", block: "start" })));

// An expandable list of exactly what a finding or graph connection covers,
// grouped by mod or file. Built when first opened: a texture pack can list
// thousands of resources.
function affectedDetails(groups, { hashes = false, label = "Show what's affected" } = {}) {
  if (!groups?.length) return null;
  const total = groups.reduce((a, g) => a + g.items.length + g.more, 0);
  if (!total) return null;
  const d = el("details", { class: "affected" }, el("summary", {}, `${label} (${total})`));
  d.addEventListener("toggle", () => {
    if (!d.open || d.dataset.filled) return;
    d.dataset.filled = "1";
    if (hashes) d.append(el("p", {}, "Game archives store each resource as a hash of its path, so resources are listed by hash under the mod archive that contains them. Modding tools such as WolvenKit can turn a hash back into a path."));
    for (const g of groups) {
      if (g.title) d.append(el("div", { class: "affected-title" }, g.title, el("span", { class: "muted" }, ` (${g.items.length + g.more})`)));
      d.append(el("ul", { class: "affected-items mono" }, ...g.items.map((i) => el("li", {}, i)),
        g.more ? el("li", { class: "muted" }, `and ${g.more} more`) : null));
    }
  });
  return d;
}

function showInGraph(modIds, label) {
  window.highlightGraph(modIds, label);
  $("#diag-graph").scrollIntoView({ behavior: "smooth", block: "start" });
}
function graphButton(modIds, label) {
  return modIds.length ? el("button", { class: "link inline", title: "Light up these mods in the graph",
    onclick: () => showInGraph(modIds, label) }, "Show in graph") : null;
}

// Mod ids of every log line that names a suspect, by suspect name.
function suspectIds(crash) {
  const ids = new Map();
  for (const i of crash.issues) i.mod_names.forEach((n, k) => {
    if (!ids.has(n)) ids.set(n, new Set());
    if (i.mod_ids[k] !== undefined) ids.get(n).add(i.mod_ids[k]);
  });
  return ids;
}

function renderProblems(analysis, crash) {
  const items = [];
  const warnings = currentGame.install.warnings;
  if (warnings.length) items.push(el("div", { class: "card finding warning" },
    el("h3", {}, "Game setup"), el("ul", { class: "warnings plain" }, ...warnings.map(warningItem))));
  if (crash) {
    const errors = crash.issues.filter((i) => i.level === "error" && i.last_session).length;
    const ids = suspectIds(crash);
    const lines = [];
    const first = crash.timeline.find((s) => s.first_problem);
    if (first) lines.push(el("li", {}, el("b", {}, "Start here: "), `${first.title}: ${first.summary} `,
      el("button", { class: "link inline", onclick: () => $("#crash-timeline").scrollIntoView({ behavior: "smooth", block: "center" }) }, "See the startup steps")));
    if (crash.session?.crashed) lines.push(el("li", {}, "The game crashed in the last session (", fmtTime(crash.latest_crash?.modified_unix), ")"));
    else if (crash.latest_crash && !crash.session) lines.push(el("li", {}, "Latest crash report: ", el("b", {}, fmtTime(crash.latest_crash.modified_unix))));
    if (errors) lines.push(el("li", {}, `${errors} error${errors === 1 ? "" : "s"} in the logs${crash.session ? " from the last session" : ""}`));
    const known = crash.known_issues.filter((k) => k.severity !== "info");
    if (known.length) lines.push(el("li", {}, `${known.length} known problem${known.length === 1 ? "" : "s"}: `, known.map((k) => k.title).join("; "), " ",
      el("button", { class: "link inline", onclick: () => $("#crash-known").scrollIntoView({ behavior: "smooth", block: "start" }) }, "See what to do")));
    for (const [name, n] of crash.suspects.slice(0, 8)) {
      const m = [...(ids.get(name) || [])];
      lines.push(el("li", {}, el("b", {}, name), ` is named in ${n} error${n === 1 ? "" : "s"} `, graphButton(m, `${name}: named in log errors`)));
    }
    if (lines.length) items.push(el("div", { class: "card finding error" }, el("h3", {}, "Crashes and errors"), el("ul", {}, ...lines)));
  }
  if (analysis) {
    const count = (sev) => analysis.findings.filter((f) => f.severity === sev);
    const errs = count("error"), warns = count("warning");
    if (errs.length || warns.length) {
      const all = [...errs, ...warns];
      const mods = [...new Set(all.flatMap((f) => f.mod_ids))];
      items.push(el("div", { class: "card finding " + (errs.length ? "error" : "warning") },
        el("h3", {}, "Compatibility"),
        el("ul", {},
          errs.length ? el("li", {}, `${errs.length} problem${errs.length === 1 ? "" : "s"}, such as a missing framework or two mods replacing the same resource or method`) : null,
          warns.length ? el("li", {}, `${warns.length} overlap${warns.length === 1 ? "" : "s"} to check`) : null),
        el("div", { class: "row" },
          el("button", { class: "link", onclick: () => $("#diag-compat").scrollIntoView({ behavior: "smooth" }) }, "See the list"),
          graphButton(mods, "Mods with compatibility findings"))));
    }
  }
  $("#diag-problems").replaceChildren(...(items.length ? items
    : [el("div", { class: "card finding ok" }, analysis && crash ? "Nothing needs attention: no crashes, log errors or conflicts found." : "Some checks failed; see the message.")]));
}

function renderCompat(r) {
  const sev = { error: "Problems", warning: "Overlaps to check", info: "Shared hooks (chained, all run)" };
  const groups = ["error", "warning", "info"].map((s) => {
    const items = r.findings.filter((f) => f.severity === s);
    if (!items.length) return null;
    return el("div", { class: `card finding ${s}` },
      el("h3", {}, `${sev[s]} (${items.length})`),
      el("ul", {}, ...items.map((f) => el("li", {}, f.message, " ", el("span", { class: "muted mono" }, f.key), " ", graphButton(f.mod_ids, f.message),
        affectedDetails(f.affected, { hashes: f.kind === "resource" })))));
  }).filter(Boolean);
  $("#findings").replaceChildren(...(groups.length ? groups
    : [el("div", { class: "card finding ok" }, r.mods.length ? "No conflicts found between your installed mods." : "No mods installed yet.")]));
  const n = (m, ...kinds) => kinds.reduce((a, k) => a + (m.counts[k] || 0), 0) || "—";
  $("#mod-summary").replaceChildren(...r.mods.map((m) => el("tr", {},
    el("td", {}, m.name),
    el("td", {}, n(m, "resource")),
    el("td", {}, m.base_overrides || "—"),
    el("td", {}, n(m, "reds_replace_method", "reds_wrap_method", "reds_add_method", "reds_add_field", "reds_replace_global", "cet_override", "cet_observe")),
    el("td", {}, n(m, "tweak_property", "xl_patch")),
    el("td", {}, m.requires.map((x) => FRAMEWORK_NAMES[x] || x).join(", ") || "—"))));
}

// ---- crashes & logs -----------------------------------------------------
function fmtTime(unix) {
  return unix ? new Date(unix * 1000).toLocaleString() : "—";
}

const STEP_ICON = { ok: "✓", warning: "!", failed: "✗", not_run: "–", not_installed: "○", unknown: "?" };

// Known problem mods and messages from the modding wiki.
function knownIssueCard(k) {
  return el("div", { class: `card finding ${k.severity}` },
    el("h3", {}, k.title),
    el("p", {}, k.explanation),
    el("p", {}, el("b", {}, "What to do: "), k.fix),
    el("div", { class: "row" },
      k.mod_names.length ? el("span", { class: "muted" }, "Mods: " + k.mod_names.join(", ")) : null,
      graphButton(k.mod_ids, k.title),
      docLink(k.link, [k.id.startsWith("ultraplus-") ? "Read more on the Ultra+ page" : "Read more on the modding wiki"])));
}

function renderTimeline(steps) {
  $("#crash-timeline").replaceChildren(...steps.map((s) => el("li", { class: `step ${s.status}` + (s.first_problem ? " first" : "") },
    el("span", { class: "step-icon", "aria-hidden": "true" }, STEP_ICON[s.status] || "?"),
    el("div", { class: "step-body" },
      el("div", {}, el("b", {}, s.title), s.version ? el("span", { class: "muted" }, ` ${s.version}`) : null,
        s.first_problem ? el("span", { class: "step-badge" }, "Start here") : null),
      el("div", {}, s.summary),
      s.mod_names.length ? el("div", {}, "Mods: ", el("b", {}, s.mod_names.join(", ")), " ", graphButton(s.mod_ids, `${s.title}: ${s.summary}`)) : null,
      s.fix ? el("div", { class: "step-fix" }, el("b", {}, "What to do: "), s.fix) : null,
      s.details.length ? el("details", {}, el("summary", {}, "Log lines"),
        el("ul", { class: "affected-items mono" }, ...s.details.map((d) => el("li", {}, d)))) : null))));
}

function issueList(items) {
  return el("ul", {}, ...items.slice(0, 200).map((i) => el("li", {},
    i.mod_names.length ? el("b", {}, i.mod_names.join(", ") + ": ") : null,
    el("span", { class: "mono" }, i.line), " ",
    el("span", { class: "muted" }, "· " + i.log + (i.time_unix ? " · " + fmtTime(i.time_unix) : "")), " ",
    graphButton(i.mod_ids, `${i.mod_names.join(", ")}: ${i.log}`),
    i.meaning ? el("div", { class: "issue-hint" }, i.meaning, i.fix ? el("span", {}, " ", el("b", {}, "What to do: "), i.fix) : null) : null)));
}

function renderCrash(r) {
  const ids = suspectIds(r);
  const current = r.issues.filter((i) => i.last_session);
  const earlier = r.issues.filter((i) => !i.last_session);
  const errors = current.filter((i) => i.level === "error");
  const warnings = current.filter((i) => i.level === "warning");
  const s = r.session;
  $("#crash-summary").replaceChildren(el("div", { class: "card finding " + (errors.length || s?.crashed ? "error" : "ok") },
    s ? el("p", {}, "Last game session: started ", el("b", {}, fmtTime(s.started_unix)), ", last log written ", el("b", {}, fmtTime(s.last_write_unix)),
      s.crashed ? el("span", {}, ", and the game ", el("b", {}, "crashed"), ".") : ".")
      : el("p", { class: "muted" }, "No launch log with a start time found, so the last session can't be separated: errors from all sessions are listed together."),
    r.latest_crash ? el("p", {}, "Latest crash report: ", el("b", {}, fmtTime(r.latest_crash.modified_unix))) : el("p", {}, "No crash reports found."),
    r.suspects.length
      ? el("p", {}, "Mods named in errors: ", ...r.suspects.flatMap(([name, n], i) => [i ? ", " : "", el("b", {}, name), ` (${n})`]), " ",
        graphButton([...new Set(r.suspects.flatMap(([name]) => [...(ids.get(name) || [])]))], "Mods named in log errors"))
      : el("p", { class: "muted" }, errors.length ? "None of the errors name an installed mod." : "No errors in the logs from the last session."),
  ));
  renderTimeline(r.timeline);
  $("#crash-known").replaceChildren(...(r.known_issues.length ? [el("h3", {}, "Known problems"), ...r.known_issues.map(knownIssueCard)] : []));
  const group = (title, items, cls) => items.length ? el("div", { class: `card finding ${cls}` },
    el("h3", {}, `${title} (${items.length})`), issueList(items)) : null;
  const label = s ? "from the last session" : "";
  const older = earlier.length ? el("details", { class: "card finding info" },
    el("summary", {}, `Errors and warnings from earlier sessions (${earlier.length})`),
    el("p", { class: "muted" }, "Written before the last game start. Not reproduced in the last session, so they may already be resolved."),
    issueList(earlier)) : null;
  $("#crash-issues").replaceChildren(...[group(`Errors ${label}`.trim(), errors, "error"), group(`Warnings ${label}`.trim(), warnings, "warning"), older].filter(Boolean));
  $("#crash-logs").replaceChildren(...r.logs.map((l) => el("tr", {},
    el("td", { class: "mono" }, el("button", { class: "link", style: "margin: 0", title: "Show this log", onclick: (e) => busy(e.target, () => showLog(l.name)) }, l.name)),
    el("td", {}, fmtSize(l.size)), el("td", {}, fmtTime(l.modified_unix)),
    el("td", { class: "actions" },
      el("button", { onclick: (e) => busy(e.target, () => showLog(l.name)) }, "View"), " ",
      el("button", { onclick: (e) => busy(e.target, () => invoke("open_log_folder", { gameId: currentGame.id, name: l.name })) }, "Open folder")))));
}

// The end of a log (the last 5000 lines), to read or copy into a bug report.
let shownLog = null;
async function showLog(name) {
  const text = await invoke("read_log", { gameId: currentGame.id, name });
  shownLog = name;
  $("#log-title").textContent = name;
  $("#log-note").textContent = text.split("\n").length >= 5000 ? "last 5000 lines" : "";
  $("#log-text").textContent = text || "(empty)";
  $("#log-viewer").classList.remove("hidden");
  $("#log-text").scrollTop = $("#log-text").scrollHeight;
}
$("#log-close").addEventListener("click", () => $("#log-viewer").classList.add("hidden"));
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") $("#log-viewer").classList.add("hidden");
});
$("#log-folder").addEventListener("click", (e) => busy(e.target, () => invoke("open_log_folder", { gameId: currentGame.id, name: shownLog })));
$("#log-copy").addEventListener("click", async () => {
  try { await navigator.clipboard.writeText($("#log-text").textContent); toast("Log copied"); }
  catch { toast("Select the text and copy it manually", true); }
});

// ---- FOMOD wizard ------------------------------------------------------
// `note` is added to the install report (e.g. that the file is unverified).
async function handleOutcome(outcome, note) {
  if (outcome.status === "installed") {
    reportInstall(outcome.report, note);
  } else if (outcome.status === "needs_choices") {
    const report = await runWizard(outcome);
    if (report) reportInstall(report, note);
  }
  loadMods();
}

const KIND_HINT = {
  SelectExactlyOne: "Pick one",
  SelectAtMostOne: "Pick one or none",
  SelectAtLeastOne: "Pick at least one",
  SelectAll: "All included",
  SelectAny: "Pick any",
};

// Resolves with an InstallReport, or null if the user cancels.
function runWizard({ token, name, fomod }) {
  const inst = fomod.installer;
  let sel = JSON.parse(JSON.stringify(fomod.defaults));
  let evaln = null;
  let pos = 0; // index into visible steps
  const imgCache = new Map();

  const modal = $("#wizard");
  modal.classList.remove("hidden");
  $("#wiz-title").textContent = name;

  return new Promise((resolve) => {
    const close = (result) => {
      modal.classList.add("hidden");
      $("#wiz-body").replaceChildren();
      resolve(result);
    };

    const visibleSteps = () => evaln.visible.map((v, i) => (v ? i : -1)).filter((i) => i >= 0);

    async function showImage(path) {
      const box = $("#wiz-image");
      if (!path) { box.replaceChildren(); return; }
      if (!imgCache.has(path)) imgCache.set(path, await invoke("fomod_image", { token, path }).catch(() => null));
      const url = imgCache.get(path);
      box.replaceChildren(url ? el("img", { src: url, alt: "" }) : "");
    }

    function describe(plugin) {
      $("#wiz-desc").textContent = plugin.description || "No description.";
      showImage(plugin.image);
    }

    async function refresh() {
      evaln = await invoke("fomod_evaluate", { token, selections: sel });
      // Drop choices that became unusable, add ones that became required.
      evaln.plugin_types.forEach((step, si) => step.forEach((types, gi) => {
        sel[si][gi] = sel[si][gi].filter((pi) => types[pi] !== "NotUsable");
        types.forEach((t, pi) => { if (t === "Required" && !sel[si][gi].includes(pi)) sel[si][gi].push(pi); });
      }));
      render();
    }

    function render() {
      const steps = visibleSteps();
      pos = Math.min(pos, steps.length - 1);
      const si = steps[pos];
      $("#wiz-steps").replaceChildren(...steps.map((s, i) =>
        el("span", { class: i === pos ? "active" : "" }, inst.steps[s].name || `Step ${i + 1}`)));
      const step = inst.steps[si];
      $("#wiz-body").replaceChildren(...(step ? step.groups.map((g, gi) => {
        const types = evaln.plugin_types[si][gi];
        const single = g.kind === "SelectExactlyOne" || g.kind === "SelectAtMostOne";
        const inputName = `g-${si}-${gi}`;
        const rows = g.plugins.map((p, pi) => {
          const t = types[pi];
          const checked = sel[si][gi].includes(pi);
          const locked = t === "NotUsable" || t === "Required" || g.kind === "SelectAll";
          const input = el("input", { type: single ? "radio" : "checkbox", name: inputName });
          input.checked = checked;
          input.disabled = locked;
          input.addEventListener("change", () => {
            if (single) sel[si][gi] = [pi];
            else if (input.checked) sel[si][gi] = [...sel[si][gi], pi];
            else sel[si][gi] = sel[si][gi].filter((x) => x !== pi);
            refresh();
          });
          const label = el("label", { class: "plugin" + (t === "NotUsable" ? " unusable" : "") },
            input, " ", p.name,
            t === "Recommended" ? el("span", { class: "badge ok" }, "recommended") : null,
            t === "Required" ? el("span", { class: "badge" }, "required") : null,
            t === "NotUsable" ? el("span", { class: "badge bad" }, "not usable") : null);
          label.addEventListener("mouseenter", () => describe(p));
          label.addEventListener("focusin", () => describe(p));
          return label;
        });
        if (g.kind === "SelectAtMostOne") {
          const none = el("input", { type: "radio", name: inputName });
          none.checked = sel[si][gi].length === 0;
          none.addEventListener("change", () => { sel[si][gi] = []; refresh(); });
          rows.push(el("label", { class: "plugin muted" }, none, " None"));
        }
        return el("fieldset", {}, el("legend", {}, g.name, " ", el("span", { class: "muted" }, KIND_HINT[g.kind] || "")), ...rows);
      }) : [el("p", {}, "This installer has no options.")]));
      const first = step?.groups[0]?.plugins[0];
      if (first) describe(first); else { $("#wiz-desc").textContent = ""; showImage(null); }
      $("#wiz-back").disabled = pos === 0;
      const last = pos >= steps.length - 1;
      $("#wiz-next").classList.toggle("hidden", last);
      $("#wiz-install").classList.toggle("hidden", !last);
    }

    $("#wiz-back").onclick = () => { pos--; render(); };
    $("#wiz-next").onclick = () => { pos++; render(); };
    $("#wiz-cancel").onclick = async () => {
      await invoke("cancel_install", { token }).catch(() => {});
      close(null);
    };
    $("#wiz-install").onclick = (e) => busy(e.target, async () => {
      const report = await invoke("finish_install", { token, selections: sel, overwrite: $("#overwrite").checked });
      close(report);
    });

    refresh().catch((e) => { toast(String(e), true); close(null); });
  });
}

// ---- nexus --------------------------------------------------------------
async function refreshNexus() {
  const s = await invoke("nexus_status");
  nexusUser = s.user;
  $("#nexus-login").classList.toggle("hidden", !!s.user);
  $("#nexus-browse").classList.toggle("hidden", !s.user);
  const box = $("#nexus-user");
  box.classList.toggle("hidden", !s.user);
  if (s.user) {
    // replaceChildren would show a null as the text "null".
    box.replaceChildren(...[
      el("b", {}, s.user.name), " ",
      el("span", { class: "badge" }, s.user.is_premium ? "Premium" : "Free account"), " ",
      s.user.is_premium ? null : el("p", { class: "muted" },
        "Nexus only lets Premium accounts download straight through apps like this one. With a free account, "
        + "“Get from Nexus” opens the file on Nexus in a CPMX2077 window instead: sign in there once, click "
        + "“Slow download” and wait for the short countdown. CPMX2077 then downloads, checks and installs the file by itself."),
    ].filter(Boolean));
    loadCategories().catch(() => {});
  }
  refreshNxmStatus().catch(() => {});
  if (s.error) toast(`Nexus: ${s.error}`, true);
}

$("#save-key").addEventListener("click", (e) => busy(e.target, async () => {
  await invoke("nexus_set_key", { key: $("#api-key").value });
  $("#api-key").value = "";
  await refreshNexus();
  toast("Connected to Nexus Mods");
  if (currentSource === "nexus" && !browse) await runBrowse(startList());
}));
async function refreshSso() {
  const slug = await invoke("nexus_sso_slug");
  $("#sso-slug").value = slug || "";
  $("#sso-login").disabled = !slug;
  $("#sso-note").textContent = slug ? "" : "Browser sign-in needs a Nexus application slug (Settings).";
}

$("#sso-login").addEventListener("click", (e) => busy(e.target, async () => {
  $("#sso-cancel").classList.remove("hidden");
  $("#sso-note").textContent = "Approve the request in your browser…";
  try {
    await invoke("nexus_sso_login");
    await refreshNexus();
    toast("Signed in to Nexus Mods");
  } finally {
    $("#sso-cancel").classList.add("hidden");
    $("#sso-note").textContent = "";
  }
}));
$("#sso-cancel").addEventListener("click", () => invoke("nexus_sso_cancel"));
$("#save-slug").addEventListener("click", (e) => busy(e.target, async () => {
  await invoke("set_nexus_sso_slug", { slug: $("#sso-slug").value });
  await refreshSso();
  toast("Saved");
}));

$("#clear-key").addEventListener("click", (e) => busy(e.target, async () => {
  await invoke("nexus_clear_key");
  await refreshNexus();
  toast("API key removed");
}));
// ---- nxm:// links from the user's browser ----------------------------------
async function refreshNxmStatus() {
  const st = await invoke("nxm_status");
  $("#nxm-status").textContent = st.registered ? "CPMX2077 handles nxm:// links."
    : st.handler ? `Right now they go to ${st.handler}.` : "No app handles nxm:// links right now.";
  $("#register-nxm").textContent = st.registered ? "Set up again" : "Handle nxm:// links";
  const ask = nexusUser && !nexusUser.is_premium && !st.registered && !loadPref("nxmPromptDismissed", false);
  $("#nxm-prompt").classList.toggle("hidden", !ask);
  return st;
}

async function registerNxm() {
  const st = await invoke("register_nxm_handler");
  savePref("nxmPromptDismissed", false);
  await refreshNxmStatus();
  if (!st.registered) throw `Your desktop still sends nxm:// links to ${st.handler || "no app"}. Set CPMX2077 as their handler in your desktop's default-apps settings.`;
  toast("Nexus downloads from your browser now come to CPMX2077");
}

$("#register-nxm").addEventListener("click", (e) => busy(e.target, registerNxm));
$("#nxm-prompt-yes").addEventListener("click", (e) => busy(e.target, registerNxm));
$("#nxm-prompt-no").addEventListener("click", () => {
  savePref("nxmPromptDismissed", true);
  $("#nxm-prompt").classList.add("hidden");
});

// Free accounts: the file's Nexus page in a CPMX2077 window, which catches
// the nxm:// link itself.
// A file already in the downloads is installed from there instead. Returns
// whether the Nexus window was opened.
async function getFromNexus(modId, fileId) {
  if (fileId && (await invoke("find_download", { key: { kind: "nexus", mod_id: modId, file_id: fileId } }))) {
    await download(modId, fileId);
    return false;
  }
  await invoke("nexus_open_in_app", { modId, fileId });
  toast("Sign in to Nexus in the new window if it asks, then click “Slow download”. CPMX2077 downloads and installs the file by itself.");
  return true;
}

// The fallback: the user's normal browser, which needs the nxm:// handler.
async function openInBrowser(modId, fileId) {
  const st = await invoke("nxm_status");
  if (!st.registered) {
    const ok = await dialog.ask(
      `Your browser sends Nexus downloads to ${st.handler ? `“${st.handler}”` : "no app"} right now, so they wouldn't reach CPMX2077. `
      + "Make CPMX2077 handle them?\n\nCPMX2077's own Nexus window works without this.",
      { title: "Nexus downloads", kind: "info" });
    if (ok) await registerNxm();
  }
  await invoke("nexus_open_page", { modId, fileId });
}

$("#nexus-go").addEventListener("click", (e) => busy(e.target, async () => {
  const q = $("#nexus-query").value;
  const nexusRef = await invoke("nexus_resolve", { input: q });
  if (nexusRef?.kind === "mod") return showMod(nexusRef.mod_id);
  if (nexusRef?.kind === "collection") return showCollection(nexusRef.slug, nexusRef.revision);
  if (q.trim().startsWith("nxm://")) return handleNxm(q.trim());
  const ref = await invoke("source_resolve", { input: q });
  if (!ref) throw "Enter a Cyberpunk 2077 mod or collection link, a mod ID, or a GitHub repository link";
  await selectSource(ref.source, false);
  await showSourceDetails(ref.source, ref.id);
}));

// ---- nexus browsing -----------------------------------------------------
// Nexus answers at most 80 mods per request and pages up to offset 100,000.
const PER_PAGE_CHOICES = [10, 20, 40, 80];
const MAX_OFFSET = 100000;
let perPage = PER_PAGE_CHOICES.includes(loadPref("nexusPerPage", 20)) ? loadPref("nexusPerPage", 20) : 20;
let browse = null; // { text, sort, offset, category, uploaderId, uploaderName }
let browseReq = 0;

function startList() {
  return { text: "", sort: "trending", offset: 0, category: selectedCategory() };
}

function fmtAgo(unix) {
  const s = Math.max(0, Math.round(Date.now() / 1000 - unix));
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.round(s / 60)} min ago`;
  if (s < 86400) return `${Math.round(s / 3600)} h ago`;
  return `${Math.round(s / 86400)} days ago`;
}

// "Saved copy" / "updated 3 min ago" next to a list or mod page.
function freshness(fetchedAt, saved) {
  if (!fetchedAt) return "";
  return saved ? `Nexus couldn't be reached: saved copy from ${fmtAgo(fetchedAt)}` : `from Nexus ${fmtAgo(fetchedAt)}`;
}

function fmtCount(n) {
  if (n === null || n === undefined) return "—";
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)}M`;
  if (n >= 1e4) return `${Math.round(n / 1e3)}k`;
  return n.toLocaleString();
}

function fmtDate(unix) {
  return unix ? new Date(unix * 1000).toLocaleDateString() : "—";
}

// Only Nexus' image CDNs are allowed (the CSP enforces the same).
function nexusImage(url, cls) {
  const ok = typeof url === "string" && ["https://staticdelivery.nexusmods.com/", "https://media.nexusmods.com/"].some((h) => url.startsWith(h));
  if (!ok) return el("div", { class: `${cls} noimg` });
  return el("img", { class: cls, src: url, alt: "", loading: "lazy", referrerpolicy: "no-referrer" });
}

async function installedNexusIds() {
  if (!currentGame) return new Set();
  const mods = await invoke("list_mods", { gameId: currentGame.id }).catch(() => []);
  return new Set(mods.filter((m) => m.nexus_mod_id).map((m) => m.nexus_mod_id));
}

async function refreshQuota() {
  const r = await invoke("nexus_rate");
  const left = Math.max(r.hourly_remaining ?? -1, r.daily_remaining ?? -1);
  $("#nexus-quota").textContent = left >= 0 ? `${left.toLocaleString()} API requests left` : "";
  $("#nexus-quota").title = r.daily_remaining !== null && r.daily_remaining !== undefined
    ? `Daily: ${r.daily_remaining}/${r.daily_limit}, hourly: ${r.hourly_remaining}/${r.hourly_limit}` : "";
}

// Nexus categories: id -> { category_id, name, parent }.
let categoriesById = new Map();

async function loadCategories() {
  if (categoriesById.size) return;
  const cats = await invoke("nexus_categories");
  categoriesById = new Map(cats.map((c) => [c.category_id, c]));
  // Nexus files every category under one catch-all named after the game.
  const roots = cats.filter((c) => !c.parent);
  const umbrella = roots.length === 1 ? roots[0].category_id : null;
  const top = cats.filter((c) => c.category_id !== umbrella && (!c.parent || c.parent === umbrella || !categoriesById.has(c.parent)));
  const sel = $("#nexus-category");
  const keep = sel.value;
  sel.replaceChildren(el("option", { value: "" }, "All categories"), ...top.flatMap((c) => [
    el("option", { value: c.category_id }, c.name),
    ...cats.filter((k) => k.parent === c.category_id).map((k) => el("option", { value: k.category_id }, `   ${k.name}`)),
  ]));
  sel.value = keep;
}

// The selected category and its subcategories.
function categoryIds(id) {
  if (!id) return null;
  return new Set([id, ...[...categoriesById.values()].filter((c) => c.parent === id).map((c) => c.category_id)]);
}

const SORT_TITLES = {
  trending: "Trending: most downloaded of the mods added in the last two weeks",
  endorsements: "Most endorsed", downloads: "Most downloaded",
  updated: "Latest updated", created: "Latest added", relevance: "Best match",
};

async function runBrowse(next, refresh = false) {
  browse = next;
  const req = ++browseReq;
  document.querySelectorAll("#nexus-browse .chips button").forEach((b) =>
    b.classList.toggle("active", !next.text && !next.uploaderId && b.dataset.sort === next.sort));
  if (!next.uploaderId) $("#author-head").classList.add("hidden");
  $("#nexus-result").classList.add("hidden");
  $("#nexus-list").classList.remove("hidden");
  $("#nexus-list-title").textContent = "Loading…";
  $("#nexus-fresh").textContent = "";
  try {
    const category = categoriesById.get(next.category);
    const [page, installed, author] = await Promise.all([
      invoke("nexus_search", { query: {
        text: next.text, sort: next.sort, offset: next.offset, count: perPage, category: category?.name ?? null,
        uploader_id: next.uploaderId ?? null, refresh,
      } }),
      installedNexusIds(),
      next.uploaderId ? invoke("nexus_author", { memberId: next.uploaderId }).catch(() => null) : null,
    ]);
    if (req !== browseReq) return;
    if (next.uploaderId) renderAuthorHead(author, next.uploaderName, page.total);
    renderPage(page, installed);
  } catch (e) {
    if (req === browseReq) $("#nexus-list-title").textContent = "";
    throw e;
  } finally {
    refreshQuota().catch(() => {});
    refreshDebug().catch(() => {});
  }
}

// The last page Nexus will hand out for this list.
function lastOffset(total) {
  return Math.min(MAX_OFFSET, Math.max(0, Math.ceil(total / perPage) - 1) * perPage);
}

function renderPage(page, installed) {
  let title = browse.uploaderId ? `Mods by ${browse.uploaderName || "this author"}, ${SORT_TITLES[browse.sort].toLowerCase()} first`
    : browse.text ? `Results for “${browse.text}”` : SORT_TITLES[browse.sort];
  const category = categoriesById.get(browse.category);
  if (category) title += ` in ${category.name}`;
  if (page.total !== null && page.total !== undefined) title += ` · ${page.total.toLocaleString()} mods`;
  if (page.hidden_adult) title += ` · ${page.hidden_adult} adult ${page.hidden_adult === 1 ? "mod" : "mods"} hidden (Settings)`;
  $("#nexus-list-title").textContent = title;
  $("#nexus-fresh").textContent = freshness(page.fetched_at, page.saved_copy);
  $("#nexus-fresh").classList.toggle("warn", !!page.saved_copy);
  $("#nexus-refresh").classList.remove("hidden");
  $("#nexus-grid").replaceChildren(...page.mods.map((m) => modCard(m, installed.has(m.mod_id))));
  if (!page.mods.length) $("#nexus-grid").append(el("p", { class: "muted" }, "No mods found."));
  const paged = page.total !== null && page.total !== undefined && page.total > perPage;
  $("#nexus-pager").classList.toggle("hidden", !paged);
  if (paged) {
    const pageNo = Math.floor(page.offset / perPage) + 1;
    const pages = Math.floor(lastOffset(page.total) / perPage) + 1;
    $("#nexus-page-input").value = pageNo;
    $("#nexus-page-input").max = pages;
    $("#nexus-page").textContent = `of ${pages.toLocaleString()}`;
    $("#nexus-page").title = pages * perPage < page.total ? `Nexus only pages through the first ${MAX_OFFSET.toLocaleString()} mods of a list` : "";
    $("#nexus-first").disabled = $("#nexus-prev").disabled = page.offset <= 0;
    $("#nexus-next").disabled = $("#nexus-last").disabled = page.offset >= lastOffset(page.total);
    browse.total = page.total;
  }
}

function goToPage(n) {
  const offset = Math.min(lastOffset(browse.total ?? 0), Math.max(0, (Math.round(n) - 1) * perPage));
  return runBrowse({ ...browse, offset });
}

function modCard(m, isInstalled) {
  const seed = { kind: "nexus", name: m.name, modId: m.mod_id };
  const card = el("div", { class: cardClass(seed), title: m.name, role: "button", tabindex: "0",
    onclick: () => selectMode ? toggleSelected(seed, card) : busy(null, () => showMod(m.mod_id)),
    onkeydown: (e) => {
      if (e.target === card && (e.key === "Enter" || e.key === " ")) { e.preventDefault(); card.click(); }
    } },
    el("div", { class: "thumb-wrap" },
      nexusImage(m.picture_url, "thumb"),
      el("button", { class: "quick-dl", title: "Download: pick one of the mod's main files", "aria-label": "Download options",
        onclick: (e) => { e.stopPropagation(); busy(e.currentTarget, () => toggleQuickMenu(m, card)); } }, "⬇")),
    el("div", { class: "mod-card-body" },
      el("div", { class: "mod-card-title" }, m.name),
      el("div", { class: "muted small" }, "by ", authorLink(m.author || m.uploader, m.uploader_id, m.uploader),
        m.version ? ` · v${m.version}` : ""),
      el("div", { class: "summary" }, m.summary || ""),
      el("div", { class: "stats" },
        el("span", { title: "Endorsements" }, `♥ ${fmtCount(m.endorsements)}`),
        el("span", { title: "Downloads" }, `⬇ ${fmtCount(m.downloads)}`),
        el("span", { title: "Last updated" }, fmtDate(m.updated)),
        categoriesById.has(m.category_id) ? el("span", { class: "badge" }, categoriesById.get(m.category_id).name) : null,
        isInstalled ? el("span", { class: "badge ok" }, "installed") : null,
        m.adult ? el("span", { class: "badge bad" }, "adult") : null)));
  // A download from this card that is still running when the page is drawn again.
  for (const [key, bar] of cardBars) if (key.startsWith(`${m.mod_id}:`)) card.querySelector(".thumb-wrap").append(bar);
  return card;
}

// "by <author>": opens every mod the uploader has on Nexus.
function authorLink(label, memberId, uploader) {
  if (!memberId) return label || "unknown";
  const name = uploader || label;
  return el("button", { class: "link author", title: `All mods by ${name}`, onclick: (e) => {
    e.stopPropagation();
    busy(null, () => showAuthor(memberId, name));
  } }, label || name);
}

function showAuthor(memberId, name) {
  $("#nexus-search").value = "";
  const sort = ["relevance", "trending"].includes(browse?.sort) || !browse ? "downloads" : browse.sort;
  return runBrowse({ text: "", sort, offset: 0, category: null, uploaderId: memberId, uploaderName: name });
}

function renderAuthorHead(a, name, total) {
  const head = $("#author-head");
  const facts = [
    total !== null && total !== undefined ? `${total.toLocaleString()} Cyberpunk 2077 mod${total === 1 ? "" : "s"}` : null,
    a?.mod_count ? `${a.mod_count.toLocaleString()} on all of Nexus` : null,
    a?.unique_downloads ? `${fmtCount(a.unique_downloads)} unique downloads` : null,
    a?.joined ? `joined ${fmtDate(a.joined)}` : null,
  ].filter(Boolean).join(" · ");
  head.replaceChildren(
    el("h3", {}, `Mods by ${a?.name || name || "this author"}`),
    a?.recognized ? el("span", { class: "badge ok", title: "Nexus' Recognised author mark" }, "recognised author") : null,
    el("span", { class: "muted small" }, facts),
    el("span", { class: "spacer" }),
    el("label", { class: "muted small" }, "Sort ",
      el("select", { class: "inline", onchange: (e) => busy(null, () => runBrowse({ ...browse, sort: e.target.value, offset: 0 })) },
        ...[["downloads", "Most downloaded"], ["endorsements", "Most endorsed"], ["updated", "Latest updated"], ["created", "Latest added"]]
          .map(([v, l]) => el("option", { value: v, selected: browse.sort === v ? "" : null }, l)))),
    el("button", { onclick: (e) => busy(e.target, () => runBrowse(startList())) }, "Back to all mods"));
  head.classList.remove("hidden");
}

// ---- downloading straight from Get mods -----------------------------------
// Downloads started on this page stay on it: a bar on the mod's card and one
// at the top of the page show how far they've got.
const cardBars = new Map(); // "modId:fileId" -> the bar on that mod's card
const fromCard = new Set(); // "modId:fileId" opened in the Nexus window from a card

// Every Nexus file in the downloads that is still on disk, as "modId:fileId".
async function downloadedNexusFiles() {
  const groups = await invoke("list_download_groups").catch(() => []);
  return new Set(groups.flatMap((g) => g.entries)
    .filter((e) => e.on_disk && e.nexus_mod_id && e.nexus_file_id).map((e) => `${e.nexus_mod_id}:${e.nexus_file_id}`));
}

async function toggleQuickMenu(m, card) {
  const wrap = card.querySelector(".thumb-wrap");
  const open = wrap.querySelector(".quick-menu");
  if (open) return open.remove();
  document.querySelectorAll(".quick-menu").forEach((x) => x.remove());
  const [files, have] = await Promise.all([invoke("nexus_quick_files", { modId: m.mod_id }), downloadedNexusFiles()]);
  refreshQuota().catch(() => {});
  refreshDebug().catch(() => {});
  const label = nexusUser?.is_premium ? "Download & install" : "Get from Nexus";
  const menu = el("div", { class: "quick-menu", onclick: (e) => e.stopPropagation(), onkeydown: (e) => e.stopPropagation() },
    ...files.map((f) => el("button", { title: `${label}: ${f.file_name}`, onclick: (e) => {
      menu.remove();
      busy(null, () => quickDownload(m, f, card));
    } },
      el("b", {}, f.name || f.file_name),
      el("span", { class: "muted small" }, [f.version && `v${f.version}`, f.size_in_bytes && fmtSize(f.size_in_bytes),
        fmtDate(f.uploaded_timestamp), have.has(`${m.mod_id}:${f.file_id}`) ? "already downloaded" : null].filter(Boolean).join(" · ")))),
    files.length ? null : el("p", { class: "muted small" }, "No main files. Open the mod to pick one."),
    el("button", { class: "link small", onclick: () => { menu.remove(); busy(null, () => showMod(m.mod_id)); } }, "All files…"));
  wrap.append(menu);
}

async function quickDownload(m, f, card) {
  if (!currentGame) throw "Select a game first";
  const key = `${m.mod_id}:${f.file_id}`;
  if (cardBars.has(key)) throw `${f.name || f.file_name} is already downloading`;
  const bar = el("div", { class: "card-progress", onclick: (e) => e.stopPropagation() }, el("div", { class: "progress-bar" }), el("span", { class: "progress-text" }, "Starting…"));
  card.querySelector(".thumb-wrap").append(bar);
  cardBars.set(key, bar);
  if (nexusUser?.is_premium) return download(m.mod_id, f.file_id);
  bar.querySelector(".progress-text").textContent = "Waiting for “Slow download” in the Nexus window";
  fromCard.add(key);
  let opened = false;
  try {
    opened = await getFromNexus(m.mod_id, f.file_id);
  } finally {
    if (!opened) {
      fromCard.delete(key);
      endCardBar(key);
    }
  }
}

// The Nexus window closed without a download: drop the waiting bars.
listen("nexus-window-closed", () => {
  for (const key of fromCard) endCardBar(key);
  fromCard.clear();
});
document.addEventListener("click", () => document.querySelectorAll(".quick-menu").forEach((x) => x.remove()));

function endCardBar(key) {
  cardBars.get(key)?.remove();
  cardBars.delete(key);
}

function selectedCategory() {
  return Number($("#nexus-category").value) || null;
}

function searchFromInputs(offset = 0) {
  if (!$("#nexus-search").value.trim() && $("#nexus-sort").value === "relevance") $("#nexus-sort").value = "endorsements";
  return { text: $("#nexus-search").value.trim(), sort: $("#nexus-sort").value, offset, category: selectedCategory() };
}

$("#nexus-search-go").addEventListener("click", (e) => busy(e.target, () => runBrowse(searchFromInputs())));
$("#nexus-search").addEventListener("keydown", (e) => {
  if (e.key === "Enter") busy(null, () => runBrowse(searchFromInputs()));
});
document.querySelectorAll("#nexus-browse .chips button").forEach((b) => b.addEventListener("click", () => busy(b, () => {
  $("#nexus-search").value = "";
  $("#nexus-sort").value = b.dataset.sort;
  return runBrowse({ text: "", sort: b.dataset.sort, offset: 0, category: selectedCategory() });
})));
$("#nexus-category").addEventListener("change", () => busy(null, () => runBrowse(searchFromInputs())));
$("#nexus-first").addEventListener("click", (e) => busy(e.target, () => goToPage(1)));
$("#nexus-prev").addEventListener("click", (e) => busy(e.target, () => goToPage(browse.offset / perPage)));
$("#nexus-next").addEventListener("click", (e) => busy(e.target, () => goToPage(browse.offset / perPage + 2)));
$("#nexus-last").addEventListener("click", (e) => busy(e.target, () => goToPage(Infinity)));
$("#nexus-page-input").addEventListener("change", (e) => {
  const n = Number(e.target.value);
  if (Number.isFinite(n) && n >= 1) busy(null, () => goToPage(n));
});
$("#nexus-refresh").addEventListener("click", (e) => busy(e.target, () => runBrowse(browse, true)));
$("#nexus-per-page").value = String(perPage);
$("#nexus-per-page").addEventListener("change", (e) => {
  const first = browse ? browse.offset : 0;
  perPage = Number(e.target.value) || 20;
  savePref("nexusPerPage", perPage);
  // Stay around the same mods: the page that holds the first one shown.
  if (browse) busy(null, () => runBrowse({ ...browse, offset: Math.floor(first / perPage) * perPage }));
});

$("#show-adult").addEventListener("change", (e) => busy(null, async () => {
  await invoke("set_nexus_show_adult", { show: e.target.checked });
  if (browse) await runBrowse(browse);
}));

// ---- debug view: every Nexus API request --------------------------------
let debugMode = loadPref("nexusDebug", false) === true;
let debugRequests = []; // newest last
let debugTimer = null;
const SERVED_LABELS = { network: "Nexus", cache: "cache", saved: "saved copy", held: "held back" };

async function refreshDebug() {
  if (!debugMode) return;
  const after = debugRequests.length ? debugRequests[debugRequests.length - 1].id : 0;
  const fresh = await invoke("nexus_requests", { after });
  if (!fresh.length && debugRequests.length) return;
  debugRequests = debugRequests.concat(fresh).slice(-300);
  renderDebug();
}

function renderDebug() {
  const reqs = debugRequests;
  const sent = reqs.filter((r) => r.served === "network");
  const avg = sent.length ? Math.round(sent.reduce((a, r) => a + r.duration_ms, 0) / sent.length) : 0;
  const counts = Object.keys(SERVED_LABELS).map((k) => [k, reqs.filter((r) => r.served === k).length]).filter(([, n]) => n);
  $("#nexus-debug-summary").textContent = reqs.length
    ? `· ${counts.map(([k, n]) => `${n} ${SERVED_LABELS[k]}`).join(", ")}${sent.length ? ` · ${avg} ms average` : ""}`
    : "· none yet";
  const slowest = Math.max(1000, ...reqs.map((r) => r.duration_ms));
  $("#nexus-debug-list").replaceChildren(...reqs.slice().reverse().map((r) => {
    const failed = r.error || (r.status && r.status >= 400);
    const bar = el("div", { class: "req-bar" });
    bar.style.width = `${Math.max(2, (100 * r.duration_ms) / slowest)}%`;
    const quota = [r.hourly_remaining, r.daily_remaining].filter((n) => n !== null && n !== undefined);
    return el("details", { class: `req ${r.served}${failed ? " failed" : ""}` },
      el("summary", {},
        el("span", { class: "mono muted" }, new Date(r.at_ms).toLocaleTimeString()),
        el("span", { class: `badge req-${r.served}` }, SERVED_LABELS[r.served] || r.served),
        el("span", { class: "mono" }, `${r.method} ${r.label?.split(" · ")[0] || r.endpoint}`),
        el("span", { class: "req-time" }, bar, el("span", { class: "muted small" }, r.served === "network" ? `${r.duration_ms} ms` : "")),
        el("span", { class: failed ? "badge bad" : "muted small" }, r.status ? String(r.status) : r.error ? "error" : "")),
      el("dl", { class: "req-detail" },
        ...[
          ["Endpoint", r.endpoint],
          ["What", r.label],
          ["Answered by", SERVED_LABELS[r.served] || r.served],
          ["Priority", r.priority === "browse" ? "browsing (pauses when the quota runs low)" : "essential (downloads, sign-in)"],
          ["Status", r.status],
          ["Time", r.served === "network" ? `${r.duration_ms} ms` : null],
          ["Size", r.bytes ? fmtSize(r.bytes) : null],
          ["Requests left", quota.length ? `hourly ${r.hourly_remaining ?? "?"}, daily ${r.daily_remaining ?? "?"}` : null],
          ["Error", r.error],
        ].filter(([, v]) => v !== null && v !== undefined && v !== "").flatMap(([k, v]) => [el("dt", {}, k), el("dd", { class: "mono" }, String(v))])));
  }));
}

function setDebugMode(on) {
  debugMode = on;
  savePref("nexusDebug", on);
  $("#nexus-debug-mode").checked = on;
  $("#nexus-debug").classList.toggle("hidden", !on);
  $("#debug-terminal-side").classList.toggle("hidden", !on);
  if (!on) showDock(false);
  clearInterval(debugTimer);
  debugTimer = null;
  if (on) {
    // Downloads and update checks call Nexus too; pick those up as they happen.
    debugTimer = setInterval(() => {
      if ($("#nexus-debug").open && !document.hidden) refreshDebug().catch(() => {});
    }, 2000);
    refreshDebug().catch(() => {});
  }
}

// ---- debug terminal: docked in the sidebar (default) or its own window ---
let debugPlace = loadPref("debugPlace", "dock") === "window" ? "window" : "dock";
let dockOpen = false;

function showDock(on) {
  dockOpen = on;
  const box = $("#debug-dock");
  box.classList.toggle("hidden", !on);
  $("#sidebar").classList.toggle("has-dock", on);
  $("#debug-terminal-side").classList.toggle("on", on);
  // A fresh frame reads the whole log again; a hidden one isn't kept polling.
  if (on && !box.querySelector("iframe")) box.append(el("iframe", { src: "debug.html?docked", title: "Debug terminal", allow: "clipboard-write" }));
  if (!on) box.replaceChildren();
}

async function popOutDebugTerminal() {
  try {
    await invoke("open_debug_terminal");
    showDock(false);
  } catch (e) {
    toast(String(e), true);
  }
}

function openDebugTerminal() {
  if (debugPlace === "window") popOutDebugTerminal();
  else {
    showDock(true);
    savePref("debugDockHidden", false);
  }
}

// Called by the docked terminal's Pop out and Hide buttons.
window.cpmxDebugTerminal = {
  popOut: popOutDebugTerminal,
  hide: () => {
    showDock(false);
    savePref("debugDockHidden", true);
  },
};
listen("debug-dock", () => {
  showDock(true);
  savePref("debugDockHidden", false);
});

$("#nexus-debug-mode").addEventListener("change", (e) => {
  setDebugMode(e.target.checked);
  if (e.target.checked) openDebugTerminal();
});
$("#debug-place").value = debugPlace;
$("#debug-place").addEventListener("change", (e) => {
  debugPlace = e.target.value;
  savePref("debugPlace", debugPlace);
});
for (const b of document.querySelectorAll("button.debug-open")) b.addEventListener("click", openDebugTerminal);
$("#debug-terminal-side").addEventListener("click", () => {
  if (debugPlace === "dock" && dockOpen) window.cpmxDebugTerminal.hide();
  else openDebugTerminal();
});
$("#nexus-debug-clear").addEventListener("click", (e) => busy(e.target, async () => {
  await invoke("nexus_clear_requests");
  debugRequests = [];
  renderDebug();
}));
setDebugMode(debugMode);
if (debugMode && debugPlace === "dock" && loadPref("debugDockHidden", false) !== true) showDock(true);

async function refreshCacheInfo() {
  const c = await invoke("nexus_cache_info");
  $("#nexus-cache-info").textContent = c.files ? `${c.files.toLocaleString()} saved, ${fmtSize(c.bytes)}` : "Nothing saved yet";
}
$("#nexus-cache-clear").addEventListener("click", (e) => busy(e.target, async () => {
  await invoke("nexus_clear_cache");
  await refreshCacheInfo();
  toast("Saved Nexus pages cleared");
}));

const FILE_GROUPS = [
  ["MAIN", "Main files"], ["UPDATE", "Updates"], ["OPTIONAL", "Optional files"],
  ["MISCELLANEOUS", "Miscellaneous"], ["OLD_VERSION", "Old versions"],
];

// ---- mod descriptions -------------------------------------------------------
// The core turns Nexus' BBCode into a tree of known elements; this builds it
// with createElement, checking every tag again. Nothing is parsed as HTML.
const DOC_TAGS = {
  b: "b", i: "i", u: "u", s: "s", sup: "sup", sub: "sub", h2: "h3", h3: "h4", h4: "h5", quote: "blockquote",
  code: "pre", ul: "ul", ol: "ol", li: "li", p: "p", div: "div", span: "span", table: "table", tr: "tr", td: "td", th: "th",
};
const SAFE_COLOR = /^(#[0-9a-f]{3}|#[0-9a-f]{6}|[a-z]{3,20})$/;

function renderDoc(nodes, depth = 0) {
  if (!Array.isArray(nodes) || depth > 60) return [];
  return nodes.map((n) => renderDocNode(n, depth)).filter(Boolean);
}

function renderDocNode(n, depth) {
  switch (n?.t) {
    case "text": return document.createTextNode(String(n.text));
    case "br": return el("br");
    case "hr": return el("hr");
    case "link": return docLink(n.href, renderDoc(n.children, depth + 1));
    case "video": return docLink(n.url, ["▶ Video on YouTube"]);
    case "image": return docImage(n);
    case "el": {
      const kids = renderDoc(n.children, depth + 1);
      if (n.tag === "spoiler") return el("details", { class: "spoiler" }, el("summary", {}, "Spoiler"), ...kids);
      if (n.tag === "center" || n.tag === "right") return el("div", { class: `align-${n.tag}` }, ...kids);
      if (n.tag === "size") return el("span", { class: `size-${Math.min(7, Math.max(1, Number(n.size) | 0))}` }, ...kids);
      if (n.tag === "color") {
        const span = el("span", {}, ...kids);
        if (typeof n.color === "string" && SAFE_COLOR.test(n.color)) span.style.color = n.color;
        return span;
      }
      return el(DOC_TAGS[n.tag] || "span", {}, ...kids);
    }
    default: return null;
  }
}

// Links to Nexus mods and collections open here; anything else in the
// user's browser.
function docLink(href, children) {
  if (typeof href !== "string") return el("span", {}, ...children);
  return el("a", { href: "#", class: "ext", title: href, onclick: (e) => {
    e.preventDefault();
    busy(null, () => openDocLink(href));
  } }, ...(children.length ? children : [href]));
}

async function openDocLink(href) {
  const ref = await invoke("nexus_resolve", { input: href });
  if (ref?.kind === "mod") return showMod(ref.mod_id);
  if (ref?.kind === "collection") return showCollection(ref.slug, ref.revision);
  await invoke("open_web_link", { url: href });
  toast(`Opened ${new URL(href).host} in your browser`);
}

// Only Nexus' own image host is loaded (the CSP allows nothing else); other
// pictures are a button that opens them in the browser.
function docImage(n) {
  if (n.inline) {
    const img = nexusImage(n.src, "desc-img");
    if (img.tagName === "IMG") return img;
  }
  return el("button", { class: "img-placeholder", title: n.src, onclick: (e) => {
    e.preventDefault();
    busy(null, () => openDocLink(n.src));
  } }, `🖼 Picture on ${n.host}: open in browser`);
}

// The mod page's requirements, each marked with whether the user has it
// (filled in once the installed mods are checked) and how to get it.
function requirementsSection(modId, r) {
  if (!r) return null;
  const total = r.nexus.length + r.external.length + r.dlc.length;
  const statusCells = new Map(); // nexus mod id -> [status td, action td]
  const reqRow = (q, nexus) => {
    const cells = [el("td", { class: "req-state" }), el("td", { class: "actions" })];
    if (nexus && q.mod_id) statusCells.set(q.mod_id, cells);
    return el("tr", {},
      el("td", {}, nexus && q.mod_id
        ? el("a", { href: "#", onclick: (e) => { e.preventDefault(); busy(null, () => showMod(q.mod_id)); } }, q.name)
        : q.url ? docLink(q.url, [q.name]) : q.name),
      el("td", { class: "muted" }, q.notes || ""),
      ...(nexus ? cells : []));
  };
  const dlcLine = el("p", {}, "DLC: ", r.dlc.join(", "));
  const getMissing = el("button", { class: "hidden" }, "Get missing");
  const summary = el("p", { class: "muted small req-summary" });
  const section = el("details", { class: "requirements", open: total ? "" : null },
    el("summary", {}, total ? `Requirements (${total})` : "Requirements: none listed"),
    r.dlc.length ? dlcLine : null,
    r.nexus.length ? el("div", { class: "row" }, summary, getMissing) : null,
    r.nexus.length ? el("table", { class: "req-table" },
      el("thead", {}, el("tr", {}, el("th", {}, "Nexus requirements"), el("th", {}, "Notes"), el("th", {}, "You have"), el("th", {}))),
      el("tbody", {}, ...r.nexus.map((q) => reqRow(q, true)))) : null,
    r.external.length ? el("table", { class: "req-table" },
      el("thead", {}, el("tr", {}, el("th", {}, "Off-site requirements"), el("th", {}, "Notes"))),
      el("tbody", {}, ...r.external.map((q) => reqRow(q, false)))) : null,
    r.required_by ? el("p", { class: "muted" }, `${r.required_by.toLocaleString()} mods list this one as a requirement.`) : null);
  if (currentGame && (statusCells.size || r.dlc.length)) {
    invoke("requirement_states", { gameId: currentGame.id, nexusIds: [...statusCells.keys()] }).then((st) => {
      const deps = st.mods.map((s) => ({ ...s, name: r.nexus.find((q) => q.mod_id === s.nexus_mod_id)?.name || `Mod ${s.nexus_mod_id}` }));
      for (const d of deps) {
        const [cls, label] = DEP_STATE[d.state] || DEP_STATE.unknown;
        const [state, action] = statusCells.get(d.nexus_mod_id);
        state.replaceChildren(el("span", { class: `badge ${cls}` }, label));
        action.replaceChildren(dependencyAction(d, () => loadMods().then(() => showMod(modId))) || "");
      }
      const missing = deps.filter((d) => d.state === "missing");
      summary.textContent = missing.length
        ? `You're missing ${missing.length} of ${deps.length}.` : deps.length ? "You have all of them." : "";
      if (missing.length > 1) {
        getMissing.classList.remove("hidden");
        getMissing.onclick = (e) => busy(e.target, () => getMissingDeps(missing));
      }
      if (r.dlc.some((n) => /phantom liberty/i.test(n))) {
        dlcLine.append(" ", el("span", { class: `badge ${st.phantom_liberty ? "ok" : "bad"}` },
          st.phantom_liberty ? "in the game folder" : "missing"));
      }
    }).catch((e) => { summary.textContent = `Couldn't check your installed mods: ${e}`; });
  }
  return section;
}

async function showMod(modId, highlightFile, refresh = false) {
  const [d, installed, have] = await Promise.all([invoke("nexus_mod_details", { modId, refresh }), installedNexusIds(), downloadedNexusFiles()]);
  refreshQuota().catch(() => {});
  refreshDebug().catch(() => {});
  const { info, files } = d;
  const sorted = [...files].sort((a, b) => (b.uploaded_timestamp || 0) - (a.uploaded_timestamp || 0));
  const groups = FILE_GROUPS.map(([cat, label]) => {
    const fs = sorted.filter((f) => (f.category_name || "MISCELLANEOUS") === cat
      || (cat === "MISCELLANEOUS" && !FILE_GROUPS.some(([c]) => c === f.category_name) && f.category_name !== "ARCHIVED" && f.category_name !== "DELETED"));
    if (!fs.length) return null;
    const body = fs.map((f) => fileRow(modId, f, highlightFile, have.has(`${modId}:${f.file_id}`)));
    return cat === "OLD_VERSION"
      ? el("details", {}, el("summary", {}, `${label} (${fs.length})`), ...body)
      : el("div", {}, el("h3", {}, label), ...body);
  });
  const shownFiles = groups.filter(Boolean).length;
  const doc = renderDoc(d.description);
  const description = el("div", { class: "bbcode" }, ...(doc.length ? doc : [d.description_text || info.summary || ""]));
  const tabs = [["description", "Description"], ["files", `Files (${files.filter((f) => f.category_name !== "ARCHIVED" && f.category_name !== "DELETED").length})`]];
  const panes = {
    description: el("div", { class: "tab-pane" },
      info.summary ? el("p", { class: "about" }, info.summary) : null,
      d.tags.length ? el("div", { class: "row tags" }, ...d.tags.map((t) => el("span", { class: "badge" }, t))) : null,
      requirementsSection(modId, d.requirements),
      description),
    files: el("div", { class: "tab-pane" },
      nexusUser?.is_premium ? null : el("p", { class: "muted" },
        "Free account: “Get from Nexus” opens the file in a CPMX2077 window. Click “Slow download” there and the file downloads and installs here by itself. "
        + "Prefer your own browser? Use “or use your browser”."),
      ...groups,
      shownFiles ? null : el("p", { class: "muted" }, "No files to download.")),
  };
  const tabBar = el("div", { class: "mod-tabs" }, ...tabs.map(([key, label]) =>
    el("button", { "data-pane": key, onclick: () => pick(key) }, label)));
  const pick = (key) => {
    tabBar.querySelectorAll("button").forEach((b) => b.classList.toggle("active", b.dataset.pane === key));
    Object.entries(panes).forEach(([k, p]) => p.classList.toggle("hidden", k !== key));
  };
  pick(highlightFile ? "files" : "description");
  const stat = (label, value) => el("div", { class: "stat" }, el("div", { class: "muted small" }, label), el("b", {}, value));
  $("#nexus-list").classList.toggle("hidden", !!browse);
  $("#nexus-result").classList.remove("hidden");
  $("#nexus-result").replaceChildren(el("div", { class: "card mod-detail" },
    el("div", { class: "row" },
      browse ? el("button", { onclick: () => {
        $("#nexus-result").classList.add("hidden");
        $("#nexus-list").classList.remove("hidden");
      } }, "← Back to results") : null,
      el("span", { class: "spacer" }),
      el("span", { class: `muted small${d.saved_copy ? " warn" : ""}` }, freshness(d.fetched_at, d.saved_copy)),
      el("button", { title: "Ask Nexus again instead of using the saved copy",
        onclick: (e) => busy(e.target, () => showMod(modId, highlightFile, true)) }, "Refresh"),
      el("button", { onclick: (e) => busy(e.target, () => invoke("nexus_open_page", { modId, fileId: null })) }, "Open on nexusmods.com")),
    el("div", { class: "mod-head" },
      nexusImage(info.picture_url, "hero"),
      el("div", {},
        el("h2", {}, info.name || `Mod ${modId}`),
        el("p", { class: "muted" }, "by ", authorLink(info.author || info.uploaded_by, info.user?.member_id, info.user?.name || info.uploaded_by),
          info.uploaded_by && info.uploaded_by !== info.author ? ` · uploaded by ${info.uploaded_by}` : ""),
        el("div", { class: "row" },
          categoriesById.has(info.category_id) ? el("span", { class: "badge" }, categoriesById.get(info.category_id).name) : null,
          installed.has(modId) ? el("span", { class: "badge ok" }, "installed") : null,
          info.contains_adult_content ? el("span", { class: "badge bad" }, "adult") : null))),
    el("div", { class: "stat-bar" },
      stat("Endorsements", fmtCount(info.endorsement_count)),
      stat("Unique DLs", fmtCount(info.mod_unique_downloads)),
      stat("Total DLs", fmtCount(info.mod_downloads)),
      stat("Version", info.version || "?"),
      stat("Last updated", fmtDate(info.updated_timestamp)),
      stat("Original upload", fmtDate(info.created_timestamp))),
    tabBar,
    panes.description,
    panes.files));
  $("main").scrollTop = 0;
}

function fileRow(modId, f, highlightFile, downloaded = false) {
  return el("div", { class: "file" },
    el("div", {},
      el("b", {}, f.name || f.file_name), " ",
      f.is_primary ? el("span", { class: "badge ok" }, "primary") : null, " ",
      f.file_id === highlightFile ? el("span", { class: "badge ok" }, "from link") : null, " ",
      downloaded ? el("span", { class: "badge", title: "This file is in your downloads, so it isn't downloaded again" }, "downloaded") : null,
      el("div", { class: "muted mono" }, `${f.file_name} · ${fmtSize(f.size_in_bytes)} · v${f.version || "?"} · ${fmtDate(f.uploaded_timestamp)}`)),
    el("div", { class: "actions" },
      nexusUser?.is_premium
        ? el("button", { class: "primary", onclick: (e) => busy(e.target, () => download(modId, f.file_id)) }, "Download & install")
        : el("div", {},
          el("button", { class: "primary", onclick: (e) => busy(e.target, () => getFromNexus(modId, f.file_id)) }, "Get from Nexus"),
          el("div", {}, el("button", { class: "link small", title: "Opens the file in your normal browser",
            onclick: (e) => busy(e.target, () => openInBrowser(modId, f.file_id)) }, "or use your browser")))));
}

async function download(modId, fileId, key = null, expires = null, replaces = null) {
  if (!currentGame) throw "Select a game first";
  try {
    const r = await invoke("nexus_download", {
      modId, fileId, key, expires, installTo: currentGame.id, overwrite: $("#overwrite").checked, replaces,
    });
    const note = [r.already_had ? ALREADY_HAD : null,
      r.download.verified ? null : "Nexus' checksum lookup was unreachable, so the file is unverified."].filter(Boolean).join("\n") || null;
    if (r.install) await handleOutcome(r.install, note);
    else if (note) toast(note, !r.already_had);
    if (replaces && updatesByMod.delete(replaces)) updatesChanged();
  } finally {
    endCardBar(`${modId}:${fileId}`);
    hideProgress();
    loadDownloads().catch(() => {});
    loadMods().catch(() => {});
  }
}

async function handleNxm(url) {
  // "Download collection" on Nexus opens a collection nxm:// link.
  const ref = await invoke("nexus_resolve", { input: url });
  if (ref?.kind === "collection") return showCollection(ref.slug, ref.revision);
  const link = await invoke("parse_nxm", { url });
  if (!nexusUser) { showTab("nexus"); throw "Connect your Nexus account first, then click the link again"; }
  if (link.expires && link.expires * 1000 < Date.now()) throw "This download link has expired; click it on Nexus again";
  // The update for an installed mod replaces the old version.
  const replaces = [...updatesByMod.values()].find((u) => u.nexus_mod_id === link.mod_id && u.nexus_file_id === link.file_id)?.mod_id ?? null;
  // Started from a mod's card: stay on the list, the card shows the progress.
  if (fromCard.delete(`${link.mod_id}:${link.file_id}`)) return download(link.mod_id, link.file_id, link.key, link.expires, replaces);
  await selectSource("nexus", false);
  showTab("nexus");
  // The page is a nicety; the download must not depend on it.
  await showMod(link.mod_id, link.file_id).catch(() => {});
  await download(link.mod_id, link.file_id, link.key, link.expires, replaces);
}

const ALREADY_HAD = "Already in your downloads, so the copy you have was used instead of downloading it again.";

// The bars at the top of Downloads and Get mods (and on a mod's card).
function showProgress(done, total, label = "Downloading", cardKey = null) {
  const text = `${label} ${fmtSize(done)}${total ? " of " + fmtSize(total) : ""}`;
  const bars = [$("#progress"), $("#nexus-progress"), cardKey && cardBars.get(cardKey)].filter(Boolean);
  for (const p of bars) {
    p.classList.remove("hidden");
    p.querySelector(".progress-bar").style.width = total ? `${(100 * done) / total}%` : "0";
    p.querySelector(".progress-text").textContent = p.classList.contains("card-progress")
      ? `${total ? Math.round((100 * done) / total) + "% · " : ""}${fmtSize(done)}${total ? " of " + fmtSize(total) : ""}` : text;
  }
}

function hideProgress() {
  $("#progress").classList.add("hidden");
  $("#nexus-progress").classList.add("hidden");
}

listen("nxm-link", (e) => busy(null, async () => {
  const ref = await invoke("nexus_resolve", { input: e.payload });
  if (ref?.kind !== "collection" && queueTakeNxm(await invoke("parse_nxm", { url: e.payload }))) return;
  await handleNxm(e.payload);
}));
listen("download-progress", (e) => {
  const { mod_id, file_id, done, total } = e.payload;
  showProgress(done, total, "Downloading", `${mod_id}:${file_id}`);
  queueProgress((q) => q.kind === "nexus" && q.modId === mod_id && q.fileId === file_id, done, total);
});
listen("source-progress", (e) => {
  const { source, id, file_id, done, total } = e.payload;
  showProgress(done, total, `Downloading ${id}:`);
  queueProgress((q) => q.kind === "source" && q.source === source && q.ref === id && q.file?.id === file_id, done, total);
});

// ---- other sources (GitHub, …) --------------------------------------------
let sourceInfos = []; // from source_list
let currentSource = "nexus";
const sourceQueries = {}; // source id -> { text, page } shown in its list
let sourceReq = 0;

async function loadSources() {
  sourceInfos = await invoke("source_list");
  $("#source-switch").replaceChildren(
    el("button", { "data-source": "nexus", role: "tab" }, "Nexus Mods"),
    ...sourceInfos.map((s) => el("button", { "data-source": s.id, role: "tab", title: s.description }, s.label)));
  document.querySelectorAll("#source-switch button").forEach((b) =>
    b.addEventListener("click", () => busy(null, () => selectSource(b.dataset.source))));
  const saved = loadPref("source", "nexus");
  await selectSource(sourceInfos.some((s) => s.id === saved) ? saved : "nexus", false);
}

async function selectSource(id, load = true) {
  currentSource = id;
  savePref("source", id);
  document.querySelectorAll("#source-switch button").forEach((b) => b.classList.toggle("active", b.dataset.source === id));
  $("#source-nexus").classList.toggle("hidden", id !== "nexus");
  $("#source-other").classList.toggle("hidden", id === "nexus");
  if (id === "nexus") {
    if (load && nexusUser && !browse) await runBrowse(startList());
    return;
  }
  const info = sourceInfos.find((s) => s.id === id);
  $("#source-desc").textContent = info.description;
  $("#source-search").placeholder = info.search_hint;
  if (load && !sourceQueries[id]) await runSourceList(id, { text: "", page: 1 });
}

async function installedRefs(source) {
  if (!currentGame) return new Set();
  const ms = await invoke("list_mods", { gameId: currentGame.id }).catch(() => []);
  return new Set(ms.filter((m) => m.source === source && m.source_ref).map((m) => m.source_ref.toLowerCase()));
}

async function runSourceList(id, q) {
  sourceQueries[id] = q;
  const req = ++sourceReq;
  const info = sourceInfos.find((s) => s.id === id);
  $("#source-result").classList.add("hidden");
  $("#source-list").classList.remove("hidden");
  $("#source-pager").classList.add("hidden");
  $("#source-list-title").textContent = "Loading…";
  try {
    let page = null;
    let listings;
    if (q.text) {
      page = await invoke("source_search", { source: id, query: { text: q.text, page: q.page } });
      listings = page.listings;
    } else {
      listings = await invoke("source_featured", { source: id });
    }
    const installed = await installedRefs(id);
    if (req !== sourceReq) return;
    $("#source-list-title").textContent = q.text
      ? `Results for “${q.text}” on ${info.label}${page.total != null ? ` · ${page.total.toLocaleString()} found` : ""}`
      : `Featured on ${info.label}`;
    $("#install-frameworks").classList.toggle("hidden", !!q.text || !listings.some((l) => l.category === "Framework"));
    $("#source-grid").replaceChildren(...listings.map((l) => listingCard(l, info, installed.has(l.id.toLowerCase()))));
    if (!listings.length) $("#source-grid").append(el("p", { class: "muted" }, "Nothing found."));
    const paged = page && (page.page > 1 || page.has_more);
    $("#source-pager").classList.toggle("hidden", !paged);
    if (paged) {
      $("#source-page").textContent = `Page ${page.page}`;
      $("#source-prev").disabled = page.page <= 1;
      $("#source-next").disabled = !page.has_more;
    }
  } catch (e) {
    if (req === sourceReq) $("#source-list-title").textContent = "";
    throw e;
  }
}

function listingCard(l, info, isInstalled) {
  const seed = { kind: "source", name: l.name, source: l.source, ref: l.id };
  return el("button", { class: cardClass(seed), title: l.name,
    onclick: (e) => selectMode ? toggleSelected(seed, e.currentTarget) : busy(null, () => showSourceDetails(l.source, l.id)) },
    el("div", { class: "mod-card-body" },
      el("div", { class: "mod-card-title" }, l.name),
      el("div", { class: "muted small" }, [l.author && `by ${l.author}`, l.version].filter(Boolean).join(" · ") || l.id),
      el("div", { class: "summary" }, l.summary || ""),
      el("div", { class: "stats" },
        l.popularity != null ? el("span", { title: info.popularity_label }, `★ ${fmtCount(l.popularity)}`) : null,
        l.downloads != null ? el("span", { title: "Downloads" }, `⬇ ${fmtCount(l.downloads)}`) : null,
        l.updated ? el("span", { title: "Last updated" }, fmtDate(l.updated)) : null,
        l.category ? el("span", { class: "badge" }, l.category) : null,
        isInstalled ? el("span", { class: "badge ok" }, "installed") : null)));
}

async function searchSource() {
  const text = $("#source-search").value.trim();
  if (text) {
    const ref = await invoke("source_resolve", { input: text });
    if (ref) {
      if (ref.source !== currentSource) await selectSource(ref.source, false);
      return showSourceDetails(ref.source, ref.id);
    }
  }
  await runSourceList(currentSource, { text, page: 1 });
}

// Queue frameworks (GitHub ids; none = every missing one) with whatever they
// need first. The queue installs in order, so requirements land first.
async function installFrameworks(wanted = []) {
  if (!currentGame) throw "Select a game first";
  const plan = await invoke("framework_plan", { gameId: currentGame.id, wanted });
  if (!plan.length) return toast("All core frameworks are already installed");
  enqueue(plan.map((f) => ({ kind: "source", name: f.name, source: f.source, ref: f.id })));
}

$("#install-frameworks").addEventListener("click", (e) => busy(e.target, () => installFrameworks()));
$("#source-search-go").addEventListener("click", (e) => busy(e.target, searchSource));
$("#source-search").addEventListener("keydown", (e) => {
  if (e.key === "Enter") busy(null, searchSource);
});
$("#source-prev").addEventListener("click", (e) => busy(e.target, () => {
  const q = sourceQueries[currentSource];
  return runSourceList(currentSource, { ...q, page: Math.max(1, q.page - 1) });
}));
$("#source-next").addEventListener("click", (e) => busy(e.target, () => {
  const q = sourceQueries[currentSource];
  return runSourceList(currentSource, { ...q, page: q.page + 1 });
}));

// `replacing`: { modId, name } when the user is picking the file that
// updates an installed mod.
async function showSourceDetails(source, id, replacing = null) {
  const info = sourceInfos.find((s) => s.id === source);
  const [d, installed] = await Promise.all([invoke("source_details", { source, id }), installedRefs(source)]);
  const l = d.listing;
  const groups = [];
  for (const f of d.files) {
    let g = groups.find((x) => x.name === f.group);
    if (!g) groups.push((g = { name: f.group, files: [] }));
    g.files.push(f);
  }
  const files = groups.map((g, i) => {
    const rows = g.files.map((f) => sourceFileRow(info, l, f, replacing));
    return i < 2 ? el("div", {}, el("h3", {}, g.name), ...rows)
      : el("details", {}, el("summary", {}, `${g.name} (${g.files.length})`), ...rows);
  });
  const desc = el("div", { class: "description collapsed" }, d.description);
  const more = el("button", { class: "link", onclick: () => {
    desc.classList.toggle("collapsed");
    more.textContent = desc.classList.contains("collapsed") ? "Show all" : "Show less";
  } }, "Show all");
  const back = () => {
    if (!sourceQueries[source]) return busy(null, () => runSourceList(source, { text: "", page: 1 }));
    $("#source-result").classList.add("hidden");
    $("#source-list").classList.remove("hidden");
  };
  $("#source-list").classList.add("hidden");
  $("#source-result").classList.remove("hidden");
  $("#source-result").replaceChildren(el("div", { class: "card mod-detail" },
    el("div", { class: "row" },
      el("button", { onclick: back }, "← Back to the list"),
      el("span", { class: "spacer" }),
      el("button", { onclick: (e) => busy(e.target, () => invoke("open_url", { url: l.url })) }, `Open on ${info.label}`)),
    el("div", { class: "mod-head" },
      el("div", {},
        el("h2", {}, l.name),
        el("p", { class: "muted" }, [l.author && `by ${l.author}`, l.version && `latest ${l.version}`, l.id].filter(Boolean).join(" · ")),
        el("div", { class: "stats" },
          l.popularity != null ? el("span", {}, `★ ${fmtCount(l.popularity)} ${info.popularity_label}`) : null,
          l.updated ? el("span", {}, `Updated ${fmtDate(l.updated)}`) : null,
          l.category ? el("span", { class: "badge" }, l.category) : null,
          installed.has(l.id.toLowerCase()) ? el("span", { class: "badge ok" }, "installed") : null),
        el("p", {}, l.summary || ""),
        l.requires.length ? el("p", { class: "muted" }, `Needs ${l.requires.join(" and ")} to load.`) : null,
        l.requires.length && !replacing ? el("button", {
          title: `Queue ${l.name} after whatever it needs that this game doesn't have yet`,
          onclick: (e) => busy(e.target, () => installFrameworks([l.id])),
        }, "Install with what it needs") : null)),
    replacing ? el("div", { class: "card notice" },
      `The new version has several files. Pick the one that updates ${replacing.name}; the installed version is replaced once it installs.`) : null,
    d.description ? el("h3", {}, "Latest release notes") : null,
    d.description ? desc : null,
    d.description.length > 600 ? more : null,
    ...(files.length ? files : [el("p", { class: "muted" }, `${l.name} has no downloadable files on ${info.label}.`)])));
  $("main").scrollTop = 0;
}

function sourceFileRow(info, l, f, replacing) {
  const meta = [fmtSize(f.size), f.version, f.uploaded ? fmtDate(f.uploaded) : null,
    f.downloads != null ? `${fmtCount(f.downloads)} downloads` : null].filter(Boolean).join(" · ");
  return el("div", { class: "file" },
    el("div", {},
      el("b", {}, f.file_name), " ",
      f.prerelease ? el("span", { class: "badge bad" }, "pre-release") : null, " ",
      f.verifiable ? el("span", { class: "badge ok", title: `Checked against the SHA-256 checksum ${info.label} publishes` }, "checksum") : null,
      el("div", { class: "muted mono" }, meta)),
    el("div", { class: "actions" },
      f.installable
        ? el("button", { class: "primary", onclick: (e) => busy(e.target, () => sourceDownload(l.source, l.id, f, replacing?.modId ?? null)) },
          replacing ? "Update with this" : "Download & install")
        : el("button", { title: "Not an archive the installer can take: it's saved to Downloads only",
          onclick: (e) => busy(e.target, () => sourceDownload(l.source, l.id, f, null, false)) }, "Download")));
}

async function sourceDownload(source, id, f, replaces = null, install = true) {
  if (install && !currentGame) throw "Select a game first";
  try {
    const r = await invoke("source_download", {
      source, id, fileId: f.id, installTo: install ? currentGame.id : null, overwrite: $("#overwrite").checked, replaces,
    });
    const note = [r.already_had ? ALREADY_HAD : null, r.download.verified ? null : `${r.download.check}, so the file is unverified.`]
      .filter(Boolean).join("\n") || null;
    if (r.install) await handleOutcome(r.install, note);
    else toast([r.already_had ? null : `Downloaded ${r.download.file_name}`, note].filter(Boolean).join("\n"));
    if (replaces && updatesByMod.delete(replaces)) updatesChanged();
  } finally {
    hideProgress();
    loadDownloads().catch(() => {});
    loadMods().catch(() => {});
  }
}

// ---- picking several mods -------------------------------------------------
let selectMode = false;
const selection = new Map(); // "nexus:107" or "github:owner/repo" -> queue seed

const seedKey = (s) => s.kind === "nexus" ? `nexus:${s.modId}` : `${s.source}:${s.ref.toLowerCase()}`;
const cardClass = (seed) => selection.has(seedKey(seed)) ? "mod-card selected" : "mod-card";

function setSelectMode(on) {
  selectMode = on;
  document.querySelectorAll(".select-mode").forEach((b) => {
    b.classList.toggle("active", on);
    b.textContent = on ? "Done selecting" : "Select several";
  });
  renderSelectBar();
}

function toggleSelected(seed, card) {
  const k = seedKey(seed);
  if (selection.has(k)) selection.delete(k); else selection.set(k, seed);
  card.classList.toggle("selected", selection.has(k));
  renderSelectBar();
}

function renderSelectBar() {
  const n = selection.size;
  $("#select-bar").classList.toggle("hidden", !selectMode && !n);
  $("#select-count").textContent = n ? `${n} mod${n === 1 ? "" : "s"} selected` : "Click mods to select them";
  $("#select-note").textContent = n && !nexusUser?.is_premium && [...selection.values()].some((s) => s.kind === "nexus")
    ? "Nexus mods open one by one in the Nexus window." : "";
  $("#select-queue").disabled = !n;
}

document.querySelectorAll(".select-mode").forEach((b) => b.addEventListener("click", () => setSelectMode(!selectMode)));
$("#select-clear").addEventListener("click", () => {
  selection.clear();
  document.querySelectorAll(".mod-card.selected").forEach((c) => c.classList.remove("selected"));
  renderSelectBar();
});
$("#select-queue").addEventListener("click", (e) => busy(e.target, async () => {
  if (!currentGame) throw "Select a game first";
  const seeds = [...selection.values()];
  const nexusInstalled = await installedNexusIds();
  const refs = {};
  for (const s of seeds) if (s.kind === "source" && !refs[s.source]) refs[s.source] = await installedRefs(s.source);
  const isInstalled = (s) => s.kind === "nexus" ? nexusInstalled.has(s.modId) : refs[s.source].has(s.ref.toLowerCase());
  const fresh = seeds.filter((s) => !isInstalled(s));
  const skipped = seeds.length - fresh.length;
  if (fresh.length) enqueue(fresh);
  selection.clear();
  document.querySelectorAll(".mod-card.selected").forEach((c) => c.classList.remove("selected"));
  setSelectMode(false);
  if (skipped) toast(`Left out ${skipped} already installed mod${skipped === 1 ? "" : "s"}. Use Check for updates in Installed mods to update them.`);
}));

// ---- download queue ---------------------------------------------------------
// Many mods in one go. Premium Nexus files and GitHub files download straight
// away. With a free Nexus account, one Nexus window steps through the file
// pages: the user starts each download there, and the window moves on to the
// next mod as soon as that download's nxm:// link arrives. Downloads run one
// at a time, and installs one at a time after them.
const queue = { items: [], clickItem: null, paused: false, usedWindow: false, showList: false };
let queueSeq = 0;
let downloadLane = Promise.resolve();
let installLane = Promise.resolve();

const FINISHED = new Set(["done", "downloaded", "skipped", "failed", "cancelled", "pick"]);
const Q_LABEL = {
  queued: "waiting", resolving: "looking up its files", "ready-click": "waiting for the Nexus window",
  click: "in the Nexus window", "ready-download": "waiting to download", downloading: "downloading",
  "ready-install": "waiting to install", installing: "installing", choices: "waiting for your installer choices",
  done: "installed", downloaded: "downloaded", skipped: "skipped", failed: "failed", cancelled: "cancelled", pick: "pick a file",
};

function enqueue(seeds) {
  if (!currentGame) throw "Select a game first";
  // Start a fresh list once everything before has finished.
  if (queue.items.every((q) => FINISHED.has(q.state))) queue.items = [];
  const added = [];
  for (const s of seeds) {
    if ([...queue.items, ...added].some((q) => !FINISHED.has(q.state) && seedKey(q) === seedKey(s))) continue;
    added.push({ ...s, id: ++queueSeq, state: "queued", note: "", progress: null, gameId: currentGame.id });
  }
  // All in the list first, so "mod 2 of 5" counts are right from the start.
  queue.items.push(...added);
  queue.paused = false;
  renderQueue();
  added.forEach(startItem);
  if (added.length) toast(`Added ${added.length} mod${added.length === 1 ? "" : "s"} to the download queue`);
}

// The one obvious file of a mod, or null when the user has to choose.
function pickNexusFile(files) {
  const main = files.filter((f) => f.category_name === "MAIN");
  return main.find((f) => f.is_primary) || (main.length === 1 ? main[0] : null);
}
function pickSourceFile(files) {
  const stable = files.filter((f) => f.installable && !f.prerelease);
  const newest = stable.filter((f) => f.group === stable[0]?.group);
  return newest.length === 1 ? newest[0] : null;
}

async function startItem(it) {
  try {
    if (it.kind === "nexus" && it.fileId === undefined) {
      it.state = "resolving";
      renderQueue();
      const d = await invoke("nexus_mod_details", { modId: it.modId });
      it.name = d.info.name || it.name;
      // No obvious file: the window opens the Files tab and takes whichever
      // file the user downloads.
      it.fileId = pickNexusFile(d.files)?.file_id ?? null;
    }
    if (it.kind === "source" && it.file === undefined) {
      it.state = "resolving";
      renderQueue();
      const d = await invoke("source_details", { source: it.source, id: it.ref });
      it.name = d.listing.name;
      it.file = pickSourceFile(d.files);
    }
  } catch (e) {
    return finishItem(it, "failed", String(e));
  }
  if (it.state === "cancelled") return;
  if (it.kind === "source") {
    if (!it.file) return finishItem(it, "pick", "it has several files: open it and pick one");
    return scheduleDownload(it);
  }
  if (it.kind === "nexus" && nexusUser?.is_premium) {
    if (!it.fileId) return finishItem(it, "pick", "it has several main files: open it and pick one");
    return scheduleDownload(it);
  }
  if (it.kind === "nexus" && !nexusUser) return finishItem(it, "failed", "connect your Nexus account first");
  // Already downloaded: no need to click for it again.
  const have = it.fileId && await invoke("find_download", { key: { kind: "nexus", mod_id: it.modId, file_id: it.fileId } }).catch(() => null);
  if (it.state === "cancelled") return;
  if (have) return scheduleDownload(it);
  it.state = "ready-click";
  advanceClick();
}

function queueStep(it) {
  return { position: queue.items.indexOf(it) + 1, total: queue.items.length, name: it.name };
}

// Show the next mod waiting for a click in the Nexus window, in queue order.
function advanceClick() {
  renderQueue();
  if (queue.clickItem || queue.paused) return;
  const next = queue.items.find((q) => q.state === "ready-click");
  if (!next) {
    // Nothing left to click: the window has done its job.
    const more = queue.items.some((q) => q.state === "queued" || q.state === "resolving");
    if (!more && queue.usedWindow) {
      queue.usedWindow = false;
      invoke("nexus_window_close").catch(() => {});
    }
    return;
  }
  queue.clickItem = next;
  queue.usedWindow = true;
  next.state = "click";
  renderQueue();
  invoke("nexus_open_in_app", { modId: next.modId, fileId: next.fileId, queue: queueStep(next) }).catch((e) => {
    if (queue.clickItem === next) queue.clickItem = null;
    finishItem(next, "failed", String(e));
    advanceClick();
  });
}

// An nxm:// link for a queued Nexus file: queue its download and move the
// Nexus window on. False for links the queue isn't waiting for.
function queueTakeNxm(link) {
  const waiting = (q) => (q.state === "click" || q.state === "ready-click") && q.kind === "nexus"
    && q.modId === link.mod_id && (!q.fileId || q.fileId === link.file_id);
  const it = queue.clickItem && waiting(queue.clickItem) ? queue.clickItem : queue.items.find(waiting);
  if (!it) return false;
  if (link.expires && link.expires * 1000 < Date.now()) return false;
  it.fileId = link.file_id;
  it.link = link;
  if (queue.clickItem === it) queue.clickItem = null;
  scheduleDownload(it);
  advanceClick();
  return true;
}

function scheduleDownload(it) {
  it.state = "ready-download";
  renderQueue();
  downloadLane = downloadLane.then(() => runDownload(it));
}

async function runDownload(it) {
  if (it.state !== "ready-download") return;
  it.state = "downloading";
  renderQueue();
  try {
    const r = it.kind === "nexus"
      ? await invoke("nexus_download", { modId: it.modId, fileId: it.fileId, key: it.link?.key ?? null, expires: it.link?.expires ?? null,
        installTo: null, overwrite: false, replaces: null })
      : await invoke("source_download", { source: it.source, id: it.ref, fileId: it.file.id, installTo: null, overwrite: false, replaces: null });
    it.downloadId = r.download_id;
    if (r.already_had) it.note = "already downloaded";
    if (!r.download.verified) it.note = it.kind === "nexus" ? "unverified: Nexus checksum lookup failed" : r.download.check;
  } catch (e) {
    return finishItem(it, "failed", String(e));
  } finally {
    hideProgress();
    loadDownloads().catch(() => {});
  }
  if (it.kind === "source" && !it.file.installable) return finishItem(it, "downloaded", it.note);
  it.state = "ready-install";
  renderQueue();
  installLane = installLane.then(() => runInstall(it));
}

async function runInstall(it) {
  it.state = "installing";
  renderQueue();
  try {
    let outcome = await invoke("install_download", { downloadId: it.downloadId, gameId: it.gameId, overwrite: $("#overwrite").checked, replaces: it.replaces ?? null });
    if (outcome.status === "needs_choices") {
      it.state = "choices";
      renderQueue();
      if (!(await runWizard(outcome))) return finishItem(it, "downloaded", "installer cancelled; it's in Downloads");
    }
    if (it.replaces && updatesByMod.delete(it.replaces)) updatesChanged();
    finishItem(it, "done", it.note);
  } catch (e) {
    finishItem(it, "failed", String(e));
  } finally {
    loadMods().catch(() => {});
  }
}

function finishItem(it, state, note = "") {
  it.state = state;
  it.note = note || "";
  it.progress = null;
  renderQueue();
  if (queue.items.length && queue.items.every((q) => FINISHED.has(q.state))) {
    const c = (s) => queue.items.filter((q) => q.state === s).length;
    toast(`Download queue finished: ${queueSummary()}`, c("failed") > 0);
    loadDownloads().catch(() => {});
  }
}

function queueSummary() {
  const n = (s) => queue.items.filter((q) => q.state === s).length;
  return [[n("done"), "installed"], [n("downloaded"), "downloaded only"], [n("skipped"), "skipped"],
    [n("pick"), "need you to pick a file"], [n("failed"), "failed"], [n("cancelled"), "cancelled"]]
    .filter(([k]) => k).map(([k, w]) => `${k} ${w}`).join(", ");
}

function queueSkip() {
  const it = queue.clickItem;
  if (!it) return;
  queue.clickItem = null;
  finishItem(it, "skipped");
  advanceClick();
}

function queueCancel() {
  queue.clickItem = null;
  queue.paused = false;
  // Downloads the user already started in the Nexus window still finish.
  for (const q of queue.items) {
    if (["queued", "resolving", "ready-click", "click"].includes(q.state) || (q.state === "ready-download" && !q.link)) {
      finishItem(q, "cancelled");
    }
  }
  queue.usedWindow = false;
  invoke("nexus_window_close").catch(() => {});
  renderQueue();
}

// The user closed the Nexus window mid-queue: wait until they resume.
function queueWindowClosed() {
  const it = queue.clickItem;
  if (!it) return;
  queue.clickItem = null;
  queue.paused = true;
  it.state = "ready-click";
  renderQueue();
}

let progressFrame = 0;
function queueProgress(match, done, total) {
  const it = queue.items.find((q) => q.state === "downloading" && match(q));
  if (!it) return;
  it.progress = total ? done / total : null;
  if (!progressFrame) progressFrame = requestAnimationFrame(() => { progressFrame = 0; renderQueue(); });
}

function renderQueue() {
  const items = queue.items;
  $("#queue").classList.toggle("hidden", !items.length);
  if (!items.length) return;
  const finished = items.filter((q) => FINISHED.has(q.state)).length;
  const active = finished < items.length;
  const c = queue.clickItem;
  const busyItem = items.find((q) => q.state === "downloading") || items.find((q) => q.state === "installing" || q.state === "choices");
  $("#queue-title").textContent = active ? `Download queue: ${finished} of ${items.length} done` : `Download queue finished: ${queueSummary()}`;
  $("#queue-bar").style.width = `${(100 * finished) / items.length}%`;
  $("#queue-now").textContent = queue.paused ? "Paused because the Nexus window was closed."
    : c ? `In the Nexus window: click “Slow download” for ${c.name}${c.fileId ? "" : " (pick the file you want)"}.`
    : busyItem ? `${Q_LABEL[busyItem.state][0].toUpperCase()}${Q_LABEL[busyItem.state].slice(1)}: ${busyItem.name}` : "";
  $("#queue-show").classList.toggle("hidden", !c && !queue.paused);
  $("#queue-show").textContent = queue.paused ? "Resume" : "Show Nexus window";
  $("#queue-skip").classList.toggle("hidden", !c);
  $("#queue-cancel").classList.toggle("hidden", !active);
  $("#queue-clear").classList.toggle("hidden", active);
  $("#queue-toggle").textContent = queue.showList ? "Hide list" : "Show list";
  $("#queue-items").classList.toggle("hidden", !queue.showList);
  if (!queue.showList) return;
  const badge = { done: "badge ok", failed: "badge bad", pick: "badge bad", cancelled: "badge", skipped: "badge" };
  $("#queue-items").replaceChildren(...items.map((q) => el("li", {},
    el("span", { class: "q-name", title: q.name }, q.name),
    q.note ? el("span", { class: "muted small", title: q.note }, q.note.length > 80 ? q.note.slice(0, 80) + "…" : q.note) : null,
    el("span", { class: badge[q.state] || "badge" },
      q.state === "downloading" && q.progress != null ? `downloading ${Math.round(q.progress * 100)}%` : Q_LABEL[q.state]))));
}

$("#queue-skip").addEventListener("click", queueSkip);
$("#queue-cancel").addEventListener("click", queueCancel);
$("#queue-clear").addEventListener("click", () => { queue.items = []; renderQueue(); });
$("#queue-toggle").addEventListener("click", () => { queue.showList = !queue.showList; renderQueue(); });
$("#queue-show").addEventListener("click", (e) => busy(e.target, async () => {
  if (queue.paused) {
    queue.paused = false;
    advanceClick();
  } else if (queue.clickItem) {
    await invoke("nexus_open_in_app", { modId: queue.clickItem.modId, fileId: queue.clickItem.fileId, queue: queueStep(queue.clickItem) });
  }
}));
listen("queue-control", (e) => (e.payload === "skip" ? queueSkip() : queueCancel()));
listen("nexus-window-closed", queueWindowClosed);

// ---- downloads ------------------------------------------------------------
// One row per mod; a mod with several downloaded versions is a folder that
// opens to show them, newest first, like in a file manager.
let openFolders = new Set(loadPref("downloadsOpen", []));
let downloadGroups = [];
let downloadsInstalled = [];
const CHANNEL_TITLE = {
  stable: "A regular release",
  beta: "A test build (beta, alpha, release candidate or preview): newer features, more likely to break",
  nightly: "An automatic development build: the newest changes, least tested",
};

function channelBadge(ch) {
  return el("span", { class: `badge channel ${ch}`, title: CHANNEL_TITLE[ch] || "" }, ch);
}

function channelShown(ch) {
  const f = $("#downloads-channel").value;
  return !f || (f === "stable" ? ch === "stable" : ch !== "stable");
}

async function loadDownloads() {
  const [groups, installed, where] = await Promise.all([
    invoke("list_download_groups"),
    currentGame ? invoke("list_mods", { gameId: currentGame.id }).catch(() => []) : [],
    invoke("downloads_location").catch(() => null),
  ]);
  downloadGroups = groups;
  downloadsInstalled = installed;
  if (where) $("#downloads-where").textContent = where.path;
  renderDownloads();
}

function renderDownloads() {
  const rows = [];
  let shownAny = false;
  for (const g of downloadGroups) {
    const entries = g.entries.filter((e) => channelShown(e.channel));
    if (!entries.length) continue;
    shownAny = true;
    if (entries.length === 1) {
      rows.push(downloadRow(entries[0], 0));
      continue;
    }
    const open = openFolders.has(g.key);
    const installedEntry = entries.find((e) => installedFor(e));
    const newest = entries[0];
    const toggle = () => {
      if (openFolders.has(g.key)) openFolders.delete(g.key); else openFolders.add(g.key);
      savePref("downloadsOpen", [...openFolders]);
      renderDownloads();
    };
    const channels = ["stable", "beta", "nightly"].filter((c) => entries.some((e) => e.channel === c));
    rows.push(el("tr", { class: "folder", onclick: (e) => { if (e.target.tagName !== "BUTTON" || e.target.classList.contains("twisty")) toggle(); } },
      el("td", {},
        el("button", { class: "twisty", "aria-expanded": String(open), title: open ? "Hide versions" : "Show versions" }, open ? "▾" : "▸"),
        el("span", { class: "folder-icon", "aria-hidden": "true" }, "📁"),
        el("b", {}, g.name),
        el("span", { class: "muted small" }, ` · ${entries.length} versions`),
        installedEntry ? el("span", { class: "badge ok" }, `${installedEntry.version || "one"} installed`) : null),
      el("td", {}, newest.version || "—", " ", newest.channel === "stable" ? null : channelBadge(newest.channel),
        channels.length > 1 ? el("div", { class: "muted small" }, channels.join(", ")) : null),
      el("td", {}, [...new Set(entries.map((e) => e.game_version).filter(Boolean))].join(", ") || "—"),
      el("td", {}, el("span", { class: "badge" }, sourceLabel({ ...newest, source: newest.source || "nexus" }))),
      el("td", {}, fmtSize(entries.reduce((n, e) => n + e.size, 0))),
      el("td", {}),
      el("td", {})));
    if (open) rows.push(...entries.map((e) => downloadRow(e, 1)));
  }
  $("#downloads-empty").textContent = downloadGroups.length ? "Nothing matches this filter." : "Nothing downloaded yet.";
  $("#downloads-empty").classList.toggle("hidden", shownAny);
  $("#downloads-body").replaceChildren(...rows);
}

function installedFor(d) {
  return downloadsInstalled.find((x) => x.archive_sha256 && x.archive_sha256 === d.sha256);
}

function downloadRow(d, depth) {
  const m = installedFor(d);
  const up = m && updatesByMod.get(m.id);
  const checked = d.checked || (d.verified ? "MD5 matches Nexus" : "unverified");
  return el("tr", { class: depth ? "child" : "" },
    el("td", {}, el("div", { class: depth ? "indent" : "" },
      depth ? null : el("b", {}, d.mod_name || d.file_name),
      m ? el("span", { class: "badge ok" }, m.status === "installed" ? "installed" : "installed, off") : null,
      d.on_disk ? null : el("span", { class: "badge bad", title: d.path }, "file missing"),
      el("div", { class: "muted mono", title: d.path }, d.file_name))),
    el("td", {}, d.version || "—", " ", channelBadge(d.channel),
      up ? el("div", { class: "badge ok" }, `${up.latest} available`) : null),
    el("td", {}, d.game_version || "—"),
    el("td", {}, depth ? null : el("span", { class: "badge" }, sourceLabel({ ...d, source: d.source || "nexus" }))),
    el("td", {}, fmtSize(d.size)),
    el("td", {}, el("span", { class: d.verified ? "badge ok" : "badge bad", title: `SHA-256 ${d.sha256}` }, checked)),
    el("td", { class: "actions" },
      up ? el("button", {
        class: "update",
        title: up.to_stable ? `${m.version} is a pre-release (a test build). ${up.latest} is the release most mods are built against.` : "",
        onclick: (e) => busy(e.target, () => applyUpdate(up)),
      }, up.to_stable ? `Switch to stable ${up.latest}` : `Update to ${up.latest}`) : null, " ",
      m || !d.on_disk ? null : el("button", {
        onclick: (e) => busy(e.target, async () => {
          if (!currentGame) throw "Select a game first";
          // Another version of this mod is installed: switch to this one.
          let other = downloadsInstalled.find((x) => sameMod(x, d));
          if (other && !(await dialog.ask(`${other.name} ${other.version || ""} (${other.archive_name}) is installed. Replace it with ${d.version || d.file_name}?`,
            { title: "Switch version", kind: "info", okLabel: "Replace it", cancelLabel: "No" }))) {
            // A Nexus mod can have several files meant to go in together.
            if (!(await dialog.ask(`Install ${d.file_name} next to ${other.archive_name} instead?`, { title: "Switch version", kind: "info" }))) return;
            other = null;
          }
          const r = await invoke("install_download", { downloadId: d.id, gameId: currentGame.id, overwrite: $("#overwrite").checked, replaces: other?.id ?? null });
          await handleOutcome(r);
          loadDownloads().catch(() => {});
        }),
      }, downloadsInstalled.some((x) => sameMod(x, d)) ? "Switch to this" : "Install"), " ",
      el("button", {
        class: "danger",
        title: m ? "The installed mod keeps working; only the downloaded archive is deleted" : "",
        onclick: (e) => busy(e.target, async () => {
          if (!(await dialog.ask(`Delete ${d.file_name}${d.version ? ` (${d.version})` : ""} from your downloads?`, { title: "Delete download", kind: "warning" }))) return;
          await invoke("delete_download", { downloadId: d.id });
          loadDownloads().catch(() => {});
        }),
      }, "Delete")));
}

// The installed mod a download is another version of.
function sameMod(m, d) {
  if (m.archive_sha256 === d.sha256) return false;
  if (d.nexus_mod_id) return m.nexus_mod_id === d.nexus_mod_id;
  if (d.source_ref) return m.source === d.source && m.source_ref === d.source_ref;
  return false;
}

$("#downloads-channel").value = loadPref("downloadsChannel", "");
$("#downloads-channel").addEventListener("change", () => {
  savePref("downloadsChannel", $("#downloads-channel").value);
  renderDownloads();
});
$("#downloads-expand").addEventListener("click", () => {
  const folders = downloadGroups.filter((g) => g.entries.length > 1).map((g) => g.key);
  const allOpen = folders.every((k) => openFolders.has(k));
  openFolders = allOpen ? new Set() : new Set(folders);
  $("#downloads-expand").textContent = allOpen ? "Open all folders" : "Close all folders";
  savePref("downloadsOpen", [...openFolders]);
  renderDownloads();
});
document.querySelectorAll("[data-goto]").forEach((b) => b.addEventListener("click", () => showTab(b.dataset.goto)));

// ---- download location (Settings) ----------------------------------------
async function refreshLocation() {
  const loc = await invoke("downloads_location");
  $("#dl-location").textContent = loc.path;
  $("#dl-location-default").classList.toggle("hidden", !loc.is_default);
  $("#dl-reset").classList.toggle("hidden", loc.is_default);
  $("#downloads-where").textContent = loc.path;
}

function moveSummary(r) {
  const lines = [`Moved ${r.moved} file${r.moved === 1 ? "" : "s"}.`];
  if (r.missing.length) lines.push(`${r.missing.length} listed file${r.missing.length === 1 ? " was" : "s were"} already gone.`);
  if (r.failed.length) lines.push(`Couldn't move ${r.failed.length}; they stay where they were:`, ...r.failed.slice(0, 4));
  return lines.join("\n");
}

async function changeLocation(path) {
  const count = (await invoke("list_downloads")).length;
  let move = false;
  if (count) {
    move = await dialog.ask(
      `Move the ${count} file${count === 1 ? "" : "s"} you've already downloaded to the new folder too? They're sorted into one folder per mod.\n\n`
      + "If you leave them, they stay where they are and keep working from there.",
      { title: "Download location", kind: "info", okLabel: "Move them", cancelLabel: "Leave them" });
  }
  const r = await invoke("set_downloads_location", { path, moveFiles: move });
  await refreshLocation();
  toast(move ? `Download location changed.\n${moveSummary(r)}` : "Download location changed. New downloads go there.", r.failed.length > 0);
  if ($("#tab-downloads").classList.contains("active")) loadDownloads().catch(() => {});
}

$("#dl-change").addEventListener("click", (e) => busy(e.target, async () => {
  const path = await dialog.open({ directory: true, title: "Where should downloads be saved?" });
  if (path) await changeLocation(path);
}));
$("#dl-reset").addEventListener("click", (e) => busy(e.target, () => changeLocation(null)));
$("#dl-organize").addEventListener("click", (e) => busy(e.target, async () => {
  const r = await invoke("organize_downloads");
  toast(r.moved || r.failed.length ? moveSummary(r) : "Everything is already in its mod's folder.", r.failed.length > 0);
}));

// ---- game versions (Installed mods) --------------------------------------
// A ribbon of every game version seen, newest on the right. Picking one
// shows the mods installed on it and, for an older version, what changed in
// the mod list since the game moved on.
let gameVersions = [];
let ribbonPick = null; // index into gameVersions, or null for all

function versionLabel(v) {
  return v.version || (v.build_id ? `build ${v.build_id}` : "unknown");
}

async function loadVersionRibbon() {
  if (!currentGame) return;
  const gameId = currentGame.id;
  const list = await invoke("game_version_history", { gameId });
  if (currentGame?.id !== gameId) return;
  const before = ribbonPick != null ? gameVersions[ribbonPick] : null;
  gameVersions = list;
  ribbonPick = before ? list.findIndex((v) => v.id === before.id && v.version === before.version) : null;
  if (ribbonPick === -1) ribbonPick = null;
  renderVersionRibbon();
}

function renderVersionRibbon() {
  const ribbon = $("#version-ribbon");
  ribbon.classList.toggle("hidden", gameVersions.length === 0);
  // Two versions with one exe version (a Steam hotfix) are told apart by build.
  const dupe = (v) => gameVersions.filter((x) => x.version === v.version).length > 1;
  const chip = (label, sub, i, title, extra = "") => el("button", {
    class: `chip${ribbonPick === i ? " active" : ""}${extra}`, role: "tab", "aria-selected": String(ribbonPick === i), title,
    onclick: () => { ribbonPick = i; renderVersionRibbon(); renderMods(); showLoadout().catch((e) => toast(String(e), true)); },
  }, el("b", {}, label), el("span", { class: "muted small" }, sub));
  ribbon.replaceChildren(
    el("span", { class: "muted small ribbon-label" }, "Game version"),
    chip("All", `${mods.length} mods`, null, "Every installed mod"),
    ...gameVersions.map((v, i) => {
      const label = versionLabel(v) + (dupe(v) && v.build_id ? ` · ${v.build_id}` : "");
      const sub = `${v.mod_ids.length} installed${v.current ? " · current" : ""}`;
      const title = [
        v.build_id ? `Steam build ${v.build_id}` : null,
        v.first_seen ? `First seen ${v.first_seen.slice(0, 10)}` : null,
        v.left_at ? `Updated away from on ${v.left_at.slice(0, 10)}` : null,
        v.id == null ? "From mods installed before CPMX2077 kept a version history" : null,
      ].filter(Boolean).join("\n");
      return [i ? el("span", { class: "ribbon-arrow", "aria-hidden": "true" }, "→") : null, chip(label, sub, i, title, v.current ? " current" : "")];
    }).flat().filter(Boolean));
}

// Filter for the installed list: mods installed on the picked version.
function ribbonFilter(m) {
  if (ribbonPick == null) return true;
  return gameVersions[ribbonPick]?.mod_ids.includes(m.id) ?? true;
}

async function showLoadout() {
  const box = $("#version-loadout");
  const v = ribbonPick != null ? gameVersions[ribbonPick] : null;
  if (!v || v.current) {
    box.classList.toggle("hidden", !v);
    if (v) box.replaceChildren(el("p", { class: "muted" },
      `Showing the ${v.mod_ids.length} mods installed or updated since the game became ${versionLabel(v)}. `
      + "Mods from older versions are still installed; pick an older version to see them."));
    return;
  }
  if (v.id == null || !v.snapshot_count) {
    box.classList.remove("hidden");
    box.replaceChildren(el("p", { class: "muted" },
      `Showing the mods installed while the game was ${versionLabel(v)}. They may need an update for the current version. `
      + "CPMX2077 hadn't recorded your full mod list for this version yet."));
    return;
  }
  const l = await invoke("game_version_loadout", { gameId: currentGame.id, id: v.id });
  const n = (c) => l.mods.filter((m) => m.change === c).length;
  const LABEL = { same: "unchanged", updated: "updated since", disabled: "turned off since", enabled: "turned on since", removed: "removed since" };
  const summary = ["same", "updated", "disabled", "enabled", "removed"].filter((c) => n(c)).map((c) => `${n(c)} ${LABEL[c]}`);
  if (l.added.length) summary.push(`${l.added.length} added since`);
  box.classList.remove("hidden");
  box.replaceChildren(
    el("p", {}, `When the game updated from ${versionLabel(v)}${l.left_at ? ` on ${l.left_at.slice(0, 10)}` : ""}, you had `,
      el("b", {}, `${l.mods.length} mods`), `: ${summary.join(", ")}.`),
    el("p", { class: "muted small" }, "The list below shows the mods still installed that were installed on that version; they may need an update for the current one."),
    el("details", {},
      el("summary", {}, "Mod list on that version"),
      el("table", {},
        el("thead", {}, el("tr", {}, el("th", {}, "Mod"), el("th", {}, "Version then"), el("th", {}, "Now"))),
        el("tbody", {}, ...l.mods.map((m) => el("tr", { class: m.change === "removed" ? "off" : "" },
          el("td", {}, el("b", {}, m.name), m.enabled ? null : el("span", { class: "badge" }, "was off"),
            el("div", { class: "muted mono" }, m.archive_name)),
          el("td", {}, m.version || "—"),
          el("td", {}, el("span", { class: `badge${m.change === "removed" ? " bad" : m.change === "same" ? "" : " ok"}` },
            m.change === "updated" ? `updated to ${m.now_version || "another file"}` : LABEL[m.change]))))))));
}

// ---- sidebar width ------------------------------------------------------
// Drag the divider to resize the left menu; double-click resets it. A
// dragged width is kept between launches; the docked debug terminal fills
// whatever width the menu has.
const SIDEBAR_MIN = 220, SIDEBAR_MAX = 640;
function clampSidebar(px) {
  // Leave the main area at least 360px however wide the menu is dragged.
  const max = Math.max(SIDEBAR_MIN, Math.min(SIDEBAR_MAX, window.innerWidth - 360));
  return Math.round(Math.min(max, Math.max(SIDEBAR_MIN, px)));
}
// null clears the user width, so the stylesheet default applies again.
function setSidebarWidth(px, save = true) {
  const root = document.documentElement.style;
  if (px == null) root.removeProperty("--sidebar-width");
  else root.setProperty("--sidebar-width", `${clampSidebar(px)}px`);
  if (save) savePref("sidebarWidth", px == null ? null : clampSidebar(px));
  $("#sidebar-resizer").setAttribute("aria-valuenow", Math.round($("#sidebar").getBoundingClientRect().width));
}
(() => {
  const handle = $("#sidebar-resizer");
  const current = () => $("#sidebar").getBoundingClientRect().width;
  const saved = () => Number(loadPref("sidebarWidth", null)) || null;
  handle.setAttribute("aria-valuemin", SIDEBAR_MIN);
  handle.setAttribute("aria-valuemax", SIDEBAR_MAX);
  setSidebarWidth(saved(), false);
  handle.addEventListener("pointerdown", (e) => {
    if (e.button !== 0) return;
    e.preventDefault();
    handle.setPointerCapture(e.pointerId);
    const startX = e.clientX, startW = current();
    document.body.classList.add("resizing");
    const move = (ev) => setSidebarWidth(startW + ev.clientX - startX, false);
    const up = () => {
      handle.removeEventListener("pointermove", move);
      handle.removeEventListener("pointerup", up);
      handle.removeEventListener("pointercancel", up);
      document.body.classList.remove("resizing");
      savePref("sidebarWidth", Math.round(current()));
    };
    handle.addEventListener("pointermove", move);
    handle.addEventListener("pointerup", up);
    handle.addEventListener("pointercancel", up);
  });
  handle.addEventListener("dblclick", () => setSidebarWidth(null));
  handle.addEventListener("keydown", (e) => {
    const step = e.shiftKey ? 50 : 10;
    if (e.key === "ArrowLeft") setSidebarWidth(current() - step);
    else if (e.key === "ArrowRight") setSidebarWidth(current() + step);
    else if (e.key === "Home") setSidebarWidth(null);
    else return;
    e.preventDefault();
  });
  // A smaller window shrinks the menu if needed; the saved width is kept.
  window.addEventListener("resize", () => setSidebarWidth(saved(), false));
})();

// ---- agent access ------------------------------------------------------
$("#copy-mcp").addEventListener("click", async () => {
  try { await navigator.clipboard.writeText($("#mcp-cmd").textContent); toast("Copied"); }
  catch { toast("Select the command and copy it manually", true); }
});

// ---- boot ---------------------------------------------------------------
// After every script has run: modpacks.js and graph.js add to the mod list
// and diagnostics.
window.addEventListener("DOMContentLoaded", async () => {
  await busy(null, loadSources);
  await busy(null, detect);
  await busy(null, refreshNexus);
  await busy(null, refreshSso);
  $("#show-adult").checked = await invoke("nexus_show_adult");
  await busy(null, refreshLocation);
  $("#mcp-cmd").textContent = await invoke("mcp_command");
  const links = await invoke("startup_links");
  for (const l of links) await busy(null, () => handleNxm(l));
  // Quietly, so a slow or rate-limited source doesn't hold up the window.
  checkUpdates(true).catch(() => { $("#updates-note").textContent = ""; });
});
