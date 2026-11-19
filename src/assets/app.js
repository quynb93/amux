// amux from a phone or another laptop: every run on every machine, grouped by space or
// state; each run's screen, changes and checks; starting runs; the machine's load.
// Text from agents is only ever set with textContent, never parsed as HTML.
"use strict";
const $ = (s) => document.querySelector(s);
const el = (tag, cls, text) => {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
};
const enc = encodeURIComponent;

let runs = [], current = null, tab = "runs", runsView = "spaces", runView = "screen";
let lastWaiting = new Set(), diffLines = [], comments = [], composing = null;

async function api(path, body) {
  const opts = body === undefined ? {} : {
    method: "POST",
    headers: { "Content-Type": "application/json", "X-Amux": "1" },
    body: JSON.stringify(body),
  };
  const r = await fetch(path, opts);
  const type = r.headers.get("content-type") || "";
  const data = type.includes("json") ? await r.json() : await r.text();
  if (!r.ok) throw new Error(typeof data === "string" ? data : r.statusText);
  return data;
}
const say = (where, msg) => { $(where).textContent = msg; };

// ---- tabs ----------------------------------------------------------------------------
function showTab(t) {
  tab = t; current = null;
  for (const b of document.querySelectorAll("#tabs button")) b.classList.toggle("on", b.dataset.tab === t);
  for (const id of ["runs", "new", "monitor"]) $(`#tab-${id}`).hidden = id !== t;
  $("#run").hidden = true; $("#top").hidden = false;
  if (t === "runs") renderList();
  if (t === "new") loadNew();
  if (t === "monitor") { loadPush(); loadMonitor(); }
}
for (const b of document.querySelectorAll("#tabs button")) b.onclick = () => showTab(b.dataset.tab);
function seg(sel, onPick) {
  for (const b of document.querySelectorAll(`${sel} button`)) {
    b.onclick = () => {
      for (const x of document.querySelectorAll(`${sel} button`)) x.classList.toggle("on", x === b);
      onPick(b.dataset.v);
    };
  }
}

// ---- the run list --------------------------------------------------------------------
const STATES = [["waiting", "Needs you"], ["working", "Working"], ["idle", "Idle"], ["ended", "Done"]];
const stateOf = (r) => r.state === "queued" || r.kind === "starting" ? "working"
  : ["waiting", "working", "idle"].includes(r.kind) ? r.kind : "ended";
const base = (p) => (p || "").replace(/\/+$/, "").split("/").pop() || p;
const spaceOf = (r) => (r.machine ? `${r.machine}: ` : "") + base(r.repo || r.cwd);

function card(r) {
  const b = el("button", "card");
  const top = el("div", "top");
  top.append(el("span", "id", r.id), el("span", r.state === "queued" ? "working" : r.kind, r.label));
  b.append(top, el("div", "sub", [r.account, r.branch].filter(Boolean).join(" · ")));
  const pr = r.pr && `PR #${r.pr.number}${r.pr.checks ? " · checks " + r.pr.checks : ""}`;
  const why = r.waiting || r.queued || pr || r.verifyLabel;
  if (why) b.append(el("div", r.waiting ? "why wait" : "why", why));
  b.onclick = () => openRun(r.id);
  return b;
}

function renderList() {
  const waiting = runs.filter((r) => r.kind === "waiting");
  say("#summary", `${runs.length} run${runs.length === 1 ? "" : "s"}` + (waiting.length ? ` · ${waiting.length} need you` : ""));
  document.title = waiting.length ? `(${waiting.length}) amux` : "amux";
  if (tab !== "runs" || current) return;
  const q = $("#filter").value.trim().toLowerCase();
  const shown = runs.filter((r) => !q || [r.id, r.account, r.branch, r.cwd, r.repo].some((v) => (v || "").toLowerCase().includes(q)));
  const list = $("#list");
  list.replaceChildren();
  const groups = new Map();
  if (runsView === "board") {
    for (const [k, name] of STATES) groups.set(name, shown.filter((r) => stateOf(r) === k));
  } else {
    // Spaces: one per repo (and machine). The ones that need you come first.
    // Within a space too, what needs you comes first.
    const order = { waiting: 0, working: 1, idle: 2, ended: 3 };
    const byNeed = [...shown].sort((a, b) => order[stateOf(a)] - order[stateOf(b)]);
    for (const r of byNeed) {
      const k = spaceOf(r);
      if (!groups.has(k)) groups.set(k, []);
      groups.get(k).push(r);
    }
    const rank = (rs) => rs.some((r) => r.kind === "waiting") ? 0 : rs.some((r) => stateOf(r) === "working") ? 1 : 2;
    const sorted = [...groups.entries()].sort((a, b) => rank(a[1]) - rank(b[1]) || a[0].localeCompare(b[0]));
    groups.clear();
    for (const [k, v] of sorted) groups.set(k, v);
  }
  for (const [name, items] of groups) {
    if (!items.length) continue;
    list.append(el("h2", null, `${name} · ${items.length}`));
    for (const r of items) list.append(card(r));
  }
  if (!shown.length) list.append(el("p", "note", runs.length ? "Nothing matches." : "No runs yet. Start one from New."));
}
$("#filter").oninput = renderList;
seg("#runs-view", (v) => { runsView = v; renderList(); });

function notifyNewWaiting() {
  const now = new Set(runs.filter((r) => r.kind === "waiting").map((r) => r.id));
  const fresh = [...now].filter((id) => !lastWaiting.has(id));
  if (fresh.length && lastWaiting.size + fresh.length > 0) {
    if (navigator.vibrate) navigator.vibrate(150);
    if (!pushActive && "Notification" in window && Notification.permission === "granted" && document.hidden) {
      for (const id of fresh) {
        const r = runs.find((x) => x.id === id);
        new Notification(`${id} needs you`, { body: r?.waiting || "" });
      }
    }
  }
  lastWaiting = now;
}

async function refresh() {
  try { runs = await api("/api/runs"); } catch (e) { say("#summary", "offline"); return; }
  notifyNewWaiting();
  renderList();
  if (current) renderRunHead();
}

// ---- one run -------------------------------------------------------------------------
const run = () => runs.find((r) => r.id === current);

function renderRunHead() {
  const r = run();
  say("#r-id", current);
  say("#r-sub", r ? [r.account, r.branch || base(r.cwd)].filter(Boolean).join(" · ") : "");
  const st = $("#r-state");
  st.className = r ? (r.state === "queued" ? "working" : r.kind) : "";
  st.textContent = r ? r.label : "";
  say("#reason", r?.waiting ? `Waiting: ${r.waiting}` : r?.queued ? `Queued: ${r.queued}` : "");
}

function openRun(id) {
  current = id;
  $("#tab-runs").hidden = true; $("#top").hidden = true; $("#run").hidden = false;
  renderRunHead();
  showRunView(runView);
  if ("Notification" in window && Notification.permission === "default") Notification.requestPermission();
}
$("#back").onclick = () => showTab("runs");

function showRunView(v) {
  runView = v;
  for (const b of document.querySelectorAll("#run-view button")) b.classList.toggle("on", b.dataset.v === v);
  for (const id of ["screen", "changes", "ship"]) $(`#v-${id}`).hidden = id !== v;
  if (v === "screen") loadScreen();
  if (v === "changes") loadChanges();
  if (v === "ship") loadShip();
}
seg("#run-view", showRunView);

async function loadScreen() {
  if (!current || runView !== "screen") return;
  try {
    const s = await api(`/api/runs/${enc(current)}/screen`);
    const pre = $("#screen");
    const atBottom = pre.scrollTop + pre.clientHeight >= pre.scrollHeight - 8;
    pre.textContent = s;
    if (atBottom) pre.scrollTop = pre.scrollHeight;
  } catch (e) { $("#screen").textContent = e.message; }
}
for (const b of document.querySelectorAll("#keys button")) {
  b.onclick = async () => {
    try { await api(`/api/runs/${enc(current)}/key`, { key: b.dataset.key }); } catch (e) { alert(e.message); }
    setTimeout(loadScreen, 300);
  };
}
$("#send").onsubmit = async (e) => {
  e.preventDefault();
  const text = $("#text").value.trim();
  if (!text) return;
  try { await api(`/api/runs/${enc(current)}/send`, { text }); $("#text").value = ""; } catch (err) { alert(err.message); }
  setTimeout(loadScreen, 400);
};

// ---- changes and review comments -----------------------------------------------------
// The same reading of a unified diff as amux's own review: each line knows its file and
// its line number on the new side (or the old side, for a removed line).
function parseDiff(text) {
  const out = [];
  let file = "", oldN = 0, newN = 0, inHunk = false;
  for (const raw of text.split("\n")) {
    if (raw.startsWith("diff --git ")) {
      inHunk = false;
      file = raw.slice(11).split(" b/").pop();
      out.push({ kind: "file", text: file, file });
      continue;
    }
    if (!inHunk && (raw.startsWith("--- ") || raw.startsWith("+++ ") || raw.startsWith("index "))) {
      if (raw.startsWith("+++ b/")) file = raw.slice(6);
      continue;
    }
    const h = raw.match(/^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/);
    if (h) { oldN = +h[1]; newN = +h[2]; inHunk = true; out.push({ kind: "hunk", text: raw, file }); continue; }
    if (inHunk && raw.startsWith("+")) { out.push({ kind: "add", text: raw, file, line: newN++ }); continue; }
    if (inHunk && raw.startsWith("-")) { out.push({ kind: "del", text: raw, file, line: oldN++, removed: true }); continue; }
    if (inHunk && (raw.startsWith(" ") || raw === "")) { out.push({ kind: "ctx", text: raw, file, line: newN++ }); oldN++; continue; }
    if (raw.trim()) out.push({ kind: "meta", text: raw, file });
  }
  return out;
}

async function loadChanges() {
  $("#diff").replaceChildren(el("p", "note", "Loading…"));
  try {
    const [text, cms] = await Promise.all([
      api(`/api/runs/${enc(current)}/diff`),
      api(`/api/runs/${enc(current)}/comments`).catch(() => []),
    ]);
    diffLines = parseDiff(text);
    comments = cms;
    composing = null;
    renderDiff();
  } catch (e) { $("#diff").replaceChildren(el("p", "note", e.message)); }
}

function renderDiff() {
  const box = $("#diff");
  box.replaceChildren();
  const pending = comments.filter((c) => !c.sentAt).length;
  say("#c-count", pending ? `${pending} comment${pending === 1 ? "" : "s"} to send` : "Tap a line to comment.");
  $("#c-send").disabled = !pending;
  if (!diffLines.some((l) => l.kind !== "meta")) { box.append(el("p", "note", "No changes yet.")); return; }
  diffLines.forEach((l, i) => {
    const row = el("div", `dl ${l.kind}`);
    row.append(el("span", "n", l.line ?? ""), el("span", "t", l.kind === "file" ? `  ${l.text}` : l.text));
    if (l.line != null) row.onclick = () => { composing = composing === i ? null : i; renderDiff(); };
    if (composing === i) row.classList.add("sel");
    box.append(row);
    comments.forEach((c, n) => {
      if (l.line != null && c.file === l.file && c.line === l.line && !!c.removed === !!l.removed) {
        box.append(el("div", c.sentAt ? "cm sent" : "cm", (c.sentAt ? "✓ " : "💬 ") + c.note));
        if (!c.sentAt && composing === i) {
          const rm = el("button", "danger", "Delete this comment");
          rm.onclick = async () => { await api(`/api/runs/${enc(current)}/comment-rm`, { n: n + 1 }).catch((e) => alert(e.message)); loadChanges(); };
          box.append(rm);
        }
      }
    });
    if (composing === i) {
      const f = el("div", "compose");
      const t = el("textarea"); t.rows = 3; t.placeholder = `Comment on ${l.file}:${l.line}…`;
      const save = el("button", "primary", "Save comment");
      save.onclick = async () => {
        const note = t.value.trim();
        if (!note) return;
        try {
          await api(`/api/runs/${enc(current)}/comment`, { file: l.file, line: l.line, removed: !!l.removed, note });
          comments = await api(`/api/runs/${enc(current)}/comments`);
          composing = null; renderDiff();
        } catch (e) { alert(e.message); }
      };
      f.append(t, save);
      box.append(f);
      setTimeout(() => t.focus(), 0);
    }
  });
}
$("#c-send").onclick = async () => {
  try { const m = await api(`/api/runs/${enc(current)}/comments-send`, {}); alert(m); } catch (e) { alert(e.message); }
  loadChanges();
};

// ---- ship: tests, push, PR, CI -------------------------------------------------------
async function loadShip() {
  say("#ship-out", "");
  $("#plan").textContent = "…"; $("#checks").textContent = "…";
  api(`/api/runs/${enc(current)}/verify`).then((t) => { $("#plan").textContent = t; }, (e) => { $("#plan").textContent = e.message; });
  api(`/api/runs/${enc(current)}/checks`).then((t) => { $("#checks").textContent = t; }, (e) => { $("#checks").textContent = e.message; });
}
for (const b of document.querySelectorAll("#v-ship [data-act]")) {
  b.onclick = async () => {
    const act = b.dataset.act;
    if ((act === "stop" || act === "rm") && !confirm(`${act === "stop" ? "Stop" : "Remove"} ${current}?`)) return;
    const [path, body] = act === "pr-draft" ? ["pr", { draft: true }] : [act, {}];
    b.disabled = true;
    say("#ship-out", "Working…");
    try {
      say("#ship-out", await api(`/api/runs/${enc(current)}/${path}`, body));
      if (act === "rm") return showTab("runs");
    } catch (e) { say("#ship-out", e.message); }
    b.disabled = false;
    refresh();
    setTimeout(loadShip, 800);
  };
}

// ---- new run -------------------------------------------------------------------------
let newLoaded = false;
async function loadNew() {
  if (newLoaded) return;
  newLoaded = true;
  const [accounts, machines, dirs] = await Promise.all([
    api("/api/accounts").catch(() => []), api("/api/machines").catch(() => []), api("/api/dirs").catch(() => []),
  ]);
  const acc = $("#n-account");
  for (const a of accounts) {
    const o = el("option", null, a.id + (a.default ? " (default)" : ""));
    o.value = a.id;
    if (a.default && !acc.value) o.selected = true;
    acc.append(o);
  }
  const m = $("#n-machine");
  m.append(Object.assign(el("option", null, "This computer"), { value: "" }));
  for (const x of machines) m.append(Object.assign(el("option", null, `${x.name} (${x.target})`), { value: x.name }));
  const dl = $("#n-dirs");
  for (const d of dirs) dl.append(Object.assign(el("option"), { value: d }));
  if (dirs[0]) $("#n-dir").value = dirs[0];
}
$("#n-worktree").onchange = () => { $("#n-branch-l").hidden = !$("#n-worktree").checked; };
$("#new").onsubmit = async (e) => {
  e.preventDefault();
  const body = {
    account: $("#n-account").value, machine: $("#n-machine").value, dir: $("#n-dir").value,
    worktree: $("#n-worktree").checked, branch: $("#n-branch").value, name: $("#n-name").value,
    prompt: $("#n-prompt").value,
  };
  say("#n-out", "Starting…");
  try {
    const out = await api("/api/spawn", body);
    say("#n-out", out);
    $("#n-prompt").value = "";
    const started = out.match(/started (\S+)/) || out.match(/queued (\S+)/);
    await refresh();
    if (started) {
      const id = body.machine ? `${body.machine}:${started[1]}` : started[1];
      showTab("runs"); runView = "screen"; openRun(id);
    }
  } catch (err) { say("#n-out", err.message); }
};

// ---- monitor -------------------------------------------------------------------------
async function loadMonitor() {
  const box = $("#monitor");
  let s;
  try { s = await api("/api/slots"); } catch (e) { box.replaceChildren(el("p", "note", e.message)); return; }
  box.replaceChildren();
  box.append(el("h2", null, "This computer"));
  const load = Math.min(1, s.load / (s.maxLoad || 1.5));
  box.append(el("div", "note", `Load ${s.load.toFixed(2)} per CPU (new work waits above ${s.maxLoad}) · ${s.cpus} CPUs`));
  const m1 = el("div", "meter"); const i1 = el("i", load >= 1 ? "hot" : ""); i1.style.width = `${Math.round(load * 100)}%`; m1.append(i1); box.append(m1);
  if (s.freeMb != null) {
    box.append(el("div", "note", `${(s.freeMb / 1024).toFixed(1)} GB memory free (waits below ${(s.minFreeMb / 1024).toFixed(1)} GB)`));
  }
  if (s.busy) box.append(el("p", "why wait", `Right now ${s.busy}: new work waits.`));
  box.append(el("h2", null, "Slots"));
  for (const c of s.classes) {
    const d = el("div", "slot");
    const top = el("div", "top");
    top.append(el("b", null, c.class), el("span", "note", `${c.holders.length}/${c.limit ?? "∞"}${c.waiting.length ? ` · ${c.waiting.length} waiting` : ""}`));
    d.append(top);
    const who = [...c.holders.map((h) => `▶ ${h.run || "pid " + h.pid}: ${h.cmd}`), ...c.waiting.map((h) => `… ${h.run || "pid " + h.pid}: ${h.cmd}`)];
    if (who.length) d.append(el("div", "who", who.join("\n")));
    box.append(d);
  }
  // Accounts that hit their usage limit, and until when.
  try {
    const us = await api("/api/usage");
    box.append(el("h2", null, "Accounts"));
    const rows = us.map((u) => {
      const q = u.quota || {};
      const pct = (w, n) => (w ? ` · ${n} ${Math.round(w.usedPercent)}%` : "");
      const used = pct(q.fiveHour, "5h") + pct(q.sevenDay, "7d");
      if (!u.limit) return `${u.account}: ok${used} · ${u.liveRuns} running`;
      const mins = Math.max(0, Math.round((u.limit.resetsAt - Date.now()) / 60000));
      return `✗ ${u.account}: limited, resets in ${Math.floor(mins / 60)}h ${mins % 60}m · ${u.liveRuns} running`;
    });
    box.append(el("div", "slot", rows.join("\n") || "No accounts."));
  } catch (e) { /* older amux on the other side */ }
  // Runs on one repo whose changes would conflict, before anyone merges.
  try {
    const reps = await api("/api/overlap");
    const rows = [];
    for (const rep of reps) {
      for (const c of rep.runs) {
        if (c.conflictsWithTarget.length) rows.push(`✗ ${c.run} conflicts with ${rep.target}: ${c.conflictsWithTarget.join(", ")}`);
      }
      for (const p of rep.pairs) {
        rows.push(p.conflicts.length ? `✗ ${p.a} ↔ ${p.b}: conflict in ${p.conflicts.join(", ")}` : `${p.a} ↔ ${p.b}: both change ${p.shared.join(", ")}, merge cleanly`);
      }
    }
    box.append(el("h2", null, "Overlaps"));
    box.append(el("div", "slot", rows.length ? rows.join("\n") : "No two runs touch the same files."));
  } catch (e) { /* no worktree runs, or not a repo */ }
  await themeSection(box);
  const machines = new Map([["This computer", runs.filter((r) => !r.machine)]]);
  for (const r of runs.filter((r) => r.machine)) {
    if (!machines.has(r.machine)) machines.set(r.machine, []);
    machines.get(r.machine).push(r);
  }
  box.append(el("h2", null, "Machines"));
  for (const [name, rs] of machines) {
    const live = rs.filter((r) => stateOf(r) !== "ended").length;
    const need = rs.filter((r) => r.kind === "waiting").length;
    box.append(el("div", "slot", `${name}: ${live} running${need ? `, ${need} need you` : ""}, ${rs.length - live} done`));
  }
}

// ---- themes --------------------------------------------------------------------------
// The same themes as `amux ui` (`amux theme`): choosing one here chooses it there too.
let themes = null;
async function themeSection(box) {
  if (!themes) { try { themes = await api("/api/themes"); } catch (e) { return; } }
  box.append(el("h2", null, "Theme"));
  const grid = el("div", "themes");
  for (const t of themes) {
    const b = el("button", t.current ? "on" : "");
    const sw = el("span", "sw");
    if (t.swatch.length) {
      sw.style.background = t.swatch[0];
      for (const c of t.swatch.slice(1)) { const d = el("i"); d.style.background = c; sw.append(d); }
    } else {
      sw.textContent = t.name === "mono" ? "Aa" : "auto";
    }
    b.append(sw, el("span", null, t.name));
    b.onclick = async () => {
      try { await api("/api/theme", { name: t.name }); } catch (e) { alert(e.message); return; }
      themes = null;
      reloadTheme();
      loadMonitor();
    };
    grid.append(b);
  }
  box.append(grid);
}
function reloadTheme() {
  const link = $("#theme-css");
  link.onload = paintChrome;
  link.href = `/theme.css?${Date.now()}`;
}
// The phone's status bar follows the theme's background.
function paintChrome() {
  const bg = getComputedStyle(document.documentElement).getPropertyValue("--bg").trim();
  if (bg) document.querySelector('meta[name="theme-color"]').setAttribute("content", bg);
}
$("#theme-css").onload = paintChrome;
paintChrome();

// ---- push notifications --------------------------------------------------------------
// Web Push (`amux phone webpush`): notifications with the app closed, straight from amux.
// It needs HTTPS and a service worker; on iPhone and iPad, also the home-screen app.
const isIOS = /iPhone|iPad|iPod/.test(navigator.userAgent) || (navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1);
const standalone = matchMedia("(display-mode: standalone)").matches || navigator.standalone === true;
let pushActive = false, swReady = null;
if ("serviceWorker" in navigator && window.isSecureContext) {
  swReady = navigator.serviceWorker.register("/sw.js");
  swReady.then((r) => r.pushManager && r.pushManager.getSubscription())
    .then((s) => s && pushInfo(s)).then((i) => { pushActive = !!(i && i.active); }, () => {});
  // A tapped notification, with the app already open: go to its run.
  navigator.serviceWorker.addEventListener("message", (e) => { if (e.data && e.data.open) location.href = e.data.open; });
}
function pushUnavailable() {
  if (!window.isSecureContext) {
    return "Notifications need HTTPS. Start the app with `amux serve --tailscale`, or `--tls` with a certificate this phone trusts, and open it at its https:// address. Or use `amux phone ntfy`.";
  }
  if (!("serviceWorker" in navigator) || !("PushManager" in window) || !("Notification" in window)) {
    if (isIOS && !standalone) return "On iPhone and iPad, notifications work in the home-screen app only (iOS 16.4 or later): tap Share → Add to Home Screen, then open amux from the home screen.";
    if (isIOS) return "Notifications need iOS 16.4 or later.";
    return "This browser can't receive push notifications. `amux phone ntfy` works with any phone.";
  }
  if (Notification.permission === "denied") return "Notifications are blocked for this app. Allow them in the browser's site settings (iPhone: Settings → Notifications → amux), then come back.";
  return null;
}
// The server's key and setup, and whether it still has this browser's subscription (it
// drops one the push service has forgotten); `active`: waiting runs reach it by push.
const pushInfo = (sub) => api("/api/push" + (sub ? `?endpoint=${enc(sub.endpoint)}` : ""));
const deviceName = () => (navigator.userAgent.match(/iPhone|iPad|Android|Macintosh|Windows|Linux/) || ["browser"])[0].replace("Macintosh", "Mac");
function b64u(s) {
  const raw = atob(s.replace(/-/g, "+").replace(/_/g, "/") + "=".repeat((4 - s.length % 4) % 4));
  return Uint8Array.from(raw, (c) => c.charCodeAt(0));
}
async function pushOn(reg, key) {
  // Asked first, straight from the tap: iOS only asks from a user's gesture.
  if (await Notification.requestPermission() !== "granted") throw new Error("Notifications weren't allowed. " + (pushUnavailable() || ""));
  const old = await reg.pushManager.getSubscription();
  if (old) await old.unsubscribe();
  const sub = await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: b64u(key) });
  return api("/api/push", { ...sub.toJSON(), device: deviceName() });
}
async function pushOff(sub) {
  const endpoint = sub.endpoint;
  await sub.unsubscribe();
  return api("/api/push/remove", { endpoint });
}
async function loadPush(msg) {
  const box = $("#push");
  box.replaceChildren(el("h2", null, "Notifications"));
  const note = el("p", "note");
  const why = pushUnavailable();
  if (why) { note.textContent = why; box.append(note); return; }
  let reg, sub, info;
  try {
    reg = await swReady;
    sub = await reg.pushManager.getSubscription();
    info = await pushInfo(sub);
  } catch (e) {
    note.textContent = `Notifications can't start here: ${e.message}. With --tls, the phone has to trust the certificate (install /amux-cert.pem).`;
    box.append(note);
    return;
  }
  // On only if amux still has this subscription; a stale one is replaced by turning on.
  const on = !!sub && info.subscribed;
  pushActive = !!info.active;
  const btn = el("button", on ? "" : "primary", on ? "Turn off on this device" : "Turn on for this device");
  const other = info.service && info.service !== "webpush" ? ` amux sends to ${info.service} now: \`amux phone webpush\` on the computer sends here instead.` : "";
  const off = sub ? "amux no longer has this device's subscription: turn it on again." : "Get a notification when a run needs you, even with the app closed.";
  note.textContent = msg || (on ? "This device gets amux's notifications, even with the app closed." : off) + other;
  btn.onclick = () => {
    btn.disabled = true;
    (on ? pushOff(sub) : pushOn(reg, info.publicKey)).then((m) => loadPush(m), (e) => loadPush(e.message));
  };
  box.append(btn, note);
}

// ---- live updates --------------------------------------------------------------------
// Changes on this computer arrive at once over Server-Sent Events; everything (including
// other machines) is also re-read every few seconds in case a stream drops.
let pending = null;
function soon() { clearTimeout(pending); pending = setTimeout(refresh, 150); }
function listen() {
  const es = new EventSource("/api/events");
  es.onopen = () => $("#live").classList.add("on");
  es.onmessage = () => { soon(); if (current && runView === "screen") setTimeout(loadScreen, 200); };
  es.onerror = () => { $("#live").classList.remove("on"); es.close(); setTimeout(listen, 5000); };
}
document.addEventListener("visibilitychange", () => { if (!document.hidden) refresh(); });

// A tapped phone notification opens its run: /#run=<id>.
function openFromLink() {
  const m = location.hash.match(/^#run=(.+)$/);
  if (!m) return;
  history.replaceState(null, "", location.pathname);
  showTab("runs"); runView = "screen"; openRun(decodeURIComponent(m[1]));
}
window.addEventListener("hashchange", openFromLink);

refresh().then(openFromLink, openFromLink);
listen();
setInterval(refresh, 5000);
setInterval(() => { if (current && runView === "screen" && !document.hidden) loadScreen(); }, 1500);
setInterval(() => { if (tab === "monitor" && !current && !document.hidden) loadMonitor(); }, 3000);
