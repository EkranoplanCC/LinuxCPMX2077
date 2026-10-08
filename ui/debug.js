// Debug terminal: streams Nexus API requests (from the request log) and what
// the app does on this machine (from the activity log), oldest at the top.
// Everything is shown as text only; nothing from the logs is parsed as HTML.
const { invoke } = window.__TAURI__.core;
const dialog = window.__TAURI__.dialog;

const $ = (sel) => document.querySelector(sel);
const MAX_LINES = 5000;

// Which filter group each kind belongs to. Errors always show.
const GROUP = {
  nexus: "api", api: "api",
  download: "download", verify: "download",
  extract: "extract",
  copy: "files", backup: "files", restore: "files", move: "files", delete: "files",
  setup: "setup", info: "setup",
  error: null,
};
const SERVED = { network: "", cache: "(cached)", saved: "(saved copy)", held: "(held back to protect the quota)" };

let lines = []; // { at, kind, failed, text, msg, path, extra }
let lastApi = 0;
let lastOps = 0;
let paused = false;

function time(ms) {
  const d = new Date(ms);
  const p = (n, w = 2) => String(n).padStart(w, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}.${p(d.getMilliseconds(), 3)}`;
}

function fromRequest(r) {
  const failed = !!r.error || (r.status && r.status >= 400);
  const what = r.label ? `  ${r.label}` : "";
  const extra = [
    r.status ? String(r.status) : null,
    r.served === "network" ? `${r.duration_ms} ms` : SERVED[r.served] || r.served,
    r.bytes ? `${r.bytes} B` : null,
    r.hourly_remaining != null || r.daily_remaining != null
      ? `left: hourly ${r.hourly_remaining ?? "?"}, daily ${r.daily_remaining ?? "?"}` : null,
    r.error,
  ].filter(Boolean).join(" · ");
  return { at: r.at_ms, kind: "nexus", failed, msg: `${r.method} ${r.endpoint}${what}`, path: null, extra };
}

function fromEntry(e) {
  return { at: e.at_ms, kind: e.kind, failed: e.kind === "error", msg: e.message, path: e.path, extra: null };
}

function tagOf(kind) {
  return kind === "nexus" ? "NEXUS" : kind.toUpperCase();
}

function plain(l) {
  return [time(l.at), tagOf(l.kind).padEnd(8), l.msg, l.path || "", l.extra ? `[${l.extra}]` : ""]
    .filter((s) => s !== "").join("  ");
}

function visible(l) {
  const group = GROUP[l.kind];
  if (group && !document.querySelector(`input[data-group="${group}"]`).checked) return false;
  const q = $("#filter").value.trim().toLowerCase();
  return !q || plain(l).toLowerCase().includes(q);
}

function row(l) {
  const div = document.createElement("div");
  div.className = `line k-${l.kind}${l.failed ? " failed" : ""}`;
  const t = document.createElement("span");
  t.className = "t";
  t.textContent = time(l.at);
  const tag = document.createElement("span");
  tag.className = "tag";
  tag.textContent = tagOf(l.kind);
  const body = document.createElement("span");
  const msg = document.createElement("span");
  msg.className = "msg";
  msg.textContent = l.msg;
  body.append(msg);
  if (l.path) {
    const p = document.createElement("span");
    p.className = "path";
    p.textContent = ` ${l.path}`;
    body.append(p);
  }
  if (l.extra) {
    const x = document.createElement("span");
    x.className = "extra";
    x.textContent = `  [${l.extra}]`;
    body.append(x);
  }
  div.append(t, tag, body);
  return div;
}

function render() {
  const out = $("#out");
  const shown = lines.filter(visible);
  if (!shown.length) {
    const empty = document.createElement("div");
    empty.className = "empty";
    empty.textContent = lines.length
      ? "Nothing matches the filters."
      : "Waiting for activity… Browse Get Mods, download or install a mod, and every step shows up here.";
    out.replaceChildren(empty);
  } else {
    out.replaceChildren(...shown.map(row));
  }
  status(shown.length);
  if ($("#follow").checked) out.scrollTop = out.scrollHeight;
}

function append(fresh) {
  const out = $("#out");
  out.querySelector(".empty")?.remove();
  lines = lines.concat(fresh);
  if (lines.length > MAX_LINES) {
    lines = lines.slice(-MAX_LINES);
    return render();
  }
  const shown = fresh.filter(visible);
  out.append(...shown.map(row));
  status(out.querySelectorAll(".line").length);
  if ($("#follow").checked) out.scrollTop = out.scrollHeight;
}

function status(shown) {
  const errors = lines.filter((l) => l.failed).length;
  $("#status").textContent =
    `${lines.length} lines${shown !== lines.length ? `, ${shown} shown` : ""}` +
    `${errors ? ` · ${errors} errors` : ""}${paused ? " · paused" : " · live"} · your API key is never shown`;
}

async function poll() {
  if (paused) return;
  const [reqs, ops] = await Promise.all([
    invoke("nexus_requests", { after: lastApi }),
    invoke("activity_log", { after: lastOps }),
  ]);
  if (reqs.length) lastApi = reqs[reqs.length - 1].id;
  if (ops.length) lastOps = ops[ops.length - 1].id;
  const fresh = reqs.map(fromRequest).concat(ops.map(fromEntry)).sort((a, b) => a.at - b.at);
  if (fresh.length) append(fresh);
}

async function tick() {
  try {
    if (!document.hidden) await poll();
  } catch (e) {
    $("#status").textContent = `Couldn't read the logs: ${e}`;
  }
  setTimeout(tick, 1000);
}

for (const box of document.querySelectorAll("input[data-group]")) box.addEventListener("change", render);
$("#filter").addEventListener("input", render);
$("#follow").addEventListener("change", () => { if ($("#follow").checked) render(); });
$("#pause").addEventListener("click", () => {
  paused = !paused;
  $("#pause").textContent = paused ? "Resume" : "Pause";
  $("#pause").classList.toggle("on", paused);
  $("#state").textContent = paused ? "PAUSED" : "ONLINE";
  status($("#out").querySelectorAll(".line").length);
});
$("#clear").addEventListener("click", async () => {
  await invoke("activity_clear");
  lines = [];
  render();
});
$("#copy").addEventListener("click", async () => {
  try {
    await navigator.clipboard.writeText(lines.filter(visible).map(plain).join("\n"));
    $("#copy").textContent = "Copied";
  } catch (e) {
    $("#status").textContent = `Couldn't copy: ${e}`;
  }
  setTimeout(() => { $("#copy").textContent = "Copy"; }, 1500);
});
$("#save").addEventListener("click", async () => {
  const path = await dialog.save({ title: "Save debug log", defaultPath: "cpmx2077-debug.log",
    filters: [{ name: "Log", extensions: ["log", "txt"] }] });
  if (!path) return;
  try {
    await invoke("save_debug_log", { path, text: lines.filter(visible).map(plain).join("\n") + "\n" });
    $("#status").textContent = `Saved to ${path}`;
  } catch (e) {
    $("#status").textContent = `Couldn't save: ${e}`;
  }
});

render();
tick();
