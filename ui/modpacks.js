// Modpacks: Nexus collections the user follows, their own mod categories,
// mod list import/export, and the dependency rows under each installed mod.
// Loaded after app.js and built from its helpers (el, $, busy, enqueue, …).
// As there, nothing from Nexus or an imported file is inserted as HTML.

// ---- dependencies in Installed mods -------------------------------------
let depsByMod = null; // installed mod id -> { deps, required_by } while shown
let showDeps = loadPref("showDeps", false);
$("#show-deps").checked = showDeps;

// Custom categories and dependencies for the mod list; called by loadMods.
async function loadModExtras(refreshDeps = false) {
  if (!currentGame) return;
  const gameId = currentGame.id;
  const [cats, deps] = await Promise.all([
    invoke("custom_categories", { gameId }),
    showDeps ? invoke("mod_dependencies", { gameId, refresh: refreshDeps }) : null,
  ]);
  if (currentGame?.id !== gameId) return;
  customCategories = cats;
  depsByMod = deps ? new Map(deps.mods.map((d) => [d.mod_id, d])) : null;
  $("#deps-note").textContent = deps?.note || "";
}

$("#show-deps").addEventListener("change", (e) => busy(e.target, async () => {
  showDeps = e.target.checked;
  savePref("showDeps", showDeps);
  if (showDeps) $("#deps-note").textContent = "Looking up what your mods need…";
  else $("#deps-note").textContent = "";
  await loadModExtras();
  renderMods();
}));

const DEP_STATE = {
  installed: ["ok", "installed"],
  disabled: ["bad", "turned off"],
  present: ["ok", "in the game folder"],
  missing: ["bad", "missing"],
  unknown: ["", "check yourself"],
};

// Rows shown indented under an installed mod: who needs it, then what it needs.
function dependencyRows(m) {
  const d = depsByMod?.get(m.id);
  if (!d) return [];
  const off = m.status === "installed" ? "" : " off";
  const rows = [];
  if (d.required_by.length) {
    const names = d.required_by.map((id) => mods.find((x) => x.id === id)?.name).filter(Boolean);
    rows.push(el("tr", { class: `dep${off}` }, el("td", {}),
      el("td", { colspan: 7, class: "muted small" }, `↰ Needed by ${names.join(", ")}`)));
  }
  for (const dep of d.deps) {
    const [cls, label] = DEP_STATE[dep.state] || DEP_STATE.unknown;
    const why = dep.listed && dep.detected ? "listed on its Nexus page and used by its files"
      : dep.listed ? "listed on its Nexus page" : "its files use it";
    rows.push(el("tr", { class: `dep${off}` },
      el("td", {}),
      el("td", { colspan: 6 },
        el("span", { class: "dep-name" }, "↳ ", dep.dlc ? `${dep.name} (expansion)` : dep.name), " ",
        el("span", { class: `badge ${cls}` }, label), " ",
        el("span", { class: "muted small" }, why),
        dep.notes ? el("div", { class: "muted small dep-note" }, `Author's note: ${dep.notes}`) : null),
      el("td", { class: "actions" }, dependencyAction(dep))));
  }
  if (!d.deps.length && !d.required_by.length) {
    rows.push(el("tr", { class: `dep${off}` }, el("td", {}), el("td", { colspan: 7, class: "muted small" }, "↳ Needs nothing else that CPMX2077 knows of")));
  }
  return rows;
}

function dependencyAction(dep) {
  const target = dep.installed_id && mods.find((x) => x.id === dep.installed_id);
  if (dep.state === "disabled" && target) {
    return el("button", { onclick: (e) => busy(e.target, () => setEnabled(target, true)).finally(loadMods) }, "Turn on");
  }
  if (dep.state !== "missing") return null;
  if (dep.framework && dep.framework !== "redmod") {
    return el("button", { title: "Installs it from GitHub, with anything it needs first",
      onclick: (e) => busy(e.target, () => installFrameworks([dep.framework])) }, "Install");
  }
  if (dep.nexus_mod_id) {
    return el("button", { onclick: (e) => busy(e.target, async () => enqueue([{ kind: "nexus", name: dep.name, modId: dep.nexus_mod_id }])) },
      "Get from Nexus");
  }
  if (dep.url) return el("button", { title: dep.url, onclick: (e) => busy(e.target, () => invoke("open_url", { url: dep.url })) }, "Open link");
  return null;
}

// ---- your own categories --------------------------------------------------
const COLOR = /^#[0-9a-f]{6}$/i;

// The category picker in a mod's row; nothing until the user makes one.
function customCategoryPicker(m) {
  const own = customCategories.categories;
  if (!own.length) return null;
  const current = customCategories.mods[m.id] || "";
  const sel = el("select", { class: "inline cat-pick", "aria-label": `Your category for ${m.name}` },
    el("option", { value: "" }, "—"), ...own.map((c) => el("option", { value: c.name }, c.name)));
  sel.value = current;
  const color = own.find((c) => c.name === current)?.color;
  if (color && COLOR.test(color)) sel.style.borderLeftColor = color;
  sel.addEventListener("change", () => busy(sel, async () => {
    await invoke("set_mod_custom_category", { modId: m.id, category: sel.value || null });
    if (sel.value) customCategories.mods[m.id] = sel.value;
    else delete customCategories.mods[m.id];
    renderMods();
  }));
  return sel;
}

async function renderCategories() {
  if (!currentGame) return;
  customCategories = await invoke("custom_categories", { gameId: currentGame.id });
  const counts = {};
  for (const name of Object.values(customCategories.mods)) counts[name] = (counts[name] || 0) + 1;
  $("#cat-list").replaceChildren(...customCategories.categories.map((c) => categoryItem(c, counts[c.name] || 0)));
}

function categoryItem(c, count) {
  const color = el("input", { type: "color", title: "Color", value: COLOR.test(c.color || "") ? c.color : "#8a8aa0" });
  color.addEventListener("change", () => busy(color, async () => {
    await invoke("edit_custom_category", { name: c.name, newName: c.name, color: color.value });
    await renderCategories();
    renderMods();
  }));
  const name = el("span", { class: "cat-name" }, c.name);
  const li = el("li", {}, color, name, el("span", { class: "muted small" }, `${count} mod${count === 1 ? "" : "s"}`),
    el("span", { class: "spacer" }),
    el("button", { class: "link inline", onclick: () => rename() }, "Rename"),
    el("button", { class: "link inline danger", onclick: (e) => busy(e.target, async () => {
      const ok = await dialog.ask(`Delete the category “${c.name}”? Its ${count} mod${count === 1 ? "" : "s"} stay installed, just without a category.`,
        { title: "Delete category", kind: "warning" });
      if (!ok) return;
      await invoke("delete_custom_category", { name: c.name });
      await renderCategories();
      renderMods();
    }) }, "Delete"));
  function rename() {
    const input = el("input", { value: c.name, maxlength: 60, "aria-label": "New name" });
    const save = () => busy(input, async () => {
      if (input.value.trim() && input.value.trim() !== c.name) {
        await invoke("edit_custom_category", { name: c.name, newName: input.value, color: c.color ?? null });
      }
      await renderCategories();
      renderMods();
    });
    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter") save();
      if (e.key === "Escape") renderCategories();
    });
    name.replaceWith(input, el("button", { onclick: save }, "Save"));
    input.focus();
  }
  return li;
}

async function addCategory() {
  const name = $("#cat-name").value;
  if (!name.trim()) throw "Type a name for the category";
  await invoke("add_custom_category", { name, color: $("#cat-color").value });
  $("#cat-name").value = "";
  await renderCategories();
  renderMods();
}
$("#cat-add").addEventListener("click", (e) => busy(e.target, addCategory));
$("#cat-name").addEventListener("keydown", (e) => {
  if (e.key === "Enter") busy(null, addCategory);
});

// ---- mod lists -------------------------------------------------------------
$("#modlist-export").addEventListener("click", (e) => busy(e.target, async () => {
  if (!currentGame) throw "Select a game first";
  const path = await dialog.save({ title: "Export mod list", defaultPath: "cpmx2077-mods.json",
    filters: [{ name: "CPMX2077 mod list", extensions: ["json"] }] });
  if (!path) return;
  const n = await invoke("export_modlist", { gameId: currentGame.id, path });
  toast(`Saved ${n} mod${n === 1 ? "" : "s"}, your categories and followed collections`);
}));

$("#modlist-import").addEventListener("click", (e) => busy(e.target, async () => {
  if (!currentGame) throw "Select a game first";
  const path = await dialog.open({ title: "Import mod list", filters: [{ name: "CPMX2077 mod list", extensions: ["json"] }] });
  if (!path) return;
  const r = await invoke("import_modlist", { gameId: currentGame.id, path });
  showModpacksMain();
  renderImport(r);
  await renderCategories();
  await loadMods();
}));

function renderImport(r) {
  const box = $("#import-result");
  const nexus = r.not_installed.filter((m) => m.source === "nexus" && m.nexus_mod_id);
  const other = r.not_installed.filter((m) => m.source !== "nexus" && m.source !== "manual" && m.source_ref
    && sourceInfos.some((s) => s.id === m.source));
  const byHand = r.not_installed.filter((m) => !nexus.includes(m) && !other.includes(m));
  const names = (list) => list.map((m) => m.version ? `${m.name} ${m.version}` : m.name).join(", ");
  const summary = [`Added ${r.categories_added} categor${r.categories_added === 1 ? "y" : "ies"}`,
    `sorted ${r.assigned} of your mods into them`].join(" and ");
  box.classList.remove("hidden");
  box.replaceChildren(el("div", { class: "card notice" },
    el("div", { class: "row" }, el("b", {}, "Mod list imported"), el("span", { class: "spacer" }),
      el("button", { class: "link inline", onclick: () => box.classList.add("hidden") }, "Close")),
    el("p", {}, `${summary}.`),
    r.not_installed.length ? null : el("p", { class: "muted" }, "You already have every mod on the list."),
    nexus.length ? el("div", { class: "row" },
      el("span", {}, `${nexus.length} from Nexus aren't installed: ${names(nexus)}`),
      el("button", { class: "primary", onclick: (e) => busy(e.target, async () => enqueue(nexus.map((m) => ({
        kind: "nexus", name: m.name, modId: m.nexus_mod_id, fileId: m.nexus_file_id ?? undefined,
      })))) }, `Get them (${nexus.length})`)) : null,
    other.length ? el("div", { class: "row" },
      el("span", {}, `${other.length} from GitHub or other sources aren't installed: ${names(other)}`),
      el("button", { onclick: (e) => busy(e.target, async () => enqueue(other.map((m) => ({
        kind: "source", name: m.name, source: m.source, ref: m.source_ref,
      })))) }, `Get them (${other.length})`)) : null,
    byHand.length ? el("p", { class: "muted" }, `These were installed by hand, so get them yourself: ${names(byHand)}`) : null,
    ...r.collections.map((c) => el("div", { class: "row" },
      el("span", {}, `The list follows the collection ${c.name || c.slug}${c.revision ? ` (revision ${c.revision})` : ""}.`),
      el("button", { onclick: (e) => busy(e.target, () => showCollection(c.slug, c.revision ?? null)) }, "Open it")))));
}

// ---- collections you follow ---------------------------------------------------
async function loadModpacks() {
  if (!currentGame) return;
  await Promise.all([renderTracked(), renderCategories(), collBrowse || !nexusUser ? null : runCollections({ text: "", sort: "endorsements", offset: 0 })]);
  if (!nexusUser) $("#coll-title").textContent = "Connect your Nexus account in Get mods to browse collections.";
}

function showModpacksMain() {
  $("#coll-detail").classList.add("hidden");
  $("#modpacks-main").classList.remove("hidden");
}

async function renderTracked(list = null) {
  if (!currentGame) return;
  list ??= await invoke("tracked_collections", { gameId: currentGame.id });
  $("#tracked-empty").classList.toggle("hidden", list.length > 0);
  $("#collections-check").classList.toggle("hidden", !list.length);
  $("#tracked-list").replaceChildren(...list.map(trackedRow));
}

function plural(n, word) {
  return `${n} ${word}${n === 1 ? "" : "s"}`;
}

function countsLine(c) {
  return [`${c.installed} installed`,
    c.other_file && `${c.other_file} on another version`,
    c.disabled && `${c.disabled} turned off`,
    c.missing && `${c.missing} missing${c.missing_required !== c.missing ? ` (${c.missing_required} required)` : ""}`]
    .filter(Boolean).join(" · ");
}

function trackedRow(t) {
  return el("div", { class: "file" },
    el("div", {},
      el("b", {}, t.name), " ",
      t.update_available ? el("span", { class: "badge ok" }, `revision ${t.latest_revision} is out`) : null, " ",
      t.counts.missing_required ? el("span", { class: "badge bad" }, `${t.counts.missing_required} required missing`) : null,
      el("div", { class: "muted small" }, [t.author && `by ${t.author}`, t.revision && `you have revision ${t.revision}`,
        t.game_version && `made for game ${t.game_version}`, plural(t.mods.length, "mod")].filter(Boolean).join(" · ")),
      el("div", { class: "muted small" }, countsLine(t.counts))),
    el("div", { class: "actions" },
      el("button", { class: t.update_available ? "update" : "", onclick: (e) => busy(e.target, () => showCollection(t.slug)) },
        t.update_available ? "See what changed" : "Open"), " ",
      el("button", { class: "danger", title: "Its mods stay installed", onclick: (e) => busy(e.target, async () => {
        await invoke("untrack_collection", { gameId: currentGame.id, slug: t.slug });
        await renderTracked();
      }) }, "Stop following")));
}

$("#collections-check").addEventListener("click", (e) => busy(e.target, async () => {
  if (!currentGame) throw "Select a game first";
  const r = await invoke("check_collection_updates", { gameId: currentGame.id });
  await renderTracked(r.collections);
  const n = r.collections.filter((c) => c.update_available).length;
  toast([n ? `${plural(n, "collection")} ${n === 1 ? "has" : "have"} a new revision` : "Every collection you follow is up to date",
    ...r.errors.slice(0, 5)].join("\n"), r.errors.length > 0 && !n);
}));

// ---- finding collections ---------------------------------------------------------
const COLL_PAGE = 20;
const COLL_SORT_TITLES = { endorsements: "Most endorsed", downloads: "Most downloaded", rating: "Best rated", updated: "Recently updated", created: "Newest" };
let collBrowse = null;
let collReq = 0;

async function runCollections(q) {
  if (!nexusUser) throw "Connect your Nexus account in Get mods to browse collections";
  collBrowse = q;
  const req = ++collReq;
  $("#coll-title").textContent = "Loading…";
  const [page, followed] = await Promise.all([
    invoke("nexus_collections", { query: { text: q.text, sort: q.sort, offset: q.offset, count: COLL_PAGE } }),
    currentGame ? invoke("tracked_collections", { gameId: currentGame.id }) : [],
  ]).catch((e) => {
    if (req === collReq) $("#coll-title").textContent = "";
    throw e;
  });
  if (req !== collReq) return;
  const mine = new Set(followed.map((c) => c.slug));
  let title = q.text ? `Collections matching “${q.text}”` : COLL_SORT_TITLES[q.sort];
  if (page.total !== null && page.total !== undefined) title += ` · ${page.total.toLocaleString()} collections`;
  $("#coll-title").textContent = title;
  $("#coll-grid").replaceChildren(...page.collections.map((c) => collectionCard(c, mine.has(c.slug))));
  if (!page.collections.length) $("#coll-grid").append(el("p", { class: "muted" }, "No collections found."));
  const total = page.total ?? 0;
  $("#coll-pager").classList.toggle("hidden", total <= COLL_PAGE);
  $("#coll-page").textContent = `Page ${Math.floor(page.offset / COLL_PAGE) + 1} of ${Math.max(1, Math.ceil(total / COLL_PAGE))}`;
  $("#coll-prev").disabled = page.offset <= 0;
  $("#coll-next").disabled = page.offset + COLL_PAGE >= total;
}

function collectionCard(c, followed) {
  return el("button", { class: "mod-card", title: c.name, onclick: () => busy(null, () => showCollection(c.slug)) },
    nexusImage(c.image, "thumb"),
    el("div", { class: "mod-card-body" },
      el("div", { class: "mod-card-title" }, c.name),
      el("div", { class: "muted small" }, [`by ${c.author || "unknown"}`, c.mod_count != null && plural(c.mod_count, "mod"),
        c.total_size && fmtSize(c.total_size)].filter(Boolean).join(" · ")),
      el("div", { class: "summary" }, c.summary || ""),
      el("div", { class: "stats" },
        el("span", { title: "Endorsements" }, `♥ ${fmtCount(c.endorsements)}`),
        el("span", { title: "Downloads" }, `⬇ ${fmtCount(c.downloads)}`),
        el("span", { title: "Last updated" }, fmtDate(c.updated)),
        c.category ? el("span", { class: "badge" }, c.category) : null,
        c.game_version ? el("span", { class: "badge", title: "Game version it was made for" }, `game ${c.game_version}`) : null,
        followed ? el("span", { class: "badge ok" }, "following") : null,
        c.adult ? el("span", { class: "badge bad" }, "adult") : null)));
}

function collSearchFromInputs() {
  return { text: $("#coll-search").value.trim(), sort: $("#coll-sort").value, offset: 0 };
}
$("#coll-search-go").addEventListener("click", (e) => busy(e.target, () => runCollections(collSearchFromInputs())));
$("#coll-search").addEventListener("keydown", (e) => {
  if (e.key === "Enter") busy(null, () => runCollections(collSearchFromInputs()));
});
$("#coll-sort").addEventListener("change", () => busy(null, () => runCollections(collSearchFromInputs())));
$("#coll-prev").addEventListener("click", (e) => busy(e.target, () => runCollections({ ...collBrowse, offset: Math.max(0, collBrowse.offset - COLL_PAGE) })));
$("#coll-next").addEventListener("click", (e) => busy(e.target, () => runCollections({ ...collBrowse, offset: collBrowse.offset + COLL_PAGE })));

// ---- one collection ----------------------------------------------------------------
// Its mods with where each stands here; installing from it follows it, so
// the manager can say when it changes. Free accounts click once per mod in
// the Nexus window, as with any queued Nexus mod; Premium downloads straight away.
async function showCollection(slug, revision = null) {
  if (!currentGame) throw "Select a game first";
  if (!nexusUser) { showTab("nexus"); throw "Connect your Nexus account to open collections"; }
  showTab("modpacks");
  const v = await invoke("collection_view", { gameId: currentGame.id, slug, revision });
  refreshQuota().catch(() => {});
  renderCollection(v);
}

const STATE_BADGE = {
  installed: () => el("span", { class: "badge ok" }, "installed"),
  disabled: () => el("span", { class: "badge bad" }, "turned off"),
  other_file: (m) => el("span", { class: "badge", title: "You have a different file of this mod than the collection lists" },
    m.installed_version ? `you have v${m.installed_version}` : "you have another version"),
  missing: () => el("span", { class: "badge bad" }, "missing"),
};

function renderCollection(v) {
  const c = v.collection;
  const gameId = currentGame.id;
  const seed = (m) => ({ kind: "nexus", name: m.mod_name, modId: m.mod_id, fileId: m.file_id });
  const replaceSeed = (m) => ({ ...seed(m), replaces: m.installed_id });
  const missing = v.mods.filter((m) => m.state === "missing");
  const missingRequired = missing.filter((m) => !m.optional);
  const other = v.mods.filter((m) => m.state === "other_file");
  const disabled = v.mods.filter((m) => m.state === "disabled");
  const required = v.mods.filter((m) => !m.optional);
  const optional = v.mods.filter((m) => m.optional);
  const following = v.tracked_revision !== null && v.tracked_revision !== undefined;
  const reopen = () => showCollection(c.slug, c.revision);
  // Queue mods and follow the collection at this revision.
  const install = async (seeds) => {
    await invoke("track_collection", { gameId, slug: c.slug, revision: c.revision });
    if (seeds.length) enqueue(seeds);
    await reopen();
  };
  const action = (label, list, onclick, cls = "") => el("button", {
    class: cls, disabled: list.length ? null : "", onclick: (e) => busy(e.target, onclick),
  }, `${label} (${list.length})`);

  const row = (m) => el("div", { class: "file" },
    el("div", {},
      el("b", {}, m.mod_name), " ",
      m.optional ? el("span", { class: "badge" }, "optional") : null, " ",
      STATE_BADGE[m.state]?.(m),
      el("div", { class: "muted mono" }, [m.file_name, m.version && `v${m.version}`].filter(Boolean).join(" · "))),
    el("div", { class: "actions" },
      el("button", { onclick: (e) => busy(e.target, async () => {
        await selectSource("nexus", false);
        showTab("nexus");
        await showMod(m.mod_id, m.file_id);
      }) }, "Open")));

  const d = v.diff;
  const update = following && d ? el("div", { class: "card notice" },
    el("p", {}, el("b", {}, `You follow revision ${v.tracked_revision}. `),
      `Revision ${c.revision} ${c.revision > v.tracked_revision ? "adds" : "has"} ${plural(d.added.length, "mod")}, `
      + `changes the version of ${d.changed.length} and drops ${d.removed.length}.`),
    c.changelog ? el("p", { class: "muted" }, `Author's notes: ${c.changelog}`) : null,
    d.added.length ? el("p", { class: "small" }, "New: ", d.added.map((m) => m.mod_name).join(", ")) : null,
    d.changed.length ? el("p", { class: "small" }, "New version: ",
      d.changed.map(([a, b]) => `${b.mod_name} ${a.version || "?"} → ${b.version || "?"}`).join(", ")) : null,
    d.removed.length ? el("div", { class: "small" }, "No longer in it (still installed if you had them; uninstall them yourself if you don't want them): ",
      ...d.removed.map((m) => {
        const have = mods.find((x) => x.nexus_mod_id === m.mod_id);
        return el("span", { class: "removed" }, m.mod_name,
          have ? el("button", { class: "link inline danger", onclick: (e) => busy(e.target, async () => { await uninstall(have); await reopen(); }) }, "Uninstall") : null);
      })) : null,
    el("div", { class: "row" },
      action(`Update to revision ${c.revision}`, [...missingRequired, ...other],
        () => install([...missingRequired.map(seed), ...other.map(replaceSeed)]), "primary"),
      missingRequired.length + other.length ? null : el("button", { onclick: (e) => busy(e.target, () => install([])) },
        `Mark revision ${c.revision} as followed`))) : null;

  $("#modpacks-main").classList.add("hidden");
  $("#coll-detail").classList.remove("hidden");
  $("#coll-detail").replaceChildren(el("div", { class: "card mod-detail" },
    el("div", { class: "row" },
      el("button", { onclick: () => { showModpacksMain(); busy(null, renderTracked); } }, "← Back to Modpacks"),
      el("span", { class: "spacer" }),
      following
        ? el("button", { class: "danger", title: "Its mods stay installed", onclick: (e) => busy(e.target, async () => {
          await invoke("untrack_collection", { gameId, slug: c.slug });
          await reopen();
        }) }, "Stop following")
        : el("button", { title: "Get told when it changes, without installing anything now", onclick: (e) => busy(e.target, () => install([])) }, "Follow"),
      el("button", { onclick: (e) => busy(e.target, () => invoke("nexus_open_collection", { slug: c.slug })) }, "Open on nexusmods.com")),
    el("div", { class: "mod-head" },
      c.image ? nexusImage(c.image, "hero") : null,
      el("div", {},
        el("h2", {}, c.name),
        el("p", { class: "muted" }, [c.author && `by ${c.author}`, c.revision && `revision ${c.revision}`,
          c.game_version && `made for game ${c.game_version}`, plural(c.mods.length, "mod")].filter(Boolean).join(" · ")),
        el("p", { class: "muted" }, `Here: ${countsLine(v.counts)}`),
        c.summary ? el("p", {}, c.summary) : null)),
    update,
    el("div", { class: "row" },
      action("Install required mods", missingRequired, () => install(missingRequired.map(seed)), "primary"),
      optional.length ? action("Install all, with optional", missing, () => install(missing.map(seed))) : null,
      other.length ? el("button", {
        title: "Replaces the version you have with the file the collection lists",
        onclick: (e) => busy(e.target, () => install(other.map(replaceSeed))),
      }, `Use the collection's versions (${other.length})`) : null,
      disabled.length ? el("button", { onclick: (e) => busy(e.target, async () => {
        for (const m of disabled) {
          const target = mods.find((x) => x.id === m.installed_id);
          if (target) await setEnabled(target, true);
        }
        await loadMods();
        await reopen();
      }) }, `Turn on the ${plural(disabled.length, "mod")} you turned off`) : null),
    nexusUser.is_premium ? null : el("p", { class: "muted" },
      "Free account: the Nexus window opens each mod's file in turn. Click “Slow download” once per mod and the queue does the rest."),
    c.external.length ? el("div", { class: "card notice" }, "Not on Nexus, get these by hand: ", c.external.join(", ")) : null,
    el("p", { class: "muted" }, "Collections can also change load order and settings; only the mods themselves are installed here."),
    el("h3", {}, `Required (${required.length})`), ...required.map(row),
    optional.length ? el("details", {}, el("summary", {}, `Optional (${optional.length})`), ...optional.map(row)) : null));
  $("main").scrollTop = 0;
}
