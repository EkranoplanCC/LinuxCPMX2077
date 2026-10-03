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
  if (name === "analysis") runAnalysis();
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

async function showMod(modId, highlightFile) {
  const { info, files } = await invoke("nexus_mod", { modId });
  const sorted = [...files].sort((a, b) => (b.uploaded_timestamp || 0) - (a.uploaded_timestamp || 0));
  $("#nexus-result").replaceChildren(el("div", { class: "card" },
    el("h2", {}, info.name || `Mod ${modId}`),
    el("p", { class: "muted" }, `by ${info.author || "unknown"} · v${info.version || "?"}`),
    el("p", {}, info.summary || ""),
    ...sorted.filter((f) => f.category_name !== "ARCHIVED" && f.category_name !== "DELETED").map((f) =>
      el("div", { class: "file" },
        el("div", {},
          el("b", {}, f.name || f.file_name), " ",
          el("span", { class: "badge" }, f.category_name || ""), " ",
          f.file_id === highlightFile ? el("span", { class: "badge ok" }, "from link") : null,
          el("div", { class: "muted mono" }, `${f.file_name} · ${fmtSize(f.size_in_bytes)} · v${f.version || "?"}`)),
        el("div", {},
          nexusUser?.is_premium
            ? el("button", { class: "primary", onclick: (e) => busy(e.target, () => download(modId, f.file_id)) }, "Download & install")
            : el("span", { class: "muted" }, "Use “Mod Manager Download” on the website"))))));
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
  await showMod(link.mod_id, link.file_id);
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
  $("#mcp-cmd").textContent = await invoke("mcp_command");
  const links = await invoke("startup_links");
  for (const l of links) await busy(null, () => handleNxm(l));
})();
