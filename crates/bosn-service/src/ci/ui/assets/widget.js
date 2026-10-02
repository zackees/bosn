// The widget's bubble and panel. Clicks never call native code: they POST to
// the daemon, which forwards a command to the widget process (no page IPC).
// Helpers (icons, `el`, `request`, `confirmButton`) come from /shared.js.
"use strict";

const api = request;

function word(run) { return run.conclusion || run.state; }

// Bubble: counts and the worst state (failed > running > queued > ok).
async function renderBubble() {
  const list = await api("/v1/runs?limit=50");
  const running = list.runs.filter((r) => r.state === "running").length;
  const queued = list.runs.filter((r) => r.state === "queued").length;
  const recent = list.runs.filter((r) => r.state === "done").slice(0, 5);
  const failed = recent.filter((r) => ["failure", "error", "timed_out"].includes(r.conclusion)).length;
  const ring = $("ring");
  const state = failed ? "failed" : running ? "running" : queued ? "queued" : "ok";
  ring.className = state;
  $("count").textContent = String(running + queued || failed);
  $("label").textContent = failed ? `${failed} failed` : running ? "running" : queued ? "queued" : "idle";
  ring.setAttribute("aria-label", `bosn ci: ${running} running, ${queued} queued, ${failed} recent failures`);
}

// Panel: every workspace's runs, runner state and management actions.
async function renderPanel() {
  const list = await api("/v1/runs?limit=30");
  const r = list.runners;
  $("runners").textContent = runnersText(r);
  $("runs").replaceChildren(...list.runs.map((run) => {
    const item = document.createElement("li");
    const button = document.createElement("button");
    button.type = "button";
    const head = document.createElement("span");
    head.className = word(run);
    head.textContent = `${ICON[word(run)] || "•"} ${word(run).replace("_", " ")} · ${run.workflow.replace(".github/workflows/", "")}`;
    const meta = document.createElement("span");
    meta.className = "meta";
    const repo = run.workspace.split("/").pop();
    meta.textContent = `${repo} · ${run.branch || "-"} · ${run.sha.slice(0, 8)}${run.dirty ? " +dirty" : ""} · ${run.actor} · ${run.jobs.completed}/${run.jobs.total} jobs`;
    button.append(head, meta);
    button.addEventListener("click", () => api("/v1/widget/open", { path: `/ci/runs/${run.id}` }));
    // The run's own action: retry a finished run, cancel a live one.
    const action = run.state === "done"
      ? el("button", { type: "button", onclick: () => api(`/v1/runs/${run.id}/retry`, {}).then(render) }, "Retry")
      : confirmButton("Cancel", () => api(`/v1/runs/${run.id}/cancel`, {}), render);
    action.classList.add("row-action");
    item.append(button, action);
    return item;
  }));
}

const render = document.body.classList.contains("bubble") ? renderBubble : renderPanel;
if ($("ring")) $("ring").addEventListener("click", () => api("/v1/widget/toggle", {}));
if ($("open-full")) $("open-full").addEventListener("click", () => api("/v1/widget/open", { path: "/" }));
if ($("drain")) $("drain").addEventListener("click", () => api("/v1/runners", { action: "drain" }).then(render));
if ($("resume")) $("resume").addEventListener("click", () => api("/v1/runners", { action: "resume" }).then(render));
if ($("set-limit")) $("set-limit").addEventListener("click", () => {
  const limit = Number($("limit").value);
  if (limit >= 1) api("/v1/runners", { action: "set_limit", limit }).then(render);
});
if ($("prune-slot")) $("prune-slot").replaceChildren(confirmButton("Prune old runs",
  () => api("/v1/runners", { action: "prune_cache", older_than_secs: WEEK_SECS }), render));
if ($("clear-cache-slot")) $("clear-cache-slot").replaceChildren(confirmButton("Clear cache",
  () => api("/v1/runners", { action: "clear_cache" }), (reply) => { $("cache").textContent = cacheText(reply.cache); }));

let pending = null;
function schedule() {
  if (!pending) pending = setTimeout(() => { pending = null; render().catch(() => {}); }, 300);
}
function connect() {
  const source = new EventSource("/v1/events");
  source.onmessage = schedule;
  source.onerror = () => { source.close(); setTimeout(connect, 2000); };
}
render().catch(() => {});
connect();
