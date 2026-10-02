// bosn ci dashboard: runs, the stage/job/step tree, step logs and runner
// management, live over /v1/events. Untrusted text only ever goes through
// textContent; writes are same-origin POSTs (the daemon checks Origin).
"use strict";

const ICON = {
  success: "✓", failure: "✗", error: "✗", timed_out: "⏱", cancelled: "⊘",
  skipped: "↷", unsupported: "⚠", incomplete: "⚠", refused: "⚠",
  in_progress: "●", running: "●", queued: "○", completed: "✓", done: "✓",
};
const $ = (id) => document.getElementById(id);
let selected = runFromPath();

function runFromPath() {
  const m = location.pathname.match(/^\/ci\/runs\/([0-9a-f-]{36})$/);
  return m ? m[1] : null;
}

function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "class") node.className = v;
    else if (k.startsWith("on")) node.addEventListener(k.slice(2), v);
    else node.setAttribute(k, v);
  }
  for (const child of children) node.append(child);
  return node;
}

// Status is shown by icon and word, never by colour alone.
function badge(word) {
  const w = word || "queued";
  return el("span", { class: `status ${w}` }, `${ICON[w] || "•"} ${w.replace("_", " ")}`);
}

async function api(path, options = {}) {
  const response = await fetch(path, { credentials: "same-origin", ...options });
  if (response.status === 401) {
    $("detail").replaceChildren(el("p", { class: "hint" }, "Session expired: run `bosn ui` for a new link."));
    throw new Error("unauthorized");
  }
  const body = await response.json();
  if (!response.ok) throw new Error(body.message || response.statusText);
  return body;
}

const post = (path, body) =>
  api(path, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body || {}) });

async function refreshList() {
  const list = await api("/v1/runs?limit=100");
  const r = list.runners;
  $("runners").replaceChildren(
    `${r.running} running · ${r.queued} queued · limit ${r.limit}`,
    r.drained ? el("strong", {}, " · drained") : "",
  );
  $("limit").placeholder = r.limit;
  $("runs").replaceChildren(...list.runs.map((run) => el("li", {},
    el("button", {
      type: "button",
      "aria-current": String(run.id === selected),
      onclick: () => select(run.id),
    },
      el("span", {}, badge(run.conclusion || run.state), " ", run.workflow.replace(".github/workflows/", "")),
      el("span", { class: "meta" }, `${run.sha.slice(0, 12)}${run.dirty ? " +dirty" : ""} · ${run.trigger}/${run.mode} · ${run.actor}`),
    ))));
}

function select(run) {
  selected = run;
  history.replaceState(null, "", `/ci/runs/${run}`);
  refreshList().catch(() => {});
  showRun().catch((e) => $("detail").replaceChildren(el("p", { class: "hint" }, e.message)));
}

async function showRun() {
  if (!selected) return;
  const run = await api(`/v1/runs/${selected}`);
  const report = run.state === "done" ? await api(`/v1/runs/${selected}/report?tail=40`) : null;
  const parts = [
    el("h2", {}, `Run ${run.id}`),
    el("p", {}, badge(run.conclusion || run.state), " ", `exit ${run.exit_code ?? "–"} · ${run.workflow} · ${run.sha}${run.dirty ? " +dirty" : ""}`),
    el("p", { class: "meta" }, `${run.trigger}/${run.mode} · ${run.actor} · cleanup: ${run.cleanup || "–"}`),
    el("div", { class: "controls" },
      el("button", { type: "button", onclick: () => post(`/v1/runs/${run.id}/cancel`).then(showRun) }, "Cancel"),
      el("button", { type: "button", onclick: () => post(`/v1/runs/${run.id}/retry`).then((r) => select(r.run)) }, "Retry"),
    ),
  ];
  if (run.reason) parts.push(el("p", { class: "meta" }, run.reason));
  if (report && report.first_failure) {
    const f = report.first_failure;
    parts.push(el("div", { class: "failure-box" },
      el("strong", {}, `First failure: ${f.job} — ${f.step || "?"} (exit ${f.exit_code ?? "?"})`),
      el("pre", {}, f.tail.join("\n"))));
  }
  parts.push(el("h3", {}, "Jobs"), tree(run));
  parts.push(el("h3", {}, "Log"), el("pre", { id: "log" }, ""));
  $("detail").replaceChildren(...parts);
  loadLog({});
}

function tree(run) {
  const groups = (run.tree && run.tree.groups) || [];
  if (!groups.length) return el("p", { class: "hint" }, "No jobs yet.");
  return el("ul", { class: "tree" }, ...groups.map((group) => el("li", {},
    el("span", { class: "meta" }, `stage ${group.name}`),
    el("ul", {}, ...group.jobs.map((job) => el("li", { class: "job" },
      el("button", { type: "button", onclick: () => loadLog({ job: job.key }) },
        badge(job.conclusion || job.status), " ", job.key),
      el("ul", { class: "steps" }, ...job.sections.map((s) => el("li", { class: "step" },
        el("button", { type: "button", onclick: () => loadLog({ job: job.key, section: `${s.stage}:${s.id}` }) },
          badge(s.conclusion || s.status), ` ${s.stage} ${s.name}`,
          s.duration_ms != null ? el("span", { class: "meta" }, ` ${s.duration_ms} ms`) : ""))))))))));
}

// Cursor-paged log for the selected run (optionally one job or step).
async function loadLog(filter) {
  const pre = $("log");
  if (!pre || !selected) return;
  pre.textContent = "";
  let since = 0;
  for (let page = 0; page < 40; page++) {
    const q = new URLSearchParams({ since_seq: since, limit: 1000, ...filter });
    const logs = await api(`/v1/runs/${selected}/logs?${q}`);
    pre.append(logs.records.map((r) => r.text).join("\n") + (logs.records.length ? "\n" : ""));
    since = logs.next_seq;
    if (!logs.more) break;
  }
}

// Live feed: debounce refreshes; a resync refetches everything.
let pending = null;
function schedule() {
  if (pending) return;
  pending = setTimeout(() => {
    pending = null;
    refreshList().catch(() => {});
    if (selected) showRun().catch(() => {});
  }, 400);
}
function connect() {
  const source = new EventSource("/v1/events");
  source.onmessage = (message) => {
    const event = JSON.parse(message.data);
    if (event.type === "resync" || !selected || event.run === selected) schedule();
    else refreshList().catch(() => {});
  };
  source.onerror = () => { source.close(); setTimeout(connect, 2000); };
}

$("drain").addEventListener("click", () => post("/v1/runners", { action: "drain" }).then(refreshList));
$("resume").addEventListener("click", () => post("/v1/runners", { action: "resume" }).then(refreshList));
$("set-limit").addEventListener("click", () => {
  const limit = Number($("limit").value);
  if (limit >= 1) post("/v1/runners", { action: "set_limit", limit }).then(refreshList);
});

refreshList().then(() => selected && showRun()).catch(() => {});
connect();
