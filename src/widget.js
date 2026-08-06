// Taskbar widget: one segmented cell per account, flowed across two rows.
//
// The layout rule (from design/taskbar-flowed-cells-mockups.html, treatment 01):
// accounts in reading order — Codex first, then Claude — split ceil(n/2) onto
// the top row and the rest below. So 1 Codex + 2 Claude puts the Codex and the
// first Claude on top and the second Claude across the row underneath, instead
// of one stretched Codex cell above a cramped pair.

import { clamp, elapsedPercent, esc } from "./shared.js";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const host = document.getElementById("widget");
const content = document.getElementById("content");
const grip = document.getElementById("grip");

/** Horizontal padding on `.w`, doubled — the rail is the window less that. */
const SIDE_PADDING = 18;
const CGAP = 2;
/** Mirrors MIN_WIDTH/MAX_WIDTH in widget.rs, which clamps authoritatively. Kept
 *  here so a drag past the limit doesn't build up slack that has to be undone
 *  before the width responds again. */
const MIN_WIDTH = 72;
const MAX_WIDTH = 420;

/** Rail width available to the cells right now. Follows the window, since the
 *  widget is resizable. */
function rail() {
  return Math.max(24, window.innerWidth - SIDE_PADDING);
}
/** Breathing room a name needs inside its cell before it reads as cramped. */
const NAME_PAD = 6;

/** Label size, scaled to the strip the widget landed in and published to CSS.
 *  JS owns it because the fit test below measures against it — a hardcoded
 *  estimate in one place and a font-size in the other would drift apart. */
function labelPx() {
  return Math.min(9, Math.max(6, window.innerHeight * 0.15));
}

/** Real text measurement rather than a characters-times-a-constant guess:
 *  account names are proportional ("Team" is far narrower than "Personal"),
 *  and the guess is what let names overflow their cells the first time. */
const measurer = document.createElement("canvas").getContext("2d");
function textWidth(text, px) {
  measurer.font = `800 ${px}px -apple-system, "Segoe UI", system-ui, sans-serif`;
  return measurer.measureText(text).width;
}

let data = null;
/** Drawing options, owned by the panel's Settings tab and pushed here. */
let opts = { show_pace: true, show_weekly: true };

/** One account's two windows, in the shape the renderer wants. */
function account(label, provider, five, week) {
  if (!five && !week) return null;
  return { label, provider, five, week };
}

/** Quota windows are grouped the same way the backend groups them when it
 *  derives `primary`/`secondary` (see codex.rs `gauge_for_group`), so this
 *  fallback finds the same gauge if those fields are ever absent. */
function gaugeForGroup(quotas, group) {
  return quotas?.find((q) => q.group === group)?.gauge || null;
}

/** Codex is a single account today; Claude reports one entry per config
 *  directory. Both arrive here as the same flat shape.
 *
 *  Either window may be missing: Codex plans that only meter weekly report no
 *  session window at all, and `column` draws those as a single bar. */
function accounts(usage) {
  const out = [];
  const cx = usage?.codex;
  if (cx?.available) {
    const five = cx.primary || gaugeForGroup(cx.quotas, "session");
    const week = cx.secondary || gaugeForGroup(cx.quotas, "weekly");
    const a = account("Codex", "codex", five, week);
    if (a) out.push(a);
  }
  const cl = usage?.claude;
  if (cl?.available) {
    const list = cl.accounts?.length ? cl.accounts : null;
    if (list) {
      for (const acct of list) {
        const a = account(acct.label || "Claude", "claude", acct.five_hour, acct.seven_day);
        if (a) out.push(a);
      }
    } else {
      const a = account("Claude", "claude", cl.five_hour, cl.seven_day);
      if (a) out.push(a);
    }
  }
  return out;
}

/** ceil(n/2) on top, the rest below. One account gets a single full row. */
function rows(list) {
  if (list.length <= 1) return [list];
  const top = Math.ceil(list.length / 2);
  return [list.slice(0, top), list.slice(top)];
}

const pct = (g) => clamp(Number(g?.used_percent) || 0, 0, 100);

/** The whole name when it fits, the initial when it doesn't. Never a fragment:
 *  a truncated name is worse than no name, because it looks like a bug. */
function nameFor(acct, cols) {
  const cellWidth = (rail() - CGAP * (cols - 1)) / cols;
  const px = labelPx();
  const fits = (s) => textWidth(s, px) + NAME_PAD <= cellWidth;
  const label = acct.label.trim();
  if (fits(label)) return label;

  // Real account labels are derived from folder names, so they often lead with
  // the provider — "claude-work", ".codex personal". The cell's colour already
  // says which provider it is, so that token is the first thing worth dropping.
  const words = label.split(/[\s\-_/\\.]+/).filter(Boolean);
  const distinct = words.length > 1 && /^(claude|codex|\.claude|\.codex)$/i.test(words[0])
    ? words.slice(1)
    : words;
  for (const candidate of [distinct.join(" "), distinct[0]]) {
    if (candidate && fits(candidate)) return candidate;
  }
  return ((distinct[0] || label)[0] || "?").toUpperCase();
}

/** `draw` rather than `opts` — the module-level `opts` holds the user's saved
 *  settings, and shadowing it here would make a future reach for
 *  `opts.show_pace` inside this function silently read the wrong object. */
function cell(g, colour, draw = {}) {
  if (!g) return `<span class="cell ${draw.cls || ""}" style="--p:0;--c:${colour}"></span>`;
  const p = pct(g);
  const hot = p >= 90;
  const pace = draw.pace ? elapsedPercent(g) : null;
  const mark = pace === null ? "" : `<i style="--t:${pace.toFixed(1)}"></i>`;
  const label = draw.name ? `<b>${esc(draw.name)}</b>` : "";
  return `<span class="cell ${hot ? "hot" : ""} ${draw.cls || ""}"
    style="--p:${p.toFixed(1)};--c:${colour}"><u></u>${mark}${label}</span>`;
}

function column(acct, cols) {
  const colour = acct.provider === "codex" ? "var(--codex)" : "var(--claude)";
  const name = nameFor(acct, cols);
  const pace = opts.show_pace;
  const week = opts.show_weekly ? acct.week : null;

  // Plans that report only one window — weekly-only, or session-only — get a
  // single bar filling the whole slot rather than a real bar stacked on an
  // empty one. An empty bar reads as "nothing used", which is the opposite of
  // "this account has no such window". Filling the slot also keeps every
  // column the same height, so the rows stay aligned. Turning the weekly bar
  // off in Settings puts every account down this same path.
  //
  // The fallback deliberately reads the *real* weekly, not the filtered one: an
  // account whose only window is weekly still has to draw something when the
  // weekly bar is switched off, or it disappears from the widget entirely.
  const only = acct.five && week ? null : acct.five || acct.week;
  if (only) {
    return `<div class="col">${cell(only, colour, { pace, name, cls: "solo" })}</div>`;
  }

  return `<div class="col">
    ${cell(acct.five, colour, { pace, name })}
    ${cell(week, colour, { cls: "wk", pace })}
  </div>`;
}

function render() {
  document.documentElement.style.setProperty("--label-px", `${labelPx().toFixed(2)}px`);
  const list = accounts(data);
  if (!list.length) {
    content.innerHTML = `<div class="idle"></div>`;
    return;
  }
  content.innerHTML = `<div class="grid">${rows(list)
    .map(
      (row) =>
        `<div class="row">${row.map((a) => column(a, row.length)).join("")}</div>`
    )
    .join("")}</div>`;
}

async function refresh() {
  try {
    const cached = await invoke("get_cached_usage");
    if (cached) data = cached;
  } catch {
    // Keep the last snapshot on screen; the widget never blanks on a hiccup.
  }
  render();
}

async function loadOptions() {
  try {
    opts = await invoke("get_widget_settings");
  } catch {
    // Defaults are the same as the backend's, so a failure here just means the
    // widget draws everything until the next change comes through.
  }
  render();
}

// Both of these are permission-gated. Reporting a failure rather than letting
// it become an unhandled rejection: without them the widget silently stops
// updating live and only refreshes on the minute timer below.
function subscribe(name, handler) {
  listen(name, handler).catch((err) => {
    console.error(`widget: cannot listen for "${name}"`, err);
  });
}

// The backend broadcasts this whenever a background collection finishes.
subscribe("usage-updated", refresh);
// ...and this when the Settings tab changes how the widget should draw.
subscribe("widget-settings", (event) => {
  if (event.payload) opts = event.payload;
  render();
});
// The strip changes height when the taskbar is switched to small icons, or when
// the widget moves to a display at another scale. Labels are sized off that.
// Throttled: this also fires on every frame of a grip-drag, and each render is
// a full rebuild plus a text measurement per cell.
let renderQueued = false;
window.addEventListener("resize", () => {
  if (renderQueued) return;
  renderQueued = true;
  requestAnimationFrame(() => {
    renderQueued = false;
    render();
  });
});
// The pace mark moves with the clock even when the numbers don't, and a window
// that has quietly reset should stop looking full.
setInterval(refresh, 60_000);

// A click anywhere on the widget opens the panel; a drag moves it along the
// taskbar. Telling those apart needs a movement threshold — landing on a 114px
// target and pressing in one motion always carries a pixel or two of travel,
// and treating that as a drag swallows the click.
const DRAG_THRESHOLD_PX = 5;

/** Where the press started, or null once it has been claimed as a drag. */
let press = null;
/** An in-progress move along the bar: the gap it started from, and how far the
 *  pointer has travelled since. Dragging right shrinks the gap to the tray. */
let moving = null;
let moveQueued = false;

function scheduleMove(commit) {
  if (!moving) return;
  const gap = Math.max(0, moving.from - moving.dx);
  if (commit) {
    invoke("set_widget_gap", { gap, commit: true }).catch((err) =>
      console.error("widget: set_widget_gap failed", err)
    );
    return;
  }
  // One reposition per frame: pointermove fires far more often, and each call
  // is an IPC round trip plus a window move.
  if (moveQueued) return;
  moveQueued = true;
  requestAnimationFrame(() => {
    moveQueued = false;
    if (moving) invoke("set_widget_gap", { gap: Math.max(0, moving.from - moving.dx), commit: false }).catch(() => {});
  });
}

host.addEventListener("pointerdown", (e) => {
  if (e.button !== 0) return;
  if (e.target === grip) {
    startResize(e);
    return;
  }
  press = { x: e.clientX, y: e.clientY };
});

// ---- resizing the left edge ----
// Movement deltas rather than absolute coordinates: the window's left edge is
// moving under the cursor as it resizes, so anything measured against the
// window would chase itself.
let resizing = null;
let queued = false;

function startResize(e) {
  resizing = { from: window.innerWidth, dx: 0 };
  grip.setPointerCapture?.(e.pointerId);
  e.preventDefault();
}

function resizeTo(commit) {
  const width = Math.min(MAX_WIDTH, Math.max(MIN_WIDTH, resizing.from - resizing.dx));
  invoke("set_widget_width", { width, commit }).catch(() => {});
}

grip.addEventListener("pointermove", (e) => {
  if (!resizing) return;
  resizing.dx += e.movementX;
  // One resize per frame at most; a pointermove can fire far more often, and
  // each one is an IPC round trip plus a window resize.
  if (queued) return;
  queued = true;
  requestAnimationFrame(() => {
    queued = false;
    if (resizing) resizeTo(false);
  });
});

for (const kind of ["pointerup", "pointercancel"]) {
  grip.addEventListener(kind, (e) => {
    if (!resizing) return;
    resizeTo(true); // the release is the only thing that touches disk
    resizing = null;
    grip.releasePointerCapture?.(e.pointerId);
  });
}

host.addEventListener("pointermove", (e) => {
  if (moving) {
    moving.dx += e.movementX;
    scheduleMove(false);
    return;
  }
  if (!press) return;
  if (Math.hypot(e.clientX - press.x, e.clientY - press.y) < DRAG_THRESHOLD_PX) return;
  // Past the threshold, this is a move rather than a click.
  //
  // Deliberately not `startDragging()`: that hands the window to the OS drag
  // loop, which will happily carry it into the middle of the desktop. The
  // widget only belongs on the taskbar, so it drives its own position the same
  // way the resize grip does — the backend places it, and its strip clamp is
  // the only place it can end up.
  press = null;
  moving = { from: opts.tray_gap ?? 10, dx: 0 };
  host.setPointerCapture?.(e.pointerId);
});

host.addEventListener("pointerup", (e) => {
  if (moving) {
    scheduleMove(true);
    moving = null;
    host.releasePointerCapture?.(e.pointerId);
    return;
  }
  if (!press || e.button !== 0) return;
  press = null;
  invoke("open_panel").catch((err) => console.error("widget: open_panel failed", err));
});

// A press that ends anywhere else — off the widget, or cancelled by the drag
// loop — must not leave a stale press behind to fire on the next release.
for (const kind of ["pointerup", "pointercancel"]) {
  window.addEventListener(kind, (e) => {
    if (e.target === host) return;
    press = null;
    if (moving) {
      scheduleMove(true);
      moving = null;
    }
  });
}

loadOptions();
refresh();
