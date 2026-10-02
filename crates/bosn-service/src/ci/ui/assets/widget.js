// The widget's bubble and panel. Clicks never call native code: they POST to
// the daemon, which forwards a command to the widget process (no page IPC).
"use strict";

const $ = (id) => document.getElementById(id);
const ICON = { success: "✓", failure: "✗", error: "✗", timed_out: "⏱", cancelled: "⊘",
               incomplete: "⚠", refused: "⚠", running: "●", queued: "○" };

async function api(path, body) {
  const options = body === undefined ? {} : {
    method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body),
  };
  const response = await fetch(path, { credentials: "same-origin", ...options });
  if (!response.ok) throw new Error(response.statusText);
  return response.json();
}

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
  $("runners").textContent = `${r.running} running · ${r.queued} queued · limit ${r.limit}${r.drained ? " · drained" : ""}`;
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
    item.append(button);
    return item;
  }));
}

const render = document.body.classList.contains("bubble") ? renderBubble : renderPanel;
if ($("ring")) $("ring").addEventListener("click", () => api("/v1/widget/toggle", {}));
if ($("open-full")) $("open-full").addEventListener("click", () => api("/v1/widget/open", { path: "/" }));
if ($("drain")) $("drain").addEventListener("click", () => api("/v1/runners", { action: "drain" }).then(render));
if ($("resume")) $("resume").addEventListener("click", () => api("/v1/runners", { action: "resume" }).then(render));

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
