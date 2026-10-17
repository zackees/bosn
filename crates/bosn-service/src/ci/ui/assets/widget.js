// The widget's bubble and panel. Clicks never call native code: they POST to
// the daemon, which forwards a command to the widget process (no page IPC).
// Helpers (icons, `el`, `request`, `confirmButton`) come from /shared.js.
// Untrusted text only ever goes through textContent.
"use strict";

const api = request;
const FAILED = ["failure", "error", "timed_out"];
const DAY_SECS = 24 * 60 * 60;

function word(run) { return run.conclusion || run.state; }
const plural = (n, one, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;
const now = () => Date.now() / 1000;

// "42s", "3m 12s", "1h 4m", "2d 3h".
function span(secs) {
  const s = Math.max(0, Math.floor(secs));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${s % 60}s`;
  if (s < DAY_SECS) return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
  return `${Math.floor(s / DAY_SECS)}d ${Math.floor((s % DAY_SECS) / 3600)}h`;
}
function ago(at) {
  const s = now() - at;
  return s < 10 ? "just now" : `${span(s).split(" ")[0]} ago`;
}

// What the run's time column says: live runs count up, finished ones age.
function when(run) {
  if (run.state === "running") return `running ${span(now() - (run.started_at || run.created_at))}`;
  if (run.state === "queued") return `queued ${span(now() - run.created_at)}`;
  const took = run.finished_at && run.started_at ? ` · ${span(run.finished_at - run.started_at)}` : "";
  return `${ago(run.finished_at || run.created_at)}${took}`;
}

// ---- bubble --------------------------------------------------------------

// Counts and the worst state (failed > running > queued > ok).
async function renderBubble() {
  const list = await api("/v1/runs?limit=50");
  const running = list.runs.filter((r) => r.state === "running").length;
  const queued = list.runs.filter((r) => r.state === "queued").length;
  const recent = list.runs.filter((r) => r.state === "done").slice(0, 5);
  const failed = recent.filter((r) => FAILED.includes(r.conclusion)).length;
  const ring = $("ring");
  const state = failed ? "failed" : running ? "running" : queued ? "queued" : "ok";
  ring.className = state;
  $("count").textContent = String(running + queued || failed);
  $("label").textContent = failed ? `${failed} failed` : running ? "running" : queued ? "queued" : "idle";
  ring.setAttribute("aria-label", `bosn ci: ${running} running, ${queued} queued, ${failed} recent failures`);
}

// ---- panel ---------------------------------------------------------------

const view = { list: null, filter: "all", online: true, updated: 0 };

function toast(message) {
  const box = $("toast");
  box.textContent = message;
  box.hidden = false;
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => { box.hidden = true; }, 4000);
}

// Run an action, refresh, and say what went wrong instead of failing silently.
function act(promise) {
  return promise.then(renderPanel, (error) => toast(error.message || "The daemon refused that"));
}
const runners = (body) => act(api("/v1/runners", body));

function visible(runs) {
  if (view.filter === "active") return runs.filter((r) => r.state !== "done");
  if (view.filter === "failed") return runs.filter((r) => FAILED.includes(r.conclusion));
  return runs;
}

function runItem(run) {
  const state = word(run);
  const title = run.job
    ? `${run.workflow.replace(".github/workflows/", "")} · ${run.job}`
    : run.workflow.replace(".github/workflows/", "");
  const repo = run.repository || run.workspace.split("/").pop();
  const glyph = run.state === "done" ? (ICON[state] || "•") : "";
  const meta = el("div", { class: "meta" },
    `${repo} · ${run.branch || "detached"} · `, el("code", {}, run.sha.slice(0, 8)));
  if (run.dirty) meta.append(el("span", { class: "tag" }, "dirty"));
  meta.append(` · ${run.actor}`);
  if (run.reason) meta.append(` · ${run.reason}`);
  const item = el("li", {
    class: `run ${state}`, tabindex: "0", role: "button",
    "aria-label": `${state.replace("_", " ")}: ${title} in ${repo}. Open in dashboard.`,
    title: `${state.replace("_", " ")} — click to open in the dashboard`,
  },
  el("span", { class: "dot", "aria-hidden": "true" }, glyph),
  el("span", { class: "title" }, title),
  el("span", { class: "when", "data-run": run.id }, when(run)),
  meta);
  if (run.state === "running") {
    const { total, completed } = run.jobs;
    const bar = el("div", { class: total ? "progress" : "progress indeterminate", title: total ? `${completed}/${total} jobs` : "starting" }, el("i"));
    bar.firstChild.style.width = total ? `${Math.max(4, (100 * completed) / total)}%` : "";
    item.append(bar);
  }
  const action = run.state === "done"
    ? el("button", { type: "button", onclick: () => act(api(`/v1/runs/${run.id}/retry`, {})) }, "Retry")
    : confirmButton("Cancel", () => api(`/v1/runs/${run.id}/cancel`, {}), renderPanel);
  item.append(el("div", { class: "actions" }, action));
  const open = (event) => {
    if (event.target.closest(".actions")) return;
    act(api("/v1/widget/open", { path: `/ci/runs/${run.id}` }));
  };
  item.addEventListener("click", open);
  item.addEventListener("keydown", (event) => { if (event.key === "Enter" || event.key === " ") { event.preventDefault(); open(event); } });
  return item;
}

function banner(list) {
  const box = $("banner");
  const r = list?.runners;
  box.className = "banner";
  let text = null;
  let action = null;
  if (!view.online) {
    box.classList.add("error");
    text = "Lost the bosn daemon. Reconnecting…";
  } else if (r?.drained) {
    text = r.queued ? `Runners are drained: ${plural(r.queued, "queued run")} waiting.` : "Runners are drained: new runs wait.";
    action = el("button", { type: "button", onclick: () => runners({ action: "resume" }) }, "Resume");
  } else if (r?.hold === "dispatcher stalled") {
    box.classList.add("error");
    text = "The dispatcher stalled; the next submission restarts it.";
  } else if (r?.hold === "cache clearing") {
    text = "Clearing the cache; queued runs start when it is done.";
  } else if (r?.hold === "concurrency limit" && r.queued) {
    text = `${r.limit === 1 ? "The only slot is" : `All ${r.limit} slots are`} busy; ${plural(r.queued, "run")} waiting.`;
    action = el("button", { type: "button", onclick: () => runners({ action: "set_limit", limit: r.limit + 1 }) }, "Add a slot");
  }
  box.hidden = text === null;
  box.replaceChildren(...(text ? [el("span", {}, text)] : []), ...(action ? [action] : []));
}

function stats(list) {
  const r = list.runners;
  const failed = list.runs.filter((run) => FAILED.includes(run.conclusion) && now() - (run.finished_at || 0) < DAY_SECS).length;
  for (const [id, value] of [["stat-running", r.running], ["stat-queued", r.queued], ["stat-failed", failed]]) {
    $(id).textContent = String(value);
    $(id).parentElement.classList.toggle("zero", value === 0);
  }
  $("stat-failed").parentElement.title = "Failed in the last 24 hours";
  $("stat-limit").textContent = `${Math.min(r.running, r.limit)}/${r.limit}`;
  if (document.activeElement !== $("limit")) $("limit").value = r.limit;
  $("drain-toggle").textContent = r.drained ? "Resume" : "Drain";
  $("drain-text").textContent = r.drained ? "Drained: queued runs wait." : "New runs start when a slot is free.";
}

function empty(list, shown) {
  const box = $("empty");
  box.hidden = shown.length > 0;
  if (!box.hidden) {
    box.replaceChildren(...(list.runs.length === 0
      ? [el("b", {}, "No runs yet"), el("span", {}, "Start one from a repository with"), el("code", {}, "bosn ci run")]
      : [el("b", {}, view.filter === "failed" ? "Nothing failed" : "Nothing running"),
        el("span", {}, view.filter === "failed" ? "Every recent run passed or is still going." : "The runners are idle.")]));
  }
}

async function renderPanel() {
  try {
    view.list = await api("/v1/runs?limit=50");
    view.online = true;
    view.updated = now();
  } catch (error) {
    view.online = false;
    if (error instanceof Unauthorized) toast("This panel's session expired. Hide it and click the tray icon again.");
  }
  $("link").classList.toggle("down", !view.online);
  $("link").title = view.online ? "Connected to the daemon" : "Reconnecting to the daemon";
  banner(view.list);
  if (!view.list) return;
  stats(view.list);
  const shown = visible(view.list.runs);
  $("runs").replaceChildren(...shown.map(runItem));
  empty(view.list, shown);
  tick();
}

// Live times without refetching.
function tick() {
  if (!view.list) return;
  const runs = new Map(view.list.runs.map((run) => [run.id, run]));
  for (const node of document.querySelectorAll(".when[data-run]")) {
    const run = runs.get(node.dataset.run);
    if (run) node.textContent = when(run);
  }
  $("updated").textContent = view.online ? `Updated ${ago(view.updated)}` : "Offline";
}

function setFilter(filter) {
  view.filter = filter;
  for (const button of document.querySelectorAll(".filters button")) {
    button.setAttribute("aria-pressed", String(button.dataset.filter === filter));
  }
  renderPanel();
}

function setLimit(limit) {
  if (Number.isInteger(limit) && limit >= 1 && limit <= 256) runners({ action: "set_limit", limit });
}

function wirePanel() {
  const hide = () => api("/v1/widget/toggle", {}).catch(() => {});
  $("hide").addEventListener("click", hide);
  document.addEventListener("keydown", (event) => { if (event.key === "Escape") hide(); });
  $("open-full").addEventListener("click", () => act(api("/v1/widget/open", { path: "/" })));
  for (const button of document.querySelectorAll("[data-filter]")) {
    button.addEventListener("click", () => setFilter(button.dataset.filter));
  }
  $("drain-toggle").addEventListener("click", () => runners({ action: view.list?.runners.drained ? "resume" : "drain" }));
  $("limit-down").addEventListener("click", () => setLimit(Number($("limit").value) - 1));
  $("limit-up").addEventListener("click", () => setLimit(Number($("limit").value) + 1));
  $("limit").addEventListener("change", () => setLimit(Number($("limit").value)));
  $("measure-cache").addEventListener("click", () => {
    $("cache").textContent = "Cache: measuring…";
    api("/v1/runners", { action: "cache_usage" }).then(
      (reply) => { $("cache").textContent = cacheText(reply.cache); },
      (error) => { $("cache").textContent = "Cache: unavailable"; toast(error.message); });
  });
  $("prune-slot").replaceChildren(confirmButton("Prune week-old runs",
    () => api("/v1/runners", { action: "prune_cache", older_than_secs: WEEK_SECS }), renderPanel));
  $("clear-cache-slot").replaceChildren(confirmButton("Clear cache",
    () => api("/v1/runners", { action: "clear_cache" }), (reply) => { $("cache").textContent = cacheText(reply.cache); }));
  // Quit is deliberate (auto-launch stays off for this login), so it confirms.
  $("quit-slot").replaceChildren(confirmButton("Quit widget", () => api("/v1/widget/quit", {}), () => {}));
  setInterval(tick, 1000);
}

// ---- shared wiring -------------------------------------------------------

const isBubble = document.body.classList.contains("bubble");
const render = isBubble ? renderBubble : renderPanel;
if (isBubble) $("ring").addEventListener("click", () => api("/v1/widget/toggle", {}));
else wirePanel();

let pending = null;
function schedule() {
  if (!pending) pending = setTimeout(() => { pending = null; render().catch(() => {}); }, 300);
}
function connect() {
  const source = new EventSource("/v1/events");
  source.onopen = schedule;
  source.onmessage = schedule;
  source.onerror = () => {
    source.close();
    if (!isBubble) { view.online = false; render().catch(() => {}); }
    setTimeout(connect, 2000);
  };
}
render().catch(() => {});
connect();
