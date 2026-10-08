// Graph of mods, frameworks and the game parts they touch, drawn either as a
// force-directed web or as a left-to-right flowchart. Plain canvas, no
// libraries. Node labels come from mod files, so they are only ever drawn as
// canvas text or set via textContent.
(() => {
  const COLORS = {
    mod: "#f6cf3c", framework: "#4fe3d0", class: "#b48cff", record: "#3ddc84",
    records: "#6b6b85", shared_resources: "#ff4d5e", base_game: "#e8e8f0",
  };
  const NAMES = {
    mod: "Mod", framework: "Framework", class: "Game class", record: "Tweak record / patched resource",
    records: "Tweak records (one mod)", shared_resources: "Resources several mods replace", base_game: "Base game",
  };
  const HARMLESS = new Set(["wraps", "observes"]);
  // Flowchart columns, left to right: what mods need, the mods, what they
  // change. The last column is grouped by kind in this order.
  const COLUMN = { framework: 0, mod: 1 };
  const GROUP = { base_game: 0, shared_resources: 1, class: 2, record: 3, records: 4 };
  const BADGE = { error: "#ff4d5e", warning: "#f6cf3c" };
  // "Group by": every node by its type, or mods by one of their properties.
  // Groups get a ring (web) or a heading (flowchart); a mod with several
  // tags sits between its tags' rings and under its first tag's heading.
  const GROUP_BY = { type: "Node type", modtype: "Mod type", tag: "Tag", category: "Nexus category", source: "Source" };
  const PALETTE = ["#4fe3d0", "#f6cf3c", "#b48cff", "#3ddc84", "#ff9f43", "#4da3ff", "#ff6bcb", "#e8e8f0", "#9be15d", "#ff4d5e"];
  // What a mod's files do, from the analysis counts, in this order.
  const MOD_KINDS = [
    ["Archive", ["resource", "xl_patch"]],
    ["Redscript", ["reds_replace_method", "reds_wrap_method", "reds_add_method", "reds_add_field", "reds_replace_global"]],
    ["TweakXL", ["tweak_record", "tweak_property"]],
    ["CET", ["cet_override", "cet_observe"]],
    ["RED4ext plugin", ["red4ext_plugin"]],
  ];
  // Frameworks whose need alone says what a mod is (CET Lua, TweakXL YAML).
  const NEEDS_KIND = { cet: "CET", tweakxl: "TweakXL", redmod: "REDmod" };
  const MIN_K = 0.05, MAX_K = 4;
  const COLOR_RE = /^#[0-9a-f]{6}$/i;

  const canvas = document.getElementById("graph");
  const ctx = canvas.getContext("2d");
  const info = document.getElementById("graph-info");
  const message = document.getElementById("graph-message");
  const zoomLabel = document.getElementById("graph-zoom");
  const layoutPick = document.getElementById("graph-layout");
  const groupPick = document.getElementById("graph-group");
  let nodes = [], edges = [], byId = new Map();
  let view = { x: 0, y: 0, k: 1 };
  // Once the user pans or zooms, the view stays where they put it.
  let userMoved = false, needsFit = false;
  let layout = loadPref("graphLayout", "force") === "flow" ? "flow" : "force";
  let groupBy = GROUP_BY[loadPref("graphGroup", "")] ? loadPref("graphGroup", "") : "";
  // Group key -> { label, color, order, count, x, y }; flowchart headings.
  let groups = new Map(), headers = [];
  let hover = null, selected = null, drag = null, alpha = 0, raf = 0;
  let lastReport = null;
  // Mod id -> { level, findings, logErrors }: what the checks found per mod.
  let problems = new Map();
  // Mods lit up from a finding or crash suspect: { modIds, label, mods, near }.
  let lit = null;

  layoutPick.value = layout;
  groupPick.value = groupBy;
  const legend = Object.entries(NAMES).map(([t, n]) => {
    const s = document.createElement("span");
    s.style.setProperty("--c", COLORS[t]);
    s.textContent = n;
    return s;
  });
  for (const [level, text] of [["error", "Has a problem"], ["warning", "Overlap to check"]]) {
    const s = document.createElement("span");
    s.className = "flag";
    s.style.setProperty("--c", BADGE[level]);
    s.textContent = text;
    legend.push(s);
  }
  document.getElementById("graph-legend").replaceChildren(...legend);

  function setMessage(text, isError = false) {
    message.textContent = text || "";
    message.classList.toggle("hidden", !text);
    message.classList.toggle("error", isError);
  }
  setMessage("Select a game to see how its mods connect.");

  // ---- sizes ----------------------------------------------------------------
  function radius(n) {
    if (n.type === "mod") return 9 + Math.min(10, Math.sqrt(n.degree) * 2);
    if (n.type === "framework" || n.type === "base_game") return 11;
    return 5 + Math.min(8, Math.sqrt(n.degree) * 1.5);
  }
  const big = (n) => n.type === "mod" || n.type === "framework" || n.type === "base_game";
  const font = (n, px) => (big(n) ? `600 ${px}px` : `${px}px`) + " system-ui, sans-serif";
  const fullLabel = (n) => n.label + (n.missing ? " (missing)" : "");

  // Label width at 12px and the flowchart box, measured once per build.
  function measure(n) {
    ctx.font = font(n, 12);
    let text = fullLabel(n);
    const max = 240;
    if (ctx.measureText(text).width > max) {
      while (text.length > 4 && ctx.measureText(text + "…").width > max) text = text.slice(0, -1);
      text += "…";
    }
    n.text = text;
    n.tw = ctx.measureText(text).width;
    n.w = n.tw + 20;
    n.h = big(n) ? 26 : 22;
  }

  // ---- building -----------------------------------------------------------
  function build(report, keepPositions) {
    const hide = document.getElementById("graph-hide-hooks").checked;
    const old = keepPositions ? new Map(nodes.map((n) => [n.id, n])) : new Map();
    const keep = report.graph.edges.filter((e) => !(hide && HARMLESS.has(e.label) && !e.conflict));
    const used = new Set(keep.flatMap((e) => [e.from, e.to]));
    nodes = report.graph.nodes
      .filter((n) => n.type === "mod" || used.has(n.id))
      .map((n) => ({ ...n, degree: 0, x: 0, y: 0, vx: 0, vy: 0, fixed: false, problem: problems.get(n.mod_id) || null }));
    byId = new Map(nodes.map((n) => [n.id, n]));
    edges = keep.filter((e) => byId.has(e.from) && byId.has(e.to)).map((e) => ({ ...e, affected: e.affected || [], a: byId.get(e.from), b: byId.get(e.to) }));
    edges.forEach((e) => { e.a.degree++; e.b.degree++; });
    nodes.forEach(measure);
    assignGroups(report);
    selected = selected && byId.get(selected.id) || null;
    hover = null;
    if (lit) lit = lightUp(lit.modIds, lit.label);

    let reused = 0;
    for (const n of nodes) {
      const prev = old.get(n.id);
      if (prev) { n.x = prev.x; n.y = prev.y; n.fixed = prev.fixed; reused++; }
    }
    if (layout === "flow") {
      flowLayout();
      alpha = 0;
    } else {
      if (reused < nodes.length) seed(nodes.filter((n) => !old.has(n.id)), reused > 0);
      // A re-check of the same mods only nudges the layout.
      alpha = reused === nodes.length ? 0.05 : 1;
      settle(reused === nodes.length ? 0 : 150);
    }
    if (!userMoved) fit();
    setMessage(nodes.length ? "" : report.mods.length
      ? "Your mods don't touch anything the graph can show."
      : "No enabled mods to show. Install or enable a mod, then press Check again.");
    tick();
  }

  // Mods on an inner spiral, everything else on an outer one, in the order
  // the analysis lists them, so the same library always starts (and so
  // settles) the same way.
  function seed(list, near) {
    const mods = list.filter((n) => n.type === "mod"), rest = list.filter((n) => n.type !== "mod");
    [...mods, ...rest].forEach((n, i) => {
      const r = 40 * Math.sqrt(i + 1), a = i * 2.39996;
      n.x = Math.cos(a) * r; n.y = Math.sin(a) * r;
      const t = groupTarget(n);
      if (t) { n.x = t.x + Math.cos(a) * 30; n.y = t.y + Math.sin(a) * 30; }
      // New nodes in an existing layout start next to what they connect to.
      if (near) {
        const e = edges.find((e) => (e.a === n && old(e.b)) || (e.b === n && old(e.a)));
        if (e) { const o = e.a === n ? e.b : e.a; n.x = o.x + Math.cos(a) * 60; n.y = o.y + Math.sin(a) * 60; }
      }
    });
    function old(n) { return !list.includes(n); }
  }

  // ---- groups ---------------------------------------------------------------
  function modType(summary) {
    if (!summary) return "Not scanned";
    const parts = MOD_KINDS.filter(([, kinds]) => kinds.some((k) => summary.counts[k] > 0)).map(([label]) => label);
    for (const need of summary.requires || []) {
      const label = NEEDS_KIND[need];
      if (label && !parts.includes(label)) parts.push(label);
    }
    return parts.length ? parts.join(" + ") : "Other files";
  }

  // [key, label, color?] for each group a node is in under `groupBy`.
  function groupsOf(n, report, modsById, summaries) {
    if (groupBy === "type") return [[n.type, NAMES[n.type] || n.type, COLORS[n.type]]];
    if (n.type !== "mod" || n.mod_id == null) return [];
    const m = modsById.get(n.mod_id);
    switch (groupBy) {
      case "modtype": { const t = modType(summaries.get(n.mod_id)); return [[t, t]]; }
      case "tag": {
        const names = modTags.mods[n.mod_id] || [];
        if (!names.length) return [["~none", "No tags"]];
        return names.map((name) => [`tag:${name}`, name, modTags.tags.find((t) => t.name === name)?.color]);
      }
      case "category": return m?.category ? [[m.category, m.category]] : [["~none", "No Nexus category"]];
      case "source": {
        if (!m) return [];
        const label = m.source === "nexus" ? "Nexus" : sourceInfos.find((s) => s.id === m.source)?.label || "Installed by hand";
        return [[label, label]];
      }
    }
    return [];
  }

  function assignGroups(report) {
    groups = new Map();
    const modsById = new Map(mods.map((m) => [m.id, m]));
    const summaries = new Map((report?.mods || []).map((s) => [s.mod_id, s]));
    for (const n of nodes) {
      n.groups = [];
      if (!groupBy) continue;
      for (const [key, label, color] of groupsOf(n, report, modsById, summaries)) {
        if (!groups.has(key)) groups.set(key, { label, color: COLOR_RE.test(color || "") ? color : null, count: 0 });
        groups.get(key).count++;
        n.groups.push(key);
      }
    }
    // Order: node types as in the legend, tags as in your list, the rest by
    // name; "no tag"/"no category" last.
    const typeOrder = Object.keys(NAMES), tagOrder = modTags.tags.map((t) => `tag:${t.name}`);
    const rank = (k) => groupBy === "type" ? typeOrder.indexOf(k) : groupBy === "tag" ? tagOrder.indexOf(k) : -1;
    const keys = [...groups.keys()].sort((a, b) => (a === "~none") - (b === "~none") || rank(a) - rank(b)
      || groups.get(a).label.localeCompare(groups.get(b).label));
    keys.forEach((k, i) => {
      const g = groups.get(k);
      g.order = i;
      g.color ||= PALETTE[i % PALETTE.length];
    });
    // Ring centres on a circle big enough for the groups and their nodes.
    const r = keys.length < 2 ? 0 : Math.max(200, 65 * Math.sqrt(nodes.length), keys.length * 110 / (2 * Math.PI));
    keys.forEach((k, i) => {
      const a = (i / keys.length) * Math.PI * 2 - Math.PI / 2;
      Object.assign(groups.get(k), { x: Math.cos(a) * r, y: Math.sin(a) * r });
    });
  }

  // Where a grouped node is pulled to: the middle of its groups' centres.
  function groupTarget(n) {
    if (!n.groups?.length) return null;
    let x = 0, y = 0;
    for (const k of n.groups) { x += groups.get(k).x; y += groups.get(k).y; }
    return { x: x / n.groups.length, y: y / n.groups.length };
  }

  const groupRank = (n) => n.groups?.length ? groups.get(n.groups[0]).order : 1e9;

  function regroup() {
    if (!lastReport) return;
    assignGroups(lastReport);
    if (layout === "flow") flowLayout();
    else { alpha = Math.max(alpha, 0.6); }
    if (!userMoved) fit();
    showInfo(selected);
    tick();
  }

  // Run the simulation without drawing, so the first frame already shows a
  // readable layout.
  function settle(budgetMs) {
    const end = performance.now() + budgetMs;
    while (alpha > 0.02 && performance.now() < end) step();
  }

  function step() {
    const n = nodes.length;
    // Repulsion (O(n²) is fine for a few hundred nodes).
    for (let i = 0; i < n; i++) {
      const p = nodes[i];
      for (let j = i + 1; j < n; j++) {
        const q = nodes[j];
        let dx = q.x - p.x, dy = q.y - p.y;
        let d2 = dx * dx + dy * dy;
        if (d2 > 640000) continue;
        if (d2 < 1) { dx = (i % 7) - 3 + 0.5; dy = (j % 5) - 2 + 0.5; d2 = dx * dx + dy * dy; }
        const d = Math.sqrt(d2);
        // Floor the distance so overlapping nodes push apart instead of
        // flying off.
        const f = (5200 / Math.max(d2, 400)) * alpha;
        dx /= d; dy /= d;
        p.vx -= dx * f; p.vy -= dy * f; q.vx += dx * f; q.vy += dy * f;
      }
    }
    // Springs.
    for (const e of edges) {
      const dx = e.b.x - e.a.x, dy = e.b.y - e.a.y;
      const d = Math.sqrt(dx * dx + dy * dy) || 0.01;
      const target = e.label === "requires" ? 190 : 130;
      const f = ((d - target) / d) * 0.04 * alpha;
      e.a.vx += dx * f; e.a.vy += dy * f; e.b.vx -= dx * f; e.b.vy -= dy * f;
    }
    for (const p of nodes) {
      p.vx -= p.x * 0.0025 * alpha; p.vy -= p.y * 0.0025 * alpha; // gravity
      const t = groupTarget(p);
      if (t) { p.vx += (t.x - p.x) * 0.02 * alpha; p.vy += (t.y - p.y) * 0.02 * alpha; }
      if (p.fixed || p === drag?.node) { p.vx = p.vy = 0; continue; }
      p.vx *= 0.82; p.vy *= 0.82;
      const v = Math.hypot(p.vx, p.vy);
      if (v > 30) { p.vx *= 30 / v; p.vy *= 30 / v; }
      p.x += p.vx; p.y += p.vy;
    }
    alpha *= 0.985;
  }

  // Columns left to right, each ordered to keep lines from crossing
  // (a few barycenter sweeps), boxes left-aligned in their column and every
  // column starting at the top.
  function flowLayout() {
    const cols = [[], [], []];
    for (const n of nodes) cols[COLUMN[n.type] ?? 2].push(n);
    const byName = (a, b) => a.label.localeCompare(b.label);
    const byGroup = (a, b) => groupRank(a) - groupRank(b);
    cols[0].sort((a, b) => byGroup(a, b) || byName(a, b)); cols[1].sort((a, b) => byGroup(a, b) || byName(a, b));
    cols[2].sort((a, b) => byGroup(a, b) || GROUP[a.type] - GROUP[b.type] || byName(a, b));
    const adj = new Map(nodes.map((n) => [n, []]));
    edges.forEach((e) => { adj.get(e.a).push(e.b); adj.get(e.b).push(e.a); });
    const pos = new Map();
    const index = () => cols.forEach((c) => c.forEach((n, i) => pos.set(n, (i + 0.5) / c.length)));
    const bary = (n) => {
      const ns = adj.get(n);
      return ns.length ? ns.reduce((a, m) => a + pos.get(m), 0) / ns.length : pos.get(n);
    };
    const sortBy = (col, group) => {
      const b = new Map(col.map((n) => [n, bary(n)]));
      col.sort((x, y) => byGroup(x, y) || (group ? GROUP[x.type] - GROUP[y.type] : 0) || b.get(x) - b.get(y));
    };
    index();
    for (let pass = 0; pass < 6; pass++) {
      sortBy(cols[1]); index();
      sortBy(cols[2], true); sortBy(cols[0]); index();
    }
    let x = 0;
    headers = [];
    const key = (n) => n.groups?.[0] ?? null;
    for (const col of cols) {
      if (!col.length) continue;
      let y = 0;
      col.forEach((n, i) => {
        const k = key(n), prev = i ? col[i - 1] : null;
        if (k !== null && (!prev || key(prev) !== k)) {
          // A heading over each group.
          if (prev) y += 18;
          const g = groups.get(k);
          headers.push({ label: `${g.label} (${g.count})`, color: g.color, x, y: y + 12 });
          y += 22;
        } else if (prev && prev.type !== n.type) y += 18; // gap between kinds
        n.x = x + n.w / 2; n.y = y + n.h / 2;
        y += n.h + 8;
      });
      col.forEach((n) => { n.vx = n.vy = 0; });
      x += Math.max(...col.map((n) => n.w)) + 150;
    }
  }

  // ---- view -----------------------------------------------------------------
  function rect() { return canvas.getBoundingClientRect(); }

  function toScreen(p) {
    const r = rect();
    return { x: r.width / 2 + (p.x + view.x) * view.k, y: r.height / 2 + (p.y + view.y) * view.k };
  }
  function toWorld(sx, sy) {
    const r = rect();
    return { x: (sx - r.width / 2) / view.k - view.x, y: (sy - r.height / 2) / view.k - view.y };
  }

  // World-space box a node takes up, label included.
  function bounds(n) {
    if (layout === "flow") return [n.x - n.w / 2, n.y - n.h / 2, n.x + n.w / 2, n.y + n.h / 2];
    const r = radius(n);
    return [n.x - r, n.y - r, n.x + r + 6 + (big(n) ? n.tw : 0), n.y + r];
  }

  // Zoom and centre so `list` (default: everything) fills the free part of
  // the canvas, left of the details panel.
  function fit(list = nodes) {
    const r = rect();
    if (!r.width || !r.height) { needsFit = true; return; }
    needsFit = false;
    if (!list.length) { view = { x: 0, y: 0, k: 1 }; updateZoom(); return; }
    let x0 = Infinity, y0 = Infinity, x1 = -Infinity, y1 = -Infinity;
    for (const n of list) {
      const [a, b, c, d] = bounds(n);
      x0 = Math.min(x0, a); y0 = Math.min(y0, b); x1 = Math.max(x1, c); y1 = Math.max(y1, d);
    }
    const panel = r.width > 760 ? info.offsetWidth + 20 : 0;
    const pad = 40, w = r.width - panel - pad * 2, h = r.height - pad * 2 - 30;
    let k = Math.min(w / Math.max(1, x1 - x0), h / Math.max(1, y1 - y0), 1.5);
    let top = false;
    // A tall flowchart fits its width and starts at the top; scroll to see more.
    if (layout === "flow" && k < 0.45) { k = Math.min(w / Math.max(1, x1 - x0), 1); top = true; }
    view.k = Math.min(MAX_K, Math.max(MIN_K, k));
    view.x = (-panel / 2) / view.k - (x0 + x1) / 2;
    view.y = top ? (pad - r.height / 2) / view.k - y0 : -(y0 + y1) / 2;
    updateZoom();
  }

  function zoomAt(factor, sx, sy) {
    const before = toWorld(sx, sy);
    view.k = Math.min(MAX_K, Math.max(MIN_K, view.k * factor));
    const after = toWorld(sx, sy);
    view.x += after.x - before.x; view.y += after.y - before.y;
    userMoved = true;
    updateZoom();
    draw();
  }
  function zoomCenter(factor) {
    const r = rect();
    zoomAt(factor, r.width / 2, r.height / 2);
  }
  function updateZoom() {
    zoomLabel.textContent = `${Math.round(view.k * 100)}%`;
  }

  function resize() {
    const r = rect();
    const dpr = window.devicePixelRatio || 1;
    canvas.width = Math.max(1, r.width * dpr);
    canvas.height = Math.max(1, r.height * dpr);
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    if (r.width && (needsFit || !userMoved)) fit();
    draw();
  }

  function neighbors(n) {
    const s = new Set([n]);
    edges.forEach((e) => { if (e.a === n) s.add(e.b); if (e.b === n) s.add(e.a); });
    return s;
  }

  // The lit mods, plus what they share: everything two of them touch, or
  // everything a single lit mod touches.
  function lightUp(modIds, label) {
    const mods = new Set(modIds.map((id) => byId.get(`mod:${id}`)).filter(Boolean));
    const near = new Set(mods);
    const count = new Map();
    edges.forEach((e) => {
      if (mods.has(e.a)) count.set(e.b, (count.get(e.b) || 0) + 1);
      if (mods.has(e.b)) count.set(e.a, (count.get(e.a) || 0) + 1);
    });
    count.forEach((c, n) => { if (c >= 2 || mods.size === 1) near.add(n); });
    return { modIds, label, mods, near };
  }

  // ---- drawing --------------------------------------------------------------
  function edgeEnds(e) {
    if (layout !== "flow") return [toScreen(e.a), toScreen(e.b)];
    const [l, r] = e.a.x <= e.b.x ? [e.a, e.b] : [e.b, e.a];
    return [toScreen({ x: l.x + l.w / 2, y: l.y }), toScreen({ x: r.x - r.w / 2, y: r.y })];
  }

  function roundRect(x, y, w, h, r) {
    ctx.beginPath();
    ctx.moveTo(x + r, y);
    ctx.arcTo(x + w, y, x + w, y + h, r);
    ctx.arcTo(x + w, y + h, x, y + h, r);
    ctx.arcTo(x, y + h, x, y, r);
    ctx.arcTo(x, y, x + w, y, r);
    ctx.closePath();
  }

  function badge(n, x, y, size) {
    if (!n.problem) return;
    ctx.globalAlpha = 1;
    ctx.fillStyle = BADGE[n.problem.level];
    ctx.strokeStyle = "#0a0a10";
    ctx.lineWidth = 2;
    ctx.beginPath(); ctx.arc(x, y, size, 0, Math.PI * 2); ctx.fill(); ctx.stroke();
    if (size >= 5) {
      ctx.fillStyle = "#0a0a10";
      ctx.font = `700 ${Math.round(size * 1.6)}px system-ui, sans-serif`;
      ctx.textAlign = "center"; ctx.textBaseline = "middle";
      ctx.fillText("!", x, y + 0.5);
      ctx.textAlign = "start"; ctx.textBaseline = "alphabetic";
    }
  }

  function draw() {
    const r = rect();
    ctx.clearRect(0, 0, r.width, r.height);
    const focus = hover || selected;
    const near = focus ? neighbors(focus) : lit?.mods.size ? lit.near : null;
    const onFocus = (e) => focus ? e.a === focus || e.b === focus : lit.mods.has(e.a) || lit.mods.has(e.b);
    const flow = layout === "flow";
    if (groupBy && !flow) drawRings();
    if (groupBy && flow && view.k * 12 >= 5) {
      ctx.font = `600 ${12 * view.k}px system-ui, sans-serif`;
      for (const h of headers) {
        const p = toScreen(h);
        ctx.fillStyle = h.color;
        ctx.fillText(h.label, p.x, p.y);
      }
    }
    for (const e of edges) {
      const [a, b] = edgeEnds(e);
      if (Math.max(a.x, b.x) < 0 || Math.min(a.x, b.x) > r.width || Math.max(a.y, b.y) < 0 || Math.min(a.y, b.y) > r.height) continue;
      const bright = !near || (near.has(e.a) && near.has(e.b) && onFocus(e));
      ctx.strokeStyle = e.conflict ? "rgba(255,77,94," + (bright ? 0.9 : 0.15) + ")" : "rgba(140,140,170," + (bright ? 0.55 : 0.08) + ")";
      ctx.lineWidth = Math.min(4, 1 + Math.log2(e.weight || 1) * 0.5) * (e.conflict ? 1.4 : 1);
      ctx.setLineDash(e.label === "requires" ? [4, 4] : []);
      ctx.beginPath(); ctx.moveTo(a.x, a.y);
      if (flow) { const mx = (a.x + b.x) / 2; ctx.bezierCurveTo(mx, a.y, mx, b.y, b.x, b.y); } else ctx.lineTo(b.x, b.y);
      ctx.stroke();
      if (near && bright && view.k > 0.5) {
        ctx.setLineDash([]);
        ctx.fillStyle = "rgba(232,232,240,.75)";
        ctx.font = "11px system-ui, sans-serif";
        ctx.fillText(e.label, (a.x + b.x) / 2 + 4, (a.y + b.y) / 2 - 4);
      }
    }
    ctx.setLineDash([]);
    for (const n of nodes) {
      const p = toScreen(n);
      const dim = near && !near.has(n);
      const ring = n === selected || (!focus && lit?.mods.has(n));
      if (flow) {
        const w = n.w * view.k, h = n.h * view.k;
        const x = p.x - w / 2, y = p.y - h / 2;
        if (x > r.width || x + w < 0 || y > r.height || y + h < 0) continue;
        ctx.globalAlpha = dim ? 0.2 : 1;
        const solid = n.type === "mod" || n.type === "framework";
        roundRect(x, y, w, h, Math.min(6, 6 * view.k));
        ctx.fillStyle = solid ? COLORS[n.type] : "#151520";
        ctx.fill();
        ctx.lineWidth = n.missing ? 3 : ring ? 2.5 : 1.5;
        ctx.strokeStyle = n.missing ? "#ff4d5e" : ring ? "#fff" : solid ? "rgba(0,0,0,.4)" : COLORS[n.type] || "#888";
        ctx.stroke();
        if (view.k * 12 >= 5) {
          ctx.fillStyle = solid ? "#0a0a10" : "#e8e8f0";
          ctx.font = font(n, 12 * view.k);
          ctx.fillText(n.text, x + 10 * view.k, p.y + 4 * view.k);
        }
        badge(n, x + w, y, Math.max(3, Math.min(8, 7 * view.k)));
      } else {
        const rad = radius(n) * Math.max(0.6, Math.min(1.6, view.k));
        if (p.x + rad + (n.tw || 0) + 10 < 0 || p.x - rad > r.width || p.y + rad < 0 || p.y - rad > r.height) continue;
        ctx.globalAlpha = dim ? 0.2 : 1;
        ctx.fillStyle = COLORS[n.type] || "#888";
        ctx.beginPath(); ctx.arc(p.x, p.y, rad, 0, Math.PI * 2); ctx.fill();
        if (n.missing) { ctx.strokeStyle = "#ff4d5e"; ctx.lineWidth = 3; ctx.stroke(); }
        if (ring) { ctx.strokeStyle = "#fff"; ctx.lineWidth = 2; ctx.stroke(); }
        const showLabel = (big(n) && view.k > 0.35) || n === focus || (near && near.has(n)) || view.k > 1.3;
        if (showLabel) {
          ctx.fillStyle = n.type === "mod" ? "#fff" : "#c8c8d8";
          ctx.font = n.type === "mod" ? "600 12px system-ui, sans-serif" : "11px system-ui, sans-serif";
          ctx.fillText(n.text, p.x + rad + 4, p.y + 4);
        }
        badge(n, p.x + rad * 0.75, p.y - rad * 0.75, Math.max(3.5, rad * 0.45));
      }
      ctx.globalAlpha = 1;
    }
  }

  function rgba(hex, a) {
    const v = parseInt(hex.slice(1), 16);
    return `rgba(${v >> 16},${(v >> 8) & 255},${v & 255},${a})`;
  }

  // A ring around each group's nodes, with its name on top.
  function drawRings() {
    const members = new Map();
    for (const n of nodes) for (const k of n.groups) (members.get(k) || members.set(k, []).get(k)).push(n);
    for (const [k, list] of members) {
      const g = groups.get(k);
      const cx = list.reduce((a, n) => a + n.x, 0) / list.length, cy = list.reduce((a, n) => a + n.y, 0) / list.length;
      const rad = Math.max(...list.map((n) => Math.hypot(n.x - cx, n.y - cy) + radius(n))) + 16;
      const p = toScreen({ x: cx, y: cy }), sr = rad * view.k;
      ctx.beginPath(); ctx.arc(p.x, p.y, sr, 0, Math.PI * 2);
      ctx.fillStyle = rgba(g.color, 0.06); ctx.fill();
      ctx.strokeStyle = rgba(g.color, 0.45); ctx.lineWidth = 1.5; ctx.setLineDash([6, 4]); ctx.stroke(); ctx.setLineDash([]);
      ctx.font = "600 12px system-ui, sans-serif";
      ctx.fillStyle = g.color;
      ctx.textAlign = "center";
      ctx.fillText(`${g.label} (${g.count})`, p.x, p.y - sr - 6);
      ctx.textAlign = "start";
    }
  }

  function tick() {
    cancelAnimationFrame(raf);
    const loop = () => {
      if (layout === "force" && (alpha > 0.01 || drag?.node)) {
        step(); step();
        if (!userMoved) fit();
        draw();
        raf = requestAnimationFrame(loop);
      } else {
        draw();
      }
    };
    raf = requestAnimationFrame(loop);
  }

  function nodeAt(sx, sy) {
    let best = null, bd = Infinity;
    for (const n of nodes) {
      const p = toScreen(n);
      if (layout === "flow") {
        if (Math.abs(p.x - sx) <= n.w * view.k / 2 + 2 && Math.abs(p.y - sy) <= n.h * view.k / 2 + 2) return n;
        continue;
      }
      const d = Math.hypot(p.x - sx, p.y - sy);
      if (d < radius(n) * Math.max(0.6, Math.min(1.6, view.k)) + 4 && d < bd) { best = n; bd = d; }
    }
    return best;
  }

  // ---- details panel -------------------------------------------------------
  function heading(text, tag = "h3") {
    const h = document.createElement(tag);
    h.textContent = text;
    return h;
  }

  function showInfo(n) {
    if (!n && lit) {
      info.className = "";
      const ul = document.createElement("ul");
      for (const m of lit.mods) {
        const li = document.createElement("li");
        li.textContent = m.label;
        ul.append(li);
      }
      const note = document.createElement("p");
      note.className = lit.mods.size ? "muted" : "lit-note";
      note.textContent = lit.mods.size ? "Click a node for details, or empty space to clear." : "None of these mods are in the graph (disabled or uninstalled).";
      const clear = document.createElement("button");
      clear.textContent = "Clear";
      clear.addEventListener("click", () => { lit = null; showInfo(selected); draw(); });
      info.replaceChildren(heading(lit.label), ul, note, clear);
      return;
    }
    if (!n) {
      info.replaceChildren("Drag to pan, scroll to zoom, drag a node to move it, click a node for details.");
      info.className = "muted";
      return;
    }
    info.className = "";
    const parts = [heading(n.label + (n.missing ? " (not installed)" : ""))];
    const t = document.createElement("div");
    t.className = "muted";
    t.textContent = NAMES[n.type] || n.type;
    parts.push(t);
    const tags = n.type === "mod" ? modTags.mods[n.mod_id] || [] : [];
    if (tags.length) {
      const d = document.createElement("div");
      d.className = "muted";
      d.textContent = `Tags: ${tags.join(", ")}`;
      parts.push(d);
    }
    if (groupBy && groupBy !== "tag" && groupBy !== "type" && n.groups.length) {
      const d = document.createElement("div");
      d.className = "muted";
      d.textContent = `${GROUP_BY[groupBy]}: ${n.groups.map((k) => groups.get(k).label).join(", ")}`;
      parts.push(d);
    }
    if (n.problem) {
      const ul = document.createElement("ul");
      ul.className = "problems";
      for (const f of n.problem.findings) {
        const li = document.createElement("li");
        li.className = f.severity === "error" ? "conflict" : "warn";
        li.append(f.message);
        const more = affectedDetails(f.affected, { hashes: f.kind === "resource" });
        if (more) li.append(more);
        ul.append(li);
      }
      if (n.problem.logErrors) {
        const li = document.createElement("li");
        li.className = "conflict";
        li.textContent = `Named in ${n.problem.logErrors} log error${n.problem.logErrors === 1 ? "" : "s"}. `;
        const go = document.createElement("button");
        go.className = "link inline";
        go.textContent = "See the logs";
        go.addEventListener("click", () => document.getElementById("diag-crashes").scrollIntoView({ behavior: "smooth", block: "start" }));
        li.append(go);
        ul.append(li);
      }
      parts.push(heading("Problems", "h4"), ul);
    }
    const ul = document.createElement("ul");
    for (const e of edges.filter((e) => e.a === n || e.b === n)) {
      const other = e.a === n ? e.b : e.a;
      const li = document.createElement("li");
      if (e.conflict) li.className = "conflict";
      const subject = e.a === n ? `${e.label} ${other.label}` : `${other.label} ${e.label} this`;
      li.append(subject + (e.detail.length ? `: ${e.detail.slice(0, 12).join(", ")}${e.detail.length > 12 ? "…" : ""}` : ""));
      const more = e.affected.length
        ? affectedDetails(e.affected, { hashes: true })
        : e.detail.length > 12 ? affectedDetails([{ title: "", items: e.detail, more: 0 }], { label: "Show all" }) : null;
      if (more) li.append(more);
      ul.append(li);
    }
    if (ul.childElementCount) parts.push(heading("Connections", "h4"), ul);
    info.replaceChildren(...parts);
  }

  // ---- input ----------------------------------------------------------------
  canvas.addEventListener("mousedown", (ev) => {
    const r = rect();
    const n = nodeAt(ev.clientX - r.left, ev.clientY - r.top);
    drag = { node: n, sx: ev.clientX, sy: ev.clientY, vx: view.x, vy: view.y, moved: false };
    canvas.classList.add("dragging");
    if (n && layout === "force") { alpha = Math.max(alpha, 0.3); tick(); }
  });
  window.addEventListener("mousemove", (ev) => {
    const r = rect();
    if (drag) {
      const dx = ev.clientX - drag.sx, dy = ev.clientY - drag.sy;
      if (Math.abs(dx) + Math.abs(dy) > 3) { drag.moved = true; userMoved = true; }
      if (!drag.moved) return;
      if (drag.node) {
        const w = toWorld(ev.clientX - r.left, ev.clientY - r.top);
        drag.node.x = w.x; drag.node.y = w.y; drag.node.fixed = true;
      } else {
        view.x = drag.vx + dx / view.k; view.y = drag.vy + dy / view.k;
      }
      draw();
      return;
    }
    if (ev.target !== canvas) return;
    const n = nodeAt(ev.clientX - r.left, ev.clientY - r.top);
    if (n !== hover) { hover = n; canvas.classList.toggle("over-node", !!n); draw(); }
  });
  window.addEventListener("mouseup", () => {
    if (!drag) return;
    if (!drag.moved) {
      selected = drag.node;
      if (!selected) lit = null;
      showInfo(selected);
      draw();
    }
    drag = null;
    canvas.classList.remove("dragging");
  });
  canvas.addEventListener("mouseleave", () => { hover = null; draw(); });
  canvas.addEventListener("wheel", (ev) => {
    ev.preventDefault();
    const r = rect();
    // Pinch gestures and mouse wheels both arrive here; scale by how far.
    const factor = Math.exp(-Math.max(-60, Math.min(60, ev.deltaY)) * (ev.ctrlKey ? 0.01 : 0.0025));
    zoomAt(factor, ev.clientX - r.left, ev.clientY - r.top);
  }, { passive: false });
  new ResizeObserver(resize).observe(canvas);

  document.getElementById("graph-zoom-in").addEventListener("click", () => zoomCenter(1.25));
  document.getElementById("graph-zoom-out").addEventListener("click", () => zoomCenter(1 / 1.25));
  zoomLabel.addEventListener("click", () => zoomCenter(1 / view.k));
  document.getElementById("graph-fit").addEventListener("click", () => { userMoved = false; fit(); draw(); });
  document.getElementById("graph-hide-hooks").addEventListener("change", () => lastReport && build(lastReport, true));
  layoutPick.addEventListener("change", () => {
    layout = layoutPick.value === "flow" ? "flow" : "force";
    savePref("graphLayout", layout);
    userMoved = false;
    if (lastReport) build(lastReport, false);
  });
  groupPick.addEventListener("change", () => {
    groupBy = GROUP_BY[groupPick.value] ? groupPick.value : "";
    savePref("graphGroup", groupBy);
    regroup();
  });
  window.graphTagsChanged = () => {
    if (groupBy === "tag") regroup();
    else if (selected) showInfo(selected);
  };
  document.getElementById("graph-reset").addEventListener("click", () => {
    userMoved = false;
    if (lastReport) build(lastReport, false);
  });

  // What the checks found per mod: compatibility findings that name it
  // (missing frameworks, clashes; not harmless shared hooks) and how many
  // log errors mention it.
  function collectProblems(report, crash) {
    const out = new Map();
    const get = (id) => {
      if (!out.has(id)) out.set(id, { level: "warning", findings: [], logErrors: 0 });
      return out.get(id);
    };
    for (const f of report.findings) {
      if (f.severity === "info") continue;
      for (const id of f.mod_ids) {
        const p = get(id);
        p.findings.push(f);
        if (f.severity === "error") p.level = "error";
      }
    }
    for (const i of crash?.issues || []) {
      if (i.level !== "error") continue;
      for (const id of new Set(i.mod_ids)) { const p = get(id); p.logErrors++; p.level = "error"; }
    }
    return out;
  }

  window.highlightGraph = (modIds, label) => {
    lit = lightUp(modIds, label);
    selected = null;
    if (lit.mods.size) {
      fit([...lit.near]);
      userMoved = true;
    }
    showInfo(null);
    draw();
  };

  window.showGraph = (report, crash) => {
    const first = !lastReport;
    lastReport = report;
    lit = null;
    problems = collectProblems(report, crash);
    if (first) userMoved = false;
    build(report, !first);
    showInfo(selected);
    resize();
  };

  window.graphError = (err) => {
    setMessage(`The graph couldn't be built: ${err}`, true);
  };
  window.graphPending = () => {
    if (!lastReport) setMessage("Building the graph…");
  };
})();
