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
  if (name === "diagnostics") busy(null, runDiagnostics);
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

const LAUNCH_OPTION = 'WINEDLLOVERRIDES="winmm,version=n,b" %command%';

function copyLaunchOption() {
  return el("button", {
    title: "Paste it in Steam: Cyberpunk 2077 › Properties › General › Launch options",
    onclick: async () => {
      try {
        await navigator.clipboard.writeText(LAUNCH_OPTION);
        toast("Copied. In Steam, open Cyberpunk 2077 › Properties › Launch options and paste it.");
      } catch {
        toast(`Copy this into Steam's launch options for Cyberpunk 2077:\n${LAUNCH_OPTION}`, true);
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
  if (c.id === "launch-options" && !ok) buttons.push(copyLaunchOption());
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
let modSort = loadPref("modSort", { key: "name", dir: 1 });
const NO_CATEGORY = "__none__";

function sourceLabel(m) {
  if (m.source === "nexus" && m.nexus_mod_id) return `Nexus #${m.nexus_mod_id}`;
  const info = sourceInfos.find((s) => s.id === m.source);
  if (info) return m.source_ref ? `${info.label} ${m.source_ref}` : info.label;
  return "Manual";
}

const SORT_KEYS = {
  name: (m) => m.name,
  category: (m) => m.category || "",
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
  renderMods();
  refreshGame().catch(() => {});
}

function renderMods() {
  const sel = $("#mods-category");
  const keep = sel.value;
  const cats = [...new Set(mods.map((m) => m.category).filter(Boolean))].sort((a, b) => a.localeCompare(b));
  sel.replaceChildren(el("option", { value: "" }, "All categories"),
    ...cats.map((c) => el("option", { value: c }, c)),
    cats.length && mods.some((m) => !m.category) ? el("option", { value: NO_CATEGORY }, "No category") : null);
  sel.value = [...sel.options].some((o) => o.value === keep) ? keep : "";
  const f = sel.value;
  const shown = sortMods(mods.filter((m) => !f || (f === NO_CATEGORY ? !m.category : m.category === f)));
  document.querySelectorAll("#tab-mods th.sortable").forEach((th) => {
    th.classList.toggle("asc", th.dataset.sort === modSort.key && modSort.dir > 0);
    th.classList.toggle("desc", th.dataset.sort === modSort.key && modSort.dir < 0);
  });
  $("#mods-empty").classList.toggle("hidden", mods.length > 0);
  $("#mods-body").replaceChildren(...shown.map(modRow));
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
    el("td", {}, m.category || "—"),
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

// Overview and diagnosis in one tab: a summary of what needs attention, the
// graph, the compatibility findings and the crash/log check. A finding or a
// crash suspect can be shown in the graph with the mods it names lit up.
async function runDiagnostics() {
  if (!currentGame) return;
  $("#diag-problems").replaceChildren(el("p", { class: "muted" }, "Checking…"));
  const [analysis, crash] = await Promise.allSettled([
    invoke("analyze_game", { gameId: currentGame.id }),
    invoke("crash_analysis", { gameId: currentGame.id }),
  ]);
  if (analysis.status === "fulfilled") {
    window.showGraph(analysis.value);
    renderCompat(analysis.value);
  }
  if (crash.status === "fulfilled") renderCrash(crash.value);
  renderProblems(analysis.value, crash.value);
  const failed = [analysis, crash].filter((r) => r.status === "rejected").map((r) => String(r.reason));
  if (failed.length) toast(failed.join("\n"), true);
}
$("#run-diagnostics").addEventListener("click", (e) => busy(e.target, runDiagnostics));
document.querySelectorAll("[data-jump]").forEach((b) => b.addEventListener("click", () =>
  document.getElementById(b.dataset.jump).scrollIntoView({ behavior: "smooth", block: "start" })));

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
    const errors = crash.issues.filter((i) => i.level === "error").length;
    const ids = suspectIds(crash);
    const lines = [];
    if (crash.latest_crash) lines.push(el("li", {}, "Latest crash report: ", el("b", {}, fmtTime(crash.latest_crash.modified_unix))));
    if (errors) lines.push(el("li", {}, `${errors} error${errors === 1 ? "" : "s"} in the logs`));
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
          errs.length ? el("li", {}, `${errs.length} problem${errs.length === 1 ? "" : "s"}, such as a missing framework or two mods replacing the same thing`) : null,
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
  const sev = { error: "Problems", warning: "Overlaps to check", info: "Shared hooks (usually fine)" };
  const groups = ["error", "warning", "info"].map((s) => {
    const items = r.findings.filter((f) => f.severity === s);
    if (!items.length) return null;
    return el("div", { class: `card finding ${s}` },
      el("h3", {}, `${sev[s]} (${items.length})`),
      el("ul", {}, ...items.map((f) => el("li", {}, f.message, " ", el("span", { class: "muted mono" }, f.key), " ", graphButton(f.mod_ids, f.message)))));
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

function renderCrash(r) {
  const ids = suspectIds(r);
  const errors = r.issues.filter((i) => i.level === "error");
  const warnings = r.issues.filter((i) => i.level === "warning");
  $("#crash-summary").replaceChildren(el("div", { class: "card finding " + (errors.length ? "error" : "ok") },
    r.latest_crash ? el("p", {}, "Latest crash report: ", el("b", {}, fmtTime(r.latest_crash.modified_unix))) : el("p", {}, "No crash reports found in the Proton prefix."),
    r.suspects.length
      ? el("p", {}, "Mods named in errors: ", ...r.suspects.flatMap(([name, n], i) => [i ? ", " : "", el("b", {}, name), ` (${n})`]), " ",
        graphButton([...new Set(r.suspects.flatMap(([name]) => [...(ids.get(name) || [])]))], "Mods named in log errors"))
      : el("p", { class: "muted" }, errors.length ? "None of the errors name an installed mod." : "No errors in the logs."),
  ));
  const group = (title, items, cls) => items.length ? el("div", { class: `card finding ${cls}` },
    el("h3", {}, `${title} (${items.length})`),
    el("ul", {}, ...items.slice(0, 200).map((i) => el("li", {},
      i.mod_names.length ? el("b", {}, i.mod_names.join(", ") + ": ") : null,
      el("span", { class: "mono" }, i.line), " ", el("span", { class: "muted" }, "· " + i.log), " ",
      graphButton(i.mod_ids, `${i.mod_names.join(", ")}: ${i.log}`))))) : null;
  $("#crash-issues").replaceChildren(...[group("Errors", errors, "error"), group("Warnings", warnings, "warning")].filter(Boolean));
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
    box.replaceChildren(
      el("b", {}, s.user.name), " ",
      el("span", { class: "badge" }, s.user.is_premium ? "Premium" : "Free account"), " ",
      s.user.is_premium ? null : el("p", { class: "muted" },
        "Nexus only lets Premium accounts download straight through apps like this one. With a free account, "
        + "“Get from Nexus” opens the file on Nexus in a CPMX2077 window instead: sign in there once, click "
        + "“Slow download” and wait for the short countdown. CPMX2077 then downloads, checks and installs the file by itself."),
    );
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
  if (currentSource === "nexus" && !browse) await runBrowse({ list: "trending", category: selectedCategory() });
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
async function getFromNexus(modId, fileId) {
  await invoke("nexus_open_in_app", { modId, fileId });
  toast("Sign in to Nexus in the new window if it asks, then click “Slow download”. CPMX2077 downloads and installs the file by itself.");
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

function parseModId(q) {
  q = q.trim();
  const m = q.match(/nexusmods\.com\/cyberpunk2077\/mods\/(\d+)/i) || q.match(/^(\d+)$/);
  return m ? Number(m[1]) : null;
}

$("#nexus-go").addEventListener("click", (e) => busy(e.target, async () => {
  const q = $("#nexus-query").value;
  if (q.trim().startsWith("nxm://")) return handleNxm(q.trim());
  const id = parseModId(q);
  if (id) return showMod(id);
  const ref = await invoke("source_resolve", { input: q });
  if (!ref) throw "Enter a Cyberpunk 2077 mod URL or numeric ID, or a GitHub repository link";
  await selectSource(ref.source, false);
  await showSourceDetails(ref.source, ref.id);
}));

// ---- nexus browsing -----------------------------------------------------
const PAGE_SIZE = 20;
let browse = null; // { list } or { text, sort, offset }
let browseReq = 0;

function fmtCount(n) {
  if (n === null || n === undefined) return "—";
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)}M`;
  if (n >= 1e4) return `${Math.round(n / 1e3)}k`;
  return n.toLocaleString();
}

function fmtDate(unix) {
  return unix ? new Date(unix * 1000).toLocaleDateString() : "—";
}

// Only Nexus' image CDN is allowed (the CSP enforces the same).
function nexusImage(url, cls) {
  if (typeof url !== "string" || !url.startsWith("https://staticdelivery.nexusmods.com/")) return el("div", { class: `${cls} noimg` });
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

const LIST_TITLES = { trending: "Trending on Nexus", latest_added: "Latest added", latest_updated: "Latest updated" };
const SORT_TITLES = { endorsements: "Most endorsed", downloads: "Most downloaded", updated: "Recently updated", created: "Newest", relevance: "Best match" };

async function runBrowse(next) {
  browse = next;
  const req = ++browseReq;
  document.querySelectorAll("#nexus-browse .chips button").forEach((b) =>
    b.classList.toggle("active", (next.list && b.dataset.list === next.list) || (!next.list && !next.text && b.dataset.sort === next.sort)));
  $("#nexus-result").classList.add("hidden");
  $("#nexus-list").classList.remove("hidden");
  $("#nexus-list-title").textContent = "Loading…";
  try {
    const category = categoriesById.get(next.category);
    const [page, installed] = await Promise.all([
      next.list
        ? invoke("nexus_browse_list", { list: next.list })
        : invoke("nexus_search", { query: { text: next.text, sort: next.sort, offset: next.offset, count: PAGE_SIZE, category: category?.name ?? null } }),
      installedNexusIds(),
    ]);
    if (req !== browseReq) return;
    // Curated lists can't be asked for one category: filter what they return.
    const ids = next.list ? categoryIds(next.category) : null;
    if (ids) page.mods = page.mods.filter((m) => ids.has(m.category_id));
    renderPage(page, installed);
  } catch (e) {
    if (req === browseReq) $("#nexus-list-title").textContent = "";
    throw e;
  } finally {
    refreshQuota().catch(() => {});
  }
}

function renderPage(page, installed) {
  let title = browse.list ? LIST_TITLES[browse.list]
    : browse.text ? `Results for “${browse.text}”` : SORT_TITLES[browse.sort];
  const category = categoriesById.get(browse.category);
  if (category) title += browse.list ? ` · only ${category.name}` : ` in ${category.name}`;
  if (page.total !== null && page.total !== undefined) title += ` · ${page.total.toLocaleString()} mods`;
  if (page.hidden_adult) title += ` · ${page.hidden_adult} adult ${page.hidden_adult === 1 ? "mod" : "mods"} hidden (Settings)`;
  $("#nexus-list-title").textContent = title;
  $("#nexus-grid").replaceChildren(...page.mods.map((m) => modCard(m, installed.has(m.mod_id))));
  if (!page.mods.length) $("#nexus-grid").append(el("p", { class: "muted" }, "No mods found."));
  const paged = !browse.list && page.total !== null && page.total !== undefined;
  $("#nexus-pager").classList.toggle("hidden", !paged);
  if (paged) {
    const pageNo = Math.floor(page.offset / PAGE_SIZE) + 1;
    const pages = Math.max(1, Math.ceil(page.total / PAGE_SIZE));
    $("#nexus-page").textContent = `Page ${pageNo} of ${pages.toLocaleString()}`;
    $("#nexus-prev").disabled = page.offset <= 0;
    $("#nexus-next").disabled = page.offset + page.count >= page.total;
  }
}

function modCard(m, isInstalled) {
  const seed = { kind: "nexus", name: m.name, modId: m.mod_id };
  return el("button", { class: cardClass(seed), title: m.name,
    onclick: (e) => selectMode ? toggleSelected(seed, e.currentTarget) : busy(null, () => showMod(m.mod_id)) },
    nexusImage(m.picture_url, "thumb"),
    el("div", { class: "mod-card-body" },
      el("div", { class: "mod-card-title" }, m.name),
      el("div", { class: "muted small" }, `by ${m.author || "unknown"}${m.version ? ` · v${m.version}` : ""}`),
      el("div", { class: "summary" }, m.summary || ""),
      el("div", { class: "stats" },
        el("span", { title: "Endorsements" }, `♥ ${fmtCount(m.endorsements)}`),
        el("span", { title: "Downloads" }, `⬇ ${fmtCount(m.downloads)}`),
        el("span", { title: "Last updated" }, fmtDate(m.updated)),
        categoriesById.has(m.category_id) ? el("span", { class: "badge" }, categoriesById.get(m.category_id).name) : null,
        isInstalled ? el("span", { class: "badge ok" }, "installed") : null,
        m.adult ? el("span", { class: "badge bad" }, "adult") : null)));
}

function selectedCategory() {
  return Number($("#nexus-category").value) || null;
}

function searchFromInputs(offset = 0) {
  return { text: $("#nexus-search").value.trim(), sort: $("#nexus-sort").value, offset, category: selectedCategory() };
}

$("#nexus-search-go").addEventListener("click", (e) => busy(e.target, () => runBrowse(searchFromInputs())));
$("#nexus-search").addEventListener("keydown", (e) => {
  if (e.key === "Enter") busy(null, () => runBrowse(searchFromInputs()));
});
document.querySelectorAll("#nexus-browse .chips button").forEach((b) => b.addEventListener("click", () => busy(b, () => {
  if (b.dataset.list) return runBrowse({ list: b.dataset.list, category: selectedCategory() });
  $("#nexus-search").value = "";
  $("#nexus-sort").value = b.dataset.sort;
  return runBrowse({ text: "", sort: b.dataset.sort, offset: 0, category: selectedCategory() });
})));
$("#nexus-category").addEventListener("change", () => busy(null, () => runBrowse(searchFromInputs())));
$("#nexus-prev").addEventListener("click", (e) => busy(e.target, () => runBrowse({ ...browse, offset: Math.max(0, browse.offset - PAGE_SIZE) })));
$("#nexus-next").addEventListener("click", (e) => busy(e.target, () => runBrowse({ ...browse, offset: browse.offset + PAGE_SIZE })));

$("#show-adult").addEventListener("change", (e) => busy(null, async () => {
  await invoke("set_nexus_show_adult", { show: e.target.checked });
  if (browse) await runBrowse(browse);
}));

const FILE_GROUPS = [
  ["MAIN", "Main files"], ["UPDATE", "Updates"], ["OPTIONAL", "Optional files"],
  ["MISCELLANEOUS", "Miscellaneous"], ["OLD_VERSION", "Old versions"],
];

async function showMod(modId, highlightFile) {
  const [d, installed] = await Promise.all([invoke("nexus_mod_details", { modId }), installedNexusIds()]);
  refreshQuota().catch(() => {});
  const { info, files } = d;
  const sorted = [...files].sort((a, b) => (b.uploaded_timestamp || 0) - (a.uploaded_timestamp || 0));
  const groups = FILE_GROUPS.map(([cat, label]) => {
    const fs = sorted.filter((f) => (f.category_name || "MISCELLANEOUS") === cat
      || (cat === "MISCELLANEOUS" && !FILE_GROUPS.some(([c]) => c === f.category_name) && f.category_name !== "ARCHIVED" && f.category_name !== "DELETED"));
    if (!fs.length) return null;
    const body = fs.map((f) => fileRow(modId, f, highlightFile));
    return cat === "OLD_VERSION"
      ? el("details", {}, el("summary", {}, `${label} (${fs.length})`), ...body)
      : el("div", {}, el("h3", {}, label), ...body);
  });
  const desc = el("div", { class: "description collapsed" }, d.description_text || info.summary || "");
  const more = el("button", { class: "link", onclick: () => {
    desc.classList.toggle("collapsed");
    more.textContent = desc.classList.contains("collapsed") ? "Show full description" : "Show less";
  } }, "Show full description");
  $("#nexus-list").classList.toggle("hidden", !!browse);
  $("#nexus-result").classList.remove("hidden");
  $("#nexus-result").replaceChildren(el("div", { class: "card mod-detail" },
    el("div", { class: "row" },
      browse ? el("button", { onclick: () => {
        $("#nexus-result").classList.add("hidden");
        $("#nexus-list").classList.remove("hidden");
      } }, "← Back to results") : null,
      el("span", { class: "spacer" }),
      el("button", { onclick: (e) => busy(e.target, () => invoke("nexus_open_page", { modId, fileId: null })) }, "Open on nexusmods.com")),
    el("div", { class: "mod-head" },
      nexusImage(info.picture_url, "hero"),
      el("div", {},
        el("h2", {}, info.name || `Mod ${modId}`),
        el("p", { class: "muted" }, `by ${info.author || info.uploaded_by || "unknown"} · v${info.version || "?"}`),
        el("div", { class: "stats" },
          el("span", {}, `♥ ${fmtCount(info.endorsement_count)} endorsements`),
          el("span", {}, `⬇ ${fmtCount(info.mod_downloads)} downloads`),
          el("span", {}, `Updated ${fmtDate(info.updated_timestamp)}`),
          categoriesById.has(info.category_id) ? el("span", { class: "badge" }, categoriesById.get(info.category_id).name) : null,
          installed.has(modId) ? el("span", { class: "badge ok" }, "installed") : null,
          info.contains_adult_content ? el("span", { class: "badge bad" }, "adult") : null),
        el("p", {}, info.summary || ""))),
    desc,
    (d.description_text || "").length > 600 ? more : null,
    nexusUser?.is_premium ? null : el("p", { class: "muted" },
      "Free account: “Get from Nexus” opens the file in a CPMX2077 window. Click “Slow download” there and the file downloads and installs here by itself. "
      + "Prefer your own browser? Use “or use your browser”."),
    ...groups));
  $("main").scrollTop = 0;
}

function fileRow(modId, f, highlightFile) {
  return el("div", { class: "file" },
    el("div", {},
      el("b", {}, f.name || f.file_name), " ",
      f.is_primary ? el("span", { class: "badge ok" }, "primary") : null, " ",
      f.file_id === highlightFile ? el("span", { class: "badge ok" }, "from link") : null,
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
  showTab("downloads");
  try {
    const r = await invoke("nexus_download", {
      modId, fileId, key, expires, installTo: currentGame.id, overwrite: $("#overwrite").checked, replaces,
    });
    const note = r.download.verified ? null : "Nexus' checksum lookup was unreachable, so the file is unverified.";
    if (r.install) await handleOutcome(r.install, note);
    else if (note) toast(note, true);
    if (replaces && updatesByMod.delete(replaces)) updatesChanged();
  } finally {
    $("#progress").classList.add("hidden");
    loadDownloads().catch(() => {});
    loadMods().catch(() => {});
  }
}

async function handleNxm(url) {
  const link = await invoke("parse_nxm", { url });
  if (!nexusUser) { showTab("nexus"); throw "Connect your Nexus account first, then click the link again"; }
  if (link.expires && link.expires * 1000 < Date.now()) throw "This download link has expired; click it on Nexus again";
  // The update for an installed mod replaces the old version.
  const replaces = [...updatesByMod.values()].find((u) => u.nexus_mod_id === link.mod_id && u.nexus_file_id === link.file_id)?.mod_id ?? null;
  await selectSource("nexus", false);
  showTab("nexus");
  // The page is a nicety; the download must not depend on it.
  await showMod(link.mod_id, link.file_id).catch(() => {});
  await download(link.mod_id, link.file_id, link.key, link.expires, replaces);
}

function showProgress(done, total, label = "Downloading") {
  $("#progress").classList.remove("hidden");
  $("#progress-bar").style.width = total ? `${(100 * done) / total}%` : "0";
  $("#progress-text").textContent = `${label} ${fmtSize(done)}${total ? " of " + fmtSize(total) : ""}`;
}

listen("nxm-link", (e) => busy(null, async () => {
  if (queueTakeNxm(await invoke("parse_nxm", { url: e.payload }))) return;
  await handleNxm(e.payload);
}));
listen("download-progress", (e) => {
  const { mod_id, file_id, done, total } = e.payload;
  showProgress(done, total);
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
    if (load && nexusUser && !browse) await runBrowse({ list: "trending", category: selectedCategory() });
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
  showTab("downloads");
  try {
    const r = await invoke("source_download", {
      source, id, fileId: f.id, installTo: install ? currentGame.id : null, overwrite: $("#overwrite").checked, replaces,
    });
    const note = r.download.verified ? null : `${r.download.check}, so the file is unverified.`;
    if (r.install) await handleOutcome(r.install, note);
    else toast([`Downloaded ${r.download.file_name}`, note].filter(Boolean).join("\n"));
    if (replaces && updatesByMod.delete(replaces)) updatesChanged();
  } finally {
    $("#progress").classList.add("hidden");
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
    if (!r.download.verified) it.note = it.kind === "nexus" ? "unverified: Nexus checksum lookup failed" : r.download.check;
  } catch (e) {
    return finishItem(it, "failed", String(e));
  } finally {
    $("#progress").classList.add("hidden");
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
async function loadDownloads() {
  const [rows, installed] = await Promise.all([
    invoke("list_downloads"),
    currentGame ? invoke("list_mods", { gameId: currentGame.id }).catch(() => []) : [],
  ]);
  $("#downloads-empty").classList.toggle("hidden", rows.length > 0);
  $("#downloads-body").replaceChildren(...rows.map((d) => {
    const m = installed.find((x) => x.archive_sha256 && x.archive_sha256 === d.sha256);
    const up = m && updatesByMod.get(m.id);
    const checked = d.checked || (d.verified ? "MD5 matches Nexus" : "unverified");
    return el("tr", {},
      el("td", {}, el("b", {}, d.mod_name || d.file_name),
        m ? el("span", { class: "badge ok" }, m.status === "installed" ? "installed" : "installed, off") : null,
        el("div", { class: "muted mono" }, d.file_name)),
      el("td", {}, d.version || "—", up ? el("div", { class: "badge ok" }, `${up.latest} available`) : null),
      el("td", {}, d.game_version || "—"),
      el("td", {}, el("span", { class: "badge" }, sourceLabel({ ...d, source: d.source || "nexus" }))),
      el("td", {}, fmtSize(d.size)),
      el("td", {}, el("span", { class: d.verified ? "badge ok" : "badge bad", title: `SHA-256 ${d.sha256}` }, checked)),
      el("td", { class: "actions" },
        up ? el("button", {
        class: "update",
        title: up.to_stable ? `${m.version} is a pre-release (a test build). ${up.latest} is the release most mods are built against.` : "",
        onclick: (e) => busy(e.target, () => applyUpdate(up)),
      }, up.to_stable ? `Switch to stable ${up.latest}` : `Update to ${up.latest}`) : null, " ",
        m ? null : el("button", {
          onclick: (e) => busy(e.target, async () => {
            if (!currentGame) throw "Select a game first";
            const r = await invoke("install_download", { downloadId: d.id, gameId: currentGame.id, overwrite: $("#overwrite").checked, replaces: null });
            await handleOutcome(r);
            loadDownloads().catch(() => {});
          }),
        }, "Install")));
  }));
}

// ---- agent access ------------------------------------------------------
$("#copy-mcp").addEventListener("click", async () => {
  try { await navigator.clipboard.writeText($("#mcp-cmd").textContent); toast("Copied"); }
  catch { toast("Select the command and copy it manually", true); }
});

// ---- boot ---------------------------------------------------------------
(async () => {
  await busy(null, loadSources);
  await busy(null, detect);
  await busy(null, refreshNexus);
  await busy(null, refreshSso);
  $("#show-adult").checked = await invoke("nexus_show_adult");
  $("#mcp-cmd").textContent = await invoke("mcp_command");
  const links = await invoke("startup_links");
  for (const l of links) await busy(null, () => handleNxm(l));
  // Quietly, so a slow or rate-limited source doesn't hold up the window.
  checkUpdates(true).catch(() => { $("#updates-note").textContent = ""; });
})();
