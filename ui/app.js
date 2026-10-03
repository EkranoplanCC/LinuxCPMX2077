// UI for the mod manager. All data from Nexus is untrusted, so nothing is
// ever inserted as HTML: elements are built with textContent only.
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
  if (name === "downloads") loadDownloads();
  if (name === "nexus" && nexusUser && !browse) busy(null, () => runBrowse({ list: "trending" }));
  if (name === "analysis") runAnalysis();
  if (name === "crashes") busy(null, runCrash);
  if (name === "graph" && currentGame) busy(null, async () => window.showGraph(await invoke("analyze_game", { gameId: currentGame.id })));
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
  const g = currentGame.install;
  $("#game-info").replaceChildren(
    el("div", {}, "Version: ", el("b", {}, g.exe_product_version || g.exe_file_version || "unknown")),
    g.build_id ? el("div", {}, "Steam build: ", el("span", { class: "mono" }, g.build_id)) : null,
    g.proton_prefix ? el("div", { class: "muted mono" }, "Prefix: " + g.proton_prefix) : null,
  );
  $("#game-warnings").replaceChildren(...g.warnings.map((w) => el("li", {}, w)));
  $("#frameworks").replaceChildren(...g.frameworks.map((f) =>
    el("li", { class: f.installed ? "on" : "" }, f.name)));
  loadMods();
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
async function loadMods() {
  if (!currentGame) return;
  const mods = await invoke("list_mods", { gameId: currentGame.id });
  $("#mods-empty").classList.toggle("hidden", mods.length > 0);
  $("#mods-body").replaceChildren(...mods.map((m) => {
    const stale = m.game_build_id && currentGame.install.build_id && m.game_build_id !== currentGame.install.build_id;
    return el("tr", {},
      el("td", {}, el("b", {}, m.name), el("div", { class: "muted mono" }, m.archive_name)),
      el("td", {}, m.version || "—"),
      el("td", {}, m.source === "nexus" && m.nexus_mod_id
        ? el("span", { class: "badge" }, `Nexus #${m.nexus_mod_id}`)
        : el("span", { class: "badge" }, "Manual")),
      el("td", {}, m.file_count),
      el("td", {}, m.game_version || "—",
        stale ? el("div", { class: "badge bad", title: `Current game build is ${currentGame.install.build_id}` }, "game updated since") : null),
      el("td", { class: "actions" },
        el("button", { onclick: (e) => busy(e.target, () => verify(m)) }, "Verify"), " ",
        el("button", { class: "danger", onclick: (e) => busy(e.target, () => uninstall(m)) }, "Uninstall")),
    );
  }));
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

function reportInstall(r) {
  const lines = [`Installed ${r.name}: ${r.files_installed} files (${r.layout})`];
  if (r.overwritten_mods.length) lines.push(`Overrode files from: ${[...new Set(r.overwritten_mods.map((c) => c.other_mod_name))].join(", ")}`);
  if (r.backed_up_game_files.length) lines.push(`Backed up ${r.backed_up_game_files.length} original game files`);
  if (r.skipped.length) lines.push(`Skipped: ${r.skipped.slice(0, 5).join(", ")}${r.skipped.length > 5 ? "…" : ""}`);
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

async function runAnalysis() {
  if (!currentGame) return;
  $("#findings").replaceChildren(el("p", { class: "muted" }, "Checking…"));
  const r = await invoke("analyze_game", { gameId: currentGame.id });
  const sev = { error: "Problems", warning: "Overlaps to check", info: "Shared hooks (usually fine)" };
  const groups = ["error", "warning", "info"].map((s) => {
    const items = r.findings.filter((f) => f.severity === s);
    if (!items.length) return null;
    return el("div", { class: `card finding ${s}` },
      el("h3", {}, `${sev[s]} (${items.length})`),
      el("ul", {}, ...items.map((f) => el("li", {}, f.message, " ", el("span", { class: "muted mono" }, f.key)))));
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
$("#run-analysis").addEventListener("click", (e) => busy(e.target, runAnalysis));

// ---- crashes & logs -----------------------------------------------------
function fmtTime(unix) {
  return unix ? new Date(unix * 1000).toLocaleString() : "—";
}

async function runCrash() {
  if (!currentGame) return;
  const r = await invoke("crash_analysis", { gameId: currentGame.id });
  const errors = r.issues.filter((i) => i.level === "error");
  const warnings = r.issues.filter((i) => i.level === "warning");
  $("#crash-summary").replaceChildren(el("div", { class: "card finding " + (errors.length ? "error" : "ok") },
    r.latest_crash ? el("p", {}, "Latest crash report: ", el("b", {}, fmtTime(r.latest_crash.modified_unix))) : el("p", {}, "No crash reports found in the Proton prefix."),
    r.suspects.length
      ? el("p", {}, "Mods named in errors: ", ...r.suspects.flatMap(([name, n], i) => [i ? ", " : "", el("b", {}, name), ` (${n})`]))
      : el("p", { class: "muted" }, errors.length ? "None of the errors name an installed mod." : "No errors in the logs."),
  ));
  const group = (title, items, cls) => items.length ? el("div", { class: `card finding ${cls}` },
    el("h3", {}, `${title} (${items.length})`),
    el("ul", {}, ...items.slice(0, 200).map((i) => el("li", {},
      i.mod_names.length ? el("b", {}, i.mod_names.join(", ") + ": ") : null,
      el("span", { class: "mono" }, i.line), " ", el("span", { class: "muted" }, "· " + i.log))))) : null;
  $("#crash-issues").replaceChildren(...[group("Errors", errors, "error"), group("Warnings", warnings, "warning")].filter(Boolean));
  $("#crash-logs").replaceChildren(...r.logs.map((l) => el("tr", {},
    el("td", { class: "mono" }, l.name), el("td", {}, fmtSize(l.size)), el("td", {}, fmtTime(l.modified_unix)))));
}
$("#run-crash").addEventListener("click", (e) => busy(e.target, runCrash));

// ---- FOMOD wizard ------------------------------------------------------
async function handleOutcome(outcome) {
  if (outcome.status === "installed") {
    reportInstall(outcome.report);
  } else if (outcome.status === "needs_choices") {
    const report = await runWizard(outcome);
    if (report) reportInstall(report);
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
        "Free accounts download through the website's “Mod Manager Download” button, which sends an nxm:// link here."),
    );
  }
  if (s.error) toast(`Nexus: ${s.error}`, true);
}

$("#save-key").addEventListener("click", (e) => busy(e.target, async () => {
  await invoke("nexus_set_key", { key: $("#api-key").value });
  $("#api-key").value = "";
  await refreshNexus();
  toast("Connected to Nexus Mods");
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
$("#register-nxm").addEventListener("click", (e) => busy(e.target, async () => {
  await invoke("register_nxm_handler");
  toast("This app now opens nxm:// links");
}));

function parseModId(q) {
  q = q.trim();
  const m = q.match(/nexusmods\.com\/cyberpunk2077\/mods\/(\d+)/i) || q.match(/^(\d+)$/);
  return m ? Number(m[1]) : null;
}

$("#nexus-go").addEventListener("click", (e) => busy(e.target, async () => {
  const q = $("#nexus-query").value;
  if (q.trim().startsWith("nxm://")) return handleNxm(q.trim());
  const id = parseModId(q);
  if (!id) throw "Enter a Cyberpunk 2077 mod URL or numeric ID";
  await showMod(id);
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
    const [page, installed] = await Promise.all([
      next.list
        ? invoke("nexus_browse_list", { list: next.list })
        : invoke("nexus_search", { query: { text: next.text, sort: next.sort, offset: next.offset, count: PAGE_SIZE } }),
      installedNexusIds(),
    ]);
    if (req !== browseReq) return;
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
  return el("button", { class: "mod-card", title: m.name, onclick: (e) => busy(null, () => showMod(m.mod_id)) },
    nexusImage(m.picture_url, "thumb"),
    el("div", { class: "mod-card-body" },
      el("div", { class: "mod-card-title" }, m.name),
      el("div", { class: "muted small" }, `by ${m.author || "unknown"}${m.version ? ` · v${m.version}` : ""}`),
      el("div", { class: "summary" }, m.summary || ""),
      el("div", { class: "stats" },
        el("span", { title: "Endorsements" }, `♥ ${fmtCount(m.endorsements)}`),
        el("span", { title: "Downloads" }, `⬇ ${fmtCount(m.downloads)}`),
        el("span", { title: "Last updated" }, fmtDate(m.updated)),
        isInstalled ? el("span", { class: "badge ok" }, "installed") : null,
        m.adult ? el("span", { class: "badge bad" }, "adult") : null)));
}

function searchFromInputs(offset = 0) {
  return { text: $("#nexus-search").value.trim(), sort: $("#nexus-sort").value, offset };
}

$("#nexus-search-go").addEventListener("click", (e) => busy(e.target, () => runBrowse(searchFromInputs())));
$("#nexus-search").addEventListener("keydown", (e) => {
  if (e.key === "Enter") busy(null, () => runBrowse(searchFromInputs()));
});
document.querySelectorAll("#nexus-browse .chips button").forEach((b) => b.addEventListener("click", () => busy(b, () => {
  if (b.dataset.list) return runBrowse({ list: b.dataset.list });
  $("#nexus-search").value = "";
  $("#nexus-sort").value = b.dataset.sort;
  return runBrowse({ text: "", sort: b.dataset.sort, offset: 0 });
})));
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
          installed.has(modId) ? el("span", { class: "badge ok" }, "installed") : null,
          info.contains_adult_content ? el("span", { class: "badge bad" }, "adult") : null),
        el("p", {}, info.summary || ""))),
    desc,
    (d.description_text || "").length > 600 ? more : null,
    nexusUser?.is_premium ? null : el("p", { class: "muted" },
      "Free account: “Get from Nexus” opens the file on nexusmods.com. Click “Mod Manager Download” there and the manager downloads and installs it. "
      + "If nothing happens, turn on Settings → Handle nxm:// links."),
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
        : el("button", { onclick: (e) => busy(e.target, () => invoke("nexus_open_page", { modId, fileId: f.file_id })) }, "Get from Nexus")));
}

async function download(modId, fileId, key = null, expires = null) {
  if (!currentGame) throw "Select a game first";
  showTab("downloads");
  const r = await invoke("nexus_download", {
    modId, fileId, key, expires, installTo: currentGame.id, overwrite: $("#overwrite").checked,
  });
  $("#progress").classList.add("hidden");
  if (!r.download.verified) toast("Downloaded, but Nexus' checksum service was unreachable, so the file is unverified.", true);
  if (r.install) await handleOutcome(r.install);
  loadDownloads();
  loadMods();
}

async function handleNxm(url) {
  const link = await invoke("parse_nxm", { url });
  if (!nexusUser) { showTab("nexus"); throw "Connect your Nexus account first, then click the link again"; }
  if (link.expires && link.expires * 1000 < Date.now()) throw "This download link has expired; click it on Nexus again";
  showTab("nexus");
  // The page is a nicety; the download must not depend on it.
  await showMod(link.mod_id, link.file_id).catch(() => {});
  await download(link.mod_id, link.file_id, link.key, link.expires);
}

listen("nxm-link", (e) => busy(null, () => handleNxm(e.payload)));
listen("download-progress", (e) => {
  const { done, total } = e.payload;
  $("#progress").classList.remove("hidden");
  $("#progress-bar").style.width = total ? `${(100 * done) / total}%` : "0";
  $("#progress-text").textContent = `Downloading ${fmtSize(done)}${total ? " of " + fmtSize(total) : ""}`;
});

async function loadDownloads() {
  const rows = await invoke("list_downloads");
  $("#downloads-body").replaceChildren(...rows.map((d) => el("tr", {},
    el("td", {}, d.file_name),
    el("td", {}, d.nexus_mod_id ? `${d.nexus_mod_id} / ${d.nexus_file_id}` : "—"),
    el("td", {}, fmtSize(d.size)),
    el("td", {}, d.verified ? el("span", { class: "badge ok" }, "MD5 matches Nexus") : el("span", { class: "badge bad" }, "unverified")),
    el("td", { class: "mono", title: d.sha256 }, d.sha256.slice(0, 16) + "…"),
    el("td", { class: "actions" }, el("button", {
      onclick: (e) => busy(e.target, async () => {
        if (!currentGame) throw "Select a game first";
        const r = await invoke("install_archive", { gameId: currentGame.id, path: d.path, name: null, overwrite: $("#overwrite").checked });
        await handleOutcome(r);
        loadMods();
      }),
    }, "Install")),
  )));
}

// ---- agent access ------------------------------------------------------
$("#copy-mcp").addEventListener("click", async () => {
  try { await navigator.clipboard.writeText($("#mcp-cmd").textContent); toast("Copied"); }
  catch { toast("Select the command and copy it manually", true); }
});

// ---- boot ---------------------------------------------------------------
(async () => {
  await busy(null, detect);
  await busy(null, refreshNexus);
  await busy(null, refreshSso);
  $("#show-adult").checked = await invoke("nexus_show_adult");
  $("#mcp-cmd").textContent = await invoke("mcp_command");
  const links = await invoke("startup_links");
  for (const l of links) await busy(null, () => handleNxm(l));
})();
