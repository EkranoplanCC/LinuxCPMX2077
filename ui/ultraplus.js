// Ultra+ recommendations: when Ultra+ (the path tracing mod) is installed,
// a card in Installed mods lists what the Ultra+ team's page recommends next
// to it, what conflicts with it, and where each of those stands here.
// Loaded after app.js and built from its helpers (el, $, busy, enqueue, …).
// Names and notes from the page are inserted as text, never as HTML.

let ultraOpen = loadPref("ultraOpen", true);

const ULTRA_STATE = {
  installed: ["installed", "ok"],
  present: ["in game folder", "ok"],
  disabled: ["turned off", "bad"],
  missing: ["not installed", ""],
};

async function loadUltraPlus(refresh = false) {
  const box = $("#ultraplus");
  if (!currentGame) return box.classList.add("hidden");
  const gameId = currentGame.id;
  const r = await invoke(refresh ? "ultraplus_refresh" : "ultraplus_report", { gameId });
  if (currentGame?.id !== gameId) return;
  box.classList.toggle("hidden", !r.detected);
  if (r.detected) renderUltraPlus(r);
}

function ultraState(it, conflict) {
  const [label, cls] = ULTRA_STATE[it.state] || [it.state, ""];
  // An installed conflict is the bad case; a missing one is fine.
  const c = conflict ? (it.state === "missing" ? "ok" : "bad") : cls;
  return el("span", { class: `badge ${c}` }, conflict && it.state === "missing" ? "not installed" : label);
}

function ultraName(it) {
  if (it.nexus_mod_id) {
    return el("a", { href: "#", title: "Open its page in Get mods",
      onclick: (e) => { e.preventDefault(); busy(null, () => showMod(it.nexus_mod_id)); } }, it.name);
  }
  return it.url ? docLink(it.url, [it.name]) : el("span", {}, it.name);
}

function ultraAction(it, conflict) {
  const m = it.installed_id ? mods.find((x) => x.id === it.installed_id) : null;
  if (conflict) {
    if (m && it.state === "installed") {
      return el("button", { onclick: (e) => busy(e.target, () => setEnabled(m, false)).finally(loadMods) }, "Turn off");
    }
    return null;
  }
  if (m && it.state === "disabled") {
    return el("button", { onclick: (e) => busy(e.target, () => setEnabled(m, true)).finally(loadMods) }, "Turn on");
  }
  if (it.state === "missing" && it.nexus_mod_id) {
    return el("button", { onclick: (e) => busy(e.target, async () => enqueue([{ kind: "nexus", name: it.name, modId: it.nexus_mod_id }])) }, "Get");
  }
  return null;
}

function ultraTable(items, conflict) {
  return el("table", { class: "req-table" },
    el("tbody", {}, ...items.map((it) => el("tr", {},
      el("td", {}, ultraName(it)),
      el("td", {}, ultraState(it, conflict)),
      el("td", { class: "muted" }, it.note || ""),
      el("td", { class: "actions" }, ultraAction(it, conflict))))));
}

function ultraGetAll(items, label) {
  const seeds = items.filter((it) => it.state === "missing" && it.nexus_mod_id)
    .map((it) => ({ kind: "nexus", name: it.name, modId: it.nexus_mod_id }));
  return seeds.length
    ? el("button", { onclick: (e) => busy(e.target, async () => enqueue(seeds)) }, `${label} (${seeds.length})`)
    : null;
}

function renderUltraPlus(r) {
  const clashes = r.conflicts.filter((it) => it.state === "installed" || it.state === "present");
  const missing = r.recommended.filter((it) => it.state === "missing").length;
  const needs = r.required.filter((it) => it.state === "missing" || it.state === "disabled");
  const summary = [
    clashes.length ? `${clashes.length} conflicting mod${clashes.length === 1 ? "" : "s"} installed` : "no conflicts",
    `${r.recommended.length - missing} of ${r.recommended.length} recommended mods`,
  ];
  if (needs.length) summary.unshift(`needs ${needs.map((it) => it.name).join(", ")}`);
  const source = r.fetched_at
    ? `From the Ultra+ page, read ${fmtAgo(r.fetched_at)}.`
    : `Built-in copy of the Ultra+ page from ${r.checked || "this release"}.`;
  const details = el("details", { open: ultraOpen ? "" : null },
    el("summary", {}, el("b", {}, "Ultra+ recommendations"), el("span", { class: "muted" }, ` · ${summary.join(" · ")}`)),
    el("p", { class: "muted small" }, source, " ",
      "The Ultra+ team lists these mods on their Cyberpunk page; “not installed” means not installed through CPMX2077."),
    el("div", { class: "row" },
      el("button", { title: "Read the lists again from the Ultra+ team's page", onclick: (e) => busy(e.target, async () => {
        await loadUltraPlus(true);
        toast("Updated the Ultra+ recommendations from their page");
      }) }, "Refresh from Ultra+ page"),
      docLink(r.page_url, ["Open the Ultra+ page"]),
      r.collection ? el("button", { title: "The modpack the Ultra+ page suggests instead of installing these one by one",
        onclick: (e) => busy(e.target, () => showCollection(r.collection.slug)) }, `Open ${r.collection.name} modpack`) : null),
    needs.length ? el("h3", {}, "Required") : null,
    needs.length ? ultraTable(r.required, false) : null,
    el("h3", {}, "Conflicts with Ultra+ path tracing"),
    ultraTable(r.conflicts, true),
    r.ray_tracing.length ? el("p", { class: "muted small" },
      "Some of these are fine with ray tracing: the Ultra+ team recommends them when you play with ray tracing instead of path tracing (see below).") : null,
    el("h3", {}, "Recommended for path tracing and ray tracing"),
    ultraTable(r.recommended, false),
    el("div", { class: "row" }, ultraGetAll(r.recommended, "Get all missing")),
    r.ray_tracing.length ? el("h3", {}, "Also recommended for ray tracing only") : null,
    r.ray_tracing.length ? el("p", { class: "muted small" }, "Ultra+ doesn't fix ray traced lighting; these mods do. Don't use them with path tracing.") : null,
    r.ray_tracing.length ? ultraTable(r.ray_tracing, false) : null);
  details.addEventListener("toggle", () => { ultraOpen = details.open; savePref("ultraOpen", ultraOpen); });
  $("#ultraplus").replaceChildren(details);
}
