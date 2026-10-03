// Force-directed graph of mods, frameworks and the game parts they touch.
// Plain canvas, no libraries. Node labels come from mod files, so they are
// only ever drawn as canvas text or set via textContent.
(() => {
  const COLORS = {
    mod: "#fcee0a", framework: "#00f0ff", class: "#b48cff", record: "#3ddc84",
    records: "#6b6b85", shared_resources: "#ff4d5e", base_game: "#e8e8f0",
  };
  const NAMES = {
    mod: "Mod", framework: "Framework", class: "Game class", record: "Tweak record / patched resource",
    records: "Tweak records (one mod)", shared_resources: "Resources several mods replace", base_game: "Base game",
  };
  const HARMLESS = new Set(["wraps", "observes"]);
  const canvas = document.getElementById("graph");
  const ctx = canvas.getContext("2d");
  const info = document.getElementById("graph-info");
  let nodes = [], edges = [], byId = new Map();
  let view = { x: 0, y: 0, k: 1 };
  let hover = null, selected = null, drag = null, alpha = 0, raf = 0;
  let lastReport = null;

  document.getElementById("graph-legend").replaceChildren(...Object.entries(NAMES).map(([t, n]) => {
    const s = document.createElement("span");
    s.style.setProperty("--c", COLORS[t]);
    s.textContent = n;
    return s;
  }));

  function radius(n) {
    if (n.type === "mod") return 9 + Math.min(10, Math.sqrt(n.degree) * 2);
    if (n.type === "framework" || n.type === "base_game") return 11;
    return 5 + Math.min(8, Math.sqrt(n.degree) * 1.5);
  }

  function build(report) {
    const hide = document.getElementById("graph-hide-hooks").checked;
    const old = new Map(nodes.map((n) => [n.id, n]));
    const keep = report.graph.edges.filter((e) => !(hide && HARMLESS.has(e.label) && !e.conflict));
    const used = new Set(keep.flatMap((e) => [e.from, e.to]));
    nodes = report.graph.nodes
      .filter((n) => n.type === "mod" || used.has(n.id))
      .map((n, i) => {
        const prev = old.get(n.id);
        const a = (i / Math.max(1, report.graph.nodes.length)) * Math.PI * 2;
        return { ...n, degree: 0, x: prev?.x ?? Math.cos(a) * 200 + Math.random() * 20, y: prev?.y ?? Math.sin(a) * 200 + Math.random() * 20, vx: 0, vy: 0, fixed: prev?.fixed ?? false };
      });
    byId = new Map(nodes.map((n) => [n.id, n]));
    edges = keep.filter((e) => byId.has(e.from) && byId.has(e.to)).map((e) => ({ ...e, a: byId.get(e.from), b: byId.get(e.to) }));
    edges.forEach((e) => { e.a.degree++; e.b.degree++; });
    selected = selected && byId.get(selected.id) || null;
    alpha = 1;
    tick();
  }

  function step() {
    const n = nodes.length;
    // Repulsion (O(n²) is fine for a few hundred nodes).
    for (let i = 0; i < n; i++) {
      const p = nodes[i];
      for (let j = i + 1; j < n; j++) {
        const q = nodes[j];
        let dx = q.x - p.x, dy = q.y - p.y;
        let d2 = dx * dx + dy * dy || 0.01;
        if (d2 > 250000) continue;
        const f = (1800 / d2) * alpha;
        const d = Math.sqrt(d2);
        dx /= d; dy /= d;
        p.vx -= dx * f; p.vy -= dy * f; q.vx += dx * f; q.vy += dy * f;
      }
    }
    // Springs.
    for (const e of edges) {
      const dx = e.b.x - e.a.x, dy = e.b.y - e.a.y;
      const d = Math.sqrt(dx * dx + dy * dy) || 0.01;
      const target = e.label === "requires" ? 140 : 90;
      const f = ((d - target) / d) * 0.04 * alpha;
      e.a.vx += dx * f; e.a.vy += dy * f; e.b.vx -= dx * f; e.b.vy -= dy * f;
    }
    for (const p of nodes) {
      p.vx -= p.x * 0.004 * alpha; p.vy -= p.y * 0.004 * alpha; // gravity
      if (p.fixed || p === drag?.node) { p.vx = p.vy = 0; continue; }
      p.vx *= 0.82; p.vy *= 0.82;
      p.x += p.vx; p.y += p.vy;
    }
    alpha *= 0.985;
  }

  function resize() {
    const r = canvas.getBoundingClientRect();
    const dpr = window.devicePixelRatio || 1;
    canvas.width = Math.max(1, r.width * dpr);
    canvas.height = Math.max(1, r.height * dpr);
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    draw();
  }

  function toScreen(p) {
    const r = canvas.getBoundingClientRect();
    return { x: r.width / 2 + (p.x + view.x) * view.k, y: r.height / 2 + (p.y + view.y) * view.k };
  }
  function toWorld(sx, sy) {
    const r = canvas.getBoundingClientRect();
    return { x: (sx - r.width / 2) / view.k - view.x, y: (sy - r.height / 2) / view.k - view.y };
  }

  function neighbors(n) {
    const s = new Set([n]);
    edges.forEach((e) => { if (e.a === n) s.add(e.b); if (e.b === n) s.add(e.a); });
    return s;
  }

  function draw() {
    const r = canvas.getBoundingClientRect();
    ctx.clearRect(0, 0, r.width, r.height);
    const focus = hover || selected;
    const near = focus ? neighbors(focus) : null;
    for (const e of edges) {
      const a = toScreen(e.a), b = toScreen(e.b);
      const lit = !near || (near.has(e.a) && near.has(e.b) && (e.a === focus || e.b === focus));
      ctx.strokeStyle = e.conflict ? "rgba(255,77,94," + (lit ? 0.9 : 0.15) + ")" : "rgba(140,140,170," + (lit ? 0.55 : 0.08) + ")";
      ctx.lineWidth = Math.min(4, 1 + Math.log2(e.weight || 1) * 0.5) * (e.conflict ? 1.4 : 1);
      ctx.setLineDash(e.label === "requires" ? [4, 4] : []);
      ctx.beginPath(); ctx.moveTo(a.x, a.y); ctx.lineTo(b.x, b.y); ctx.stroke();
      if (focus && lit && view.k > 0.5) {
        ctx.setLineDash([]);
        ctx.fillStyle = "rgba(232,232,240,.75)";
        ctx.font = "11px system-ui, sans-serif";
        ctx.fillText(e.label, (a.x + b.x) / 2 + 4, (a.y + b.y) / 2 - 4);
      }
    }
    ctx.setLineDash([]);
    for (const n of nodes) {
      const p = toScreen(n);
      const rad = radius(n) * Math.max(0.6, Math.min(1.6, view.k));
      const dim = near && !near.has(n);
      ctx.globalAlpha = dim ? 0.2 : 1;
      ctx.fillStyle = COLORS[n.type] || "#888";
      ctx.beginPath(); ctx.arc(p.x, p.y, rad, 0, Math.PI * 2); ctx.fill();
      if (n.missing) { ctx.strokeStyle = "#ff4d5e"; ctx.lineWidth = 3; ctx.stroke(); }
      if (n === selected) { ctx.strokeStyle = "#fff"; ctx.lineWidth = 2; ctx.stroke(); }
      const showLabel = n.type === "mod" || n.type === "framework" || n.type === "base_game" || n === focus || (near && near.has(n)) || view.k > 1.3;
      if (showLabel) {
        ctx.fillStyle = n.type === "mod" ? "#fff" : "#c8c8d8";
        ctx.font = (n.type === "mod" ? "600 12px" : "11px") + " system-ui, sans-serif";
        ctx.fillText(n.label + (n.missing ? " (missing)" : ""), p.x + rad + 4, p.y + 4);
      }
      ctx.globalAlpha = 1;
    }
  }

  function tick() {
    cancelAnimationFrame(raf);
    const loop = () => {
      if (alpha > 0.01 || drag?.node) {
        for (let i = 0; i < 2; i++) step();
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
      const d = Math.hypot(p.x - sx, p.y - sy);
      if (d < radius(n) * Math.max(0.6, Math.min(1.6, view.k)) + 4 && d < bd) { best = n; bd = d; }
    }
    return best;
  }

  function showInfo(n) {
    if (!n) {
      info.replaceChildren("Drag to pan, scroll to zoom, drag a node to move it, click a node for details.");
      info.className = "muted";
      return;
    }
    info.className = "";
    const h = document.createElement("h3");
    h.textContent = n.label + (n.missing ? " (not installed)" : "");
    const t = document.createElement("div");
    t.className = "muted";
    t.textContent = NAMES[n.type] || n.type;
    const ul = document.createElement("ul");
    for (const e of edges.filter((e) => e.a === n || e.b === n)) {
      const other = e.a === n ? e.b : e.a;
      const li = document.createElement("li");
      if (e.conflict) li.className = "conflict";
      const subject = e.a === n ? `${e.label} ${other.label}` : `${other.label} ${e.label} this`;
      li.textContent = subject + (e.detail.length ? `: ${e.detail.slice(0, 12).join(", ")}${e.detail.length > 12 ? "…" : ""}` : "");
      ul.append(li);
    }
    info.replaceChildren(h, t, ul);
  }

  canvas.addEventListener("mousedown", (ev) => {
    const r = canvas.getBoundingClientRect();
    const n = nodeAt(ev.clientX - r.left, ev.clientY - r.top);
    drag = { node: n, sx: ev.clientX, sy: ev.clientY, vx: view.x, vy: view.y, moved: false };
    canvas.classList.add("dragging");
    if (n) { alpha = Math.max(alpha, 0.3); tick(); }
  });
  window.addEventListener("mousemove", (ev) => {
    const r = canvas.getBoundingClientRect();
    if (drag) {
      const dx = ev.clientX - drag.sx, dy = ev.clientY - drag.sy;
      if (Math.abs(dx) + Math.abs(dy) > 3) drag.moved = true;
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
    if (n !== hover) { hover = n; draw(); }
  });
  window.addEventListener("mouseup", () => {
    if (!drag) return;
    if (!drag.moved) { selected = drag.node; showInfo(selected); draw(); }
    drag = null;
    canvas.classList.remove("dragging");
  });
  canvas.addEventListener("mouseleave", () => { hover = null; draw(); });
  canvas.addEventListener("wheel", (ev) => {
    ev.preventDefault();
    const r = canvas.getBoundingClientRect();
    const before = toWorld(ev.clientX - r.left, ev.clientY - r.top);
    view.k = Math.min(4, Math.max(0.2, view.k * (ev.deltaY < 0 ? 1.12 : 1 / 1.12)));
    const after = toWorld(ev.clientX - r.left, ev.clientY - r.top);
    view.x += after.x - before.x; view.y += after.y - before.y;
    draw();
  }, { passive: false });
  new ResizeObserver(resize).observe(canvas);

  document.getElementById("graph-hide-hooks").addEventListener("change", () => lastReport && build(lastReport));
  document.getElementById("graph-reset").addEventListener("click", () => {
    nodes.forEach((n) => { n.fixed = false; n.x = (Math.random() - 0.5) * 400; n.y = (Math.random() - 0.5) * 400; });
    view = { x: 0, y: 0, k: 1 };
    alpha = 1; tick();
  });

  window.showGraph = (report) => {
    lastReport = report;
    build(report);
    showInfo(selected);
    resize();
  };
})();
