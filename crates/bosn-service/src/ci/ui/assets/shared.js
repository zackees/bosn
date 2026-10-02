// Shared by the dashboard and the widget pages: status icons, element
// helpers, same-origin API calls and a two-step confirmation for
// destructive actions (no browser dialogs, which a webview may not show).
// Untrusted text only ever goes through textContent.
"use strict";

const ICON = {
  success: "✓", failure: "✗", error: "✗", timed_out: "⏱", cancelled: "⊘",
  skipped: "↷", unsupported: "⚠", remote_only: "☁", incomplete: "⚠", refused: "⚠",
  in_progress: "●", running: "●", queued: "○", completed: "✓", done: "✓",
};
const CONFIRM_MS = 4000;

const $ = (id) => document.getElementById(id);

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

class Unauthorized extends Error {}

// GET, or a JSON POST when `body` is given (writes carry the page's Origin,
// which the daemon checks). Errors carry the daemon's message.
async function request(path, body) {
  const options = body === undefined ? {} : {
    method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body),
  };
  const response = await fetch(path, { credentials: "same-origin", ...options });
  if (response.status === 401) throw new Unauthorized("session expired");
  const reply = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(reply.message || response.statusText);
  return reply;
}

// A button that needs a second click within CONFIRM_MS before `action` runs.
function confirmButton(label, action, onDone) {
  const button = el("button", { type: "button", class: "destructive" }, label);
  let armed = null;
  const disarm = () => {
    clearTimeout(armed);
    armed = null;
    button.textContent = label;
    button.classList.remove("confirming");
  };
  button.addEventListener("click", () => {
    if (!armed) {
      button.textContent = `Confirm ${label.toLowerCase()}?`;
      button.classList.add("confirming");
      armed = setTimeout(disarm, CONFIRM_MS);
      return;
    }
    disarm();
    action().then(onDone, (error) => { button.title = error.message; });
  });
  return button;
}

function humanBytes(bytes) {
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return unit === 0 ? `${bytes} B` : `${value.toFixed(1)} ${units[unit]}`;
}

function cacheText(cache) {
  return cache.bytes === null ? "cache: none" : `cache: ${humanBytes(cache.bytes)}`;
}

function runnersText(r) {
  return `${r.running} running · ${r.queued} queued · limit ${r.limit}${r.drained ? " · drained" : ""}`;
}

const WEEK_SECS = 7 * 24 * 60 * 60;
