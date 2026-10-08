// File map in the Netrunner tab: a registry-editor style browser of every
// file mods installed. Folders on the left, the selected folder's contents on
// the right with the mod each file came from and where it sits in the game.
// The tree comes from the install tracking (`mod_file_tree`), not a disk scan.
(() => {
  const STATE_LABEL = { active: "In use", overridden: "Overridden", missing: "Missing", disabled: "Mod off" };
  const ROW_LIMIT = 500;
  let tree = null;
  let group = loadPref("netrunner.group", "location");
  let selected = "";
  const expanded = { location: new Set([""]), mod: new Set([""]) };
  let byKey = new Map();
  let modNames = new Map();
  let gameId = null;

  // Keys are unique per node: the folder path, prefixed by the mod in the
  // "by mod" view where the same folder appears under several mods.
  function index(node, parent, prefix) {
    const base = node.mod_id != null ? `m${node.mod_id}` : prefix;
    node.key = node.mod_id != null ? base : (base ? `${base}:${node.path}` : node.path);
    node.parent = parent;
    byKey.set(node.key, node);
    for (const c of node.folders) index(c, node, base);
  }

  async function load() {
    if (!currentGame) return;
    if (gameId !== currentGame.id) {
      gameId = currentGame.id;
      selected = "";
    }
    try {
      tree = await invoke("mod_file_tree", { gameId: currentGame.id, group });
    } catch (e) {
      tree = null;
      $("#net-tree").replaceChildren(el("p", { class: "muted" }, `Couldn't read the file list: ${e}`));
      $("#net-rows").replaceChildren();
      return;
    }
    byKey = new Map();
    modNames = new Map(tree.mods.map((m) => [m.id, m.name]));
    tree.root.name = group === "mod" ? "All mods" : "Game folder";
    index(tree.root, null, "");
    if (!byKey.has(selected)) selected = "";
    render();
  }
  window.loadFileTree = load;

  function render() {
    renderTree();
    renderList();
  }

  // ---- left pane: folders ---------------------------------------------
  function renderTree() {
    const box = $("#net-tree");
    if (!tree.root.file_count) {
      box.replaceChildren(el("p", { class: "muted" }, "No mod files installed yet."));
      return;
    }
    box.replaceChildren(treeItem(tree.root, 0));
  }

  function treeItem(node, depth) {
    const open = expanded[group].has(node.key);
    const hasKids = node.folders.length > 0;
    const caret = el("span", { class: "net-caret", "aria-hidden": "true" }, hasKids ? (open ? "▾" : "▸") : "");
    caret.addEventListener("click", (e) => { e.stopPropagation(); toggle(node); });
    const label = el("button", {
      class: `net-node${node.key === selected ? " selected" : ""}${node.mod_id != null ? " mod" : ""}`,
      role: "treeitem", "aria-expanded": hasKids ? String(open) : null, "data-key": node.key,
      style: `padding-left:${6 + depth * 16}px`, title: node.path || node.name,
    }, caret,
      el("span", { class: "net-icon", "aria-hidden": "true" }, node.mod_id != null ? "◆" : open && hasKids ? "▤" : "▣"),
      el("span", { class: "net-name" }, node.name),
      el("span", { class: "net-count" }, node.file_count),
      node.problems ? el("span", { class: "net-warn", title: `${node.problems} missing or overridden` }, "!") : null);
    label.addEventListener("click", () => select(node));
    label.addEventListener("dblclick", () => toggle(node));
    label.addEventListener("keydown", (e) => keyNav(e, node));
    const wrap = el("div", { class: "net-branch" }, label);
    if (open) for (const c of node.folders) wrap.append(treeItem(c, depth + 1));
    return wrap;
  }

  function toggle(node, force) {
    const set = expanded[group];
    const open = force ?? !set.has(node.key);
    if (open) set.add(node.key); else set.delete(node.key);
    renderTree();
    focusKey(node.key);
  }

  function select(node, { open = true } = {}) {
    selected = node.key;
    $("#net-filter").value = "";
    // Selecting reveals the node: every folder above it opens, and a click
    // opens the folder itself too (arrow keys only move).
    if (open) expanded[group].add(node.key);
    for (let p = node.parent; p; p = p.parent) expanded[group].add(p.key);
    render();
    focusKey(node.key);
  }

  function focusKey(key) {
    const b = [...document.querySelectorAll("#net-tree .net-node")].find((n) => n.dataset.key === key);
    b?.focus({ preventScroll: false });
  }

  // Arrow keys move like a registry editor: up/down between visible rows,
  // right opens or steps in, left closes or steps out.
  function keyNav(e, node) {
    const rows = [...document.querySelectorAll("#net-tree .net-node")];
    const i = rows.findIndex((r) => r.dataset.key === node.key);
    const go = (r) => { if (r) { e.preventDefault(); select(byKey.get(r.dataset.key), { open: false }); } };
    if (e.key === "ArrowDown") go(rows[i + 1]);
    else if (e.key === "ArrowUp") go(rows[i - 1]);
    else if (e.key === "ArrowRight" && node.folders.length) {
      e.preventDefault();
      if (!expanded[group].has(node.key)) toggle(node, true); else select(node.folders[0], { open: false });
    } else if (e.key === "ArrowLeft") {
      e.preventDefault();
      if (expanded[group].has(node.key) && node.folders.length) toggle(node, false);
      else if (node.parent) select(node.parent, { open: false });
    }
  }

  // ---- right pane: contents -------------------------------------------
  function renderList() {
    const node = byKey.get(selected) || tree.root;
    const query = $("#net-filter").value.trim().toLowerCase();
    setAddress(node, query);
    const rows = [];
    let total;
    if (query) {
      const hits = [];
      collect(tree.root, (f) => f.path.toLowerCase().includes(query) || (modNames.get(f.mod_id) || "").toLowerCase().includes(query), hits);
      total = hits.length;
      for (const f of hits.slice(0, ROW_LIMIT)) rows.push(fileRow(f));
    } else {
      for (const c of node.folders) rows.push(folderRow(c));
      for (const f of node.files.slice(0, ROW_LIMIT)) rows.push(fileRow(f));
      total = node.folders.length + node.files.length;
    }
    $("#net-rows").replaceChildren(...rows);
    const empty = $("#net-empty");
    if (!total) empty.textContent = query ? "Nothing matches." : "This folder is empty.";
    else if (total > rows.length) empty.textContent = `Showing ${rows.length} of ${total}. Open a subfolder or search to narrow it down.`;
    empty.classList.toggle("hidden", !(total === 0 || total > rows.length));
  }

  function collect(node, test, out) {
    for (const f of node.files) if (test(f)) out.push(f);
    for (const c of node.folders) collect(c, test, out);
  }

  function setAddress(node, query) {
    const crumbs = [];
    for (let n = node; n; n = n.parent) crumbs.unshift(n);
    const path = $("#net-path");
    if (query) {
      path.replaceChildren(`Search: “${query}”`);
    } else {
      path.replaceChildren(...crumbs.flatMap((n, i) => {
        const name = i === 0 && group === "location" ? tree.game_path : n.name;
        const b = el("button", { class: "link crumb", onclick: () => select(n) }, name);
        return i ? [el("span", { class: "sep" }, " › "), b] : [b];
      }));
    }
    $("#net-open").onclick = () => busy($("#net-open"), () => openFolder(query ? "" : node.path));
  }

  function openFolder(path) {
    return invoke("open_game_folder", { gameId: currentGame.id, path });
  }

  function folderRow(node) {
    const tr = el("tr", { class: "net-folder" },
      el("td", {}, el("button", { class: "link", onclick: () => select(node) }, `${node.mod_id != null ? "◆" : "▣"} ${node.name}`)),
      el("td", { class: "mono muted" }, node.mod_id != null ? "" : `${gameName()}/${node.path}/`),
      el("td", { class: "muted" }, node.mods.length === 1 ? modNames.get(node.mods[0]) : `${node.mods.length} mods`),
      el("td", {}, node.problems ? el("span", { class: "net-state missing" }, `${node.problems} need attention`) : el("span", { class: "muted" }, node.file_count === 1 ? "1 file" : `${node.file_count} files`)),
      el("td", {}, fmtSize(node.size)));
    tr.addEventListener("dblclick", () => select(node));
    return tr;
  }

  // The game folder's own name stands in for its full path in each row; the
  // address bar and tooltips carry the full path.
  function gameName() {
    return tree.game_path.replace(/\/+$/, "").split("/").pop() || tree.game_path;
  }

  function fileRow(f) {
    const folder = f.path.includes("/") ? f.path.slice(0, f.path.lastIndexOf("/")) : "";
    const state = f.state === "overridden" && f.overridden_by != null
      ? `Overridden by ${modNames.get(f.overridden_by) || "another mod"}`
      : STATE_LABEL[f.state];
    const modName = modNames.get(f.mod_id) || `Mod ${f.mod_id}`;
    const modCell = group === "location"
      ? el("button", { class: "link", title: "Show this mod's files", onclick: () => showMod(f.mod_id) }, modName)
      : modName;
    return el("tr", { class: `net-file ${f.state}` },
      el("td", { class: "mono", title: f.path }, f.name),
      el("td", { class: "mono" }, el("button", { class: "link path", title: `${tree.game_path}/${f.path}\nClick to open this folder`, onclick: (e) => busy(e.target, () => openFolder(folder)) },
        `${gameName()}/${folder ? `${folder}/` : ""}`)),
      el("td", {}, modCell),
      el("td", {}, el("span", { class: `net-state ${f.state}` }, state)),
      el("td", {}, fmtSize(f.size)));
  }

  async function showMod(modId) {
    setGroup("mod");
    await load();
    const n = byKey.get(`m${modId}`);
    if (n) { expanded.mod.add(n.key); select(n); }
  }

  function setGroup(g) {
    group = g;
    savePref("netrunner.group", g);
    selected = "";
    document.querySelectorAll("#net-group button").forEach((b) => b.classList.toggle("active", b.dataset.group === g));
  }

  document.querySelectorAll("#net-group button").forEach((b) => {
    b.classList.toggle("active", b.dataset.group === group);
    b.addEventListener("click", () => { if (b.dataset.group !== group) { setGroup(b.dataset.group); busy(null, load); } });
  });
  let filterTimer;
  $("#net-filter").addEventListener("input", () => {
    clearTimeout(filterTimer);
    filterTimer = setTimeout(() => { if (tree) renderList(); }, 150);
  });
})();
