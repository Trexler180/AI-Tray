import {
  accountShown,
  clamp,
  claudeAccount,
  CODEX_ACCOUNT,
  codexReserve,
  elapsedPercent,
  esc,
  historyKey,
  meterTier,
  runsOutAt,
  tightestLimit,
  widgetRows,
} from "./shared.js";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

let data = null;
let activeTab = "overview";
let creditsOpen = false; // the Credits detail screen, layered over any tab
let notificationSettings = {
  codex: false,
  claude: false,
  codex_resets: false,
  claude_resets: false,
  warn_levels: [90, 95],
  used_up: true,
  forecast: true,
  quiet_hours: false,
  quiet_start: 22 * 60,
  quiet_end: 8 * 60,
};
let notifyOptionsError = false; // the last alert-options change failed to save
// What the tray icon follows: "tightest", "logo", or an account id.
let trayIconMode = "tightest";
// Settings shows one section at a time; the page outgrew a single scroll.
const SETTINGS_SECTIONS = [
  ["alerts", "Alerts"],
  ["display", "Display"],
  ["widget", "Widget"],
  ["accounts", "Accounts"],
  ["advanced", "Advanced"],
];
let settingsSection = localStorage.getItem("settingsSection") || "alerts";
// One row per account on the Overview: "auto" switches it on from
// COMPACT_FROM accounts, where the full cards stop fitting without a scroll.
let compactMode = localStorage.getItem("compactOverview") || "auto";
const COMPACT_FROM = 4;
// The first-launch setup card, until it is dismissed or everything in it is on.
let welcomeDismissed = localStorage.getItem("welcomeDismissed") === "1";
let confirmingRemove = null; // Claude folder awaiting the inline "Remove?" confirm
let removeError = null; // why the last folder removal failed, if it did
// When the last refresh failed with numbers already on screen: what is shown is
// older than the header suggests, so the header says so.
let refreshFailedAt = null;
let widgetSettings = { enabled: false, tray_gap: 10, width: 114 };
// "ok" | "vertical_taskbar" | "no_taskbar" — why the widget is or is not placeable.
let widgetPlacement = "ok";
let widgetMonitors = [];
let widgetLoadError = "";
// Whether the model-scoped weekly gauge (e.g. the Fable-only limit) is shown.
let showModelWeekly = localStorage.getItem("showModelWeekly") !== "0";
// Meter direction: false (default) drains a full bar as the allowance is
// spent; true fills an empty bar instead.
let meterFillsUp = localStorage.getItem("meterFillsUp") === "1";
// A hairline on every time-based meter marking how far through the window the
// clock is, so the bar can be read as a pace and not just a level.
let meterPaceLine = localStorage.getItem("meterPaceLine") !== "0";
// A band at the fill edge covering the quota spent over the last
// `recentMinutes()`. Off by default: it can only speak for time the app was
// running, which is worth opting into rather than appearing unannounced.
// Whether the *widget* draws one is its own setting, but the reach is shared —
// see `recent_minutes` in widget.rs.
let showRecentBurn = localStorage.getItem("showRecentBurn") === "1";
// The reaches the band offers. Anything shorter than a quarter of an hour reads
// as 0% on most windows — the percentages providers report are too coarse to
// resolve it — and anything longer stops being "recent".
const RECENT_CHOICES = [
  { minutes: 15, label: "15m" },
  { minutes: 30, label: "30m" },
  { minutes: 60, label: "1h" },
  { minutes: 180, label: "3h" },
];
const recentMinutes = () => widgetSettings.recent_minutes || 60;
// Recent spend per window key, computed by the recorder. Null until the first
// load; an empty object means the record simply has nothing to say yet.
let recentBurns = null;
let notifyError = null; // provider whose toggle failed to save, if any
let updateState = null; // updater status + version, pushed from Rust on change
let updateError = null; // error from the last update toggle, if any
let startOnLogin = null; // whether the app registers itself to run at sign-in
let startOnLoginError = false; // the last start-on-login toggle failed to save
// Reset keys are "codex:<credit id>" or "claude:<account id>" — a Claude claim
// spends whichever grant the server names next, so the account is the handle.
let confirmingReset = null; // reset key awaiting the inline "Use reset" confirm
let usingReset = null; // reset key whose claim is in flight
let resetError = null; // error from the last Codex consume attempt, if any
let claudeResetNotice = null; // { acct, text, error } from the last Claude claim
let editingAccount = null; // account id whose Claude label is being renamed inline
let editingDraft = ""; // in-progress label text, kept across background re-renders
let editFocusPending = false; // focus the rename input once when editing starts
let addingFolder = false; // whether the "add Claude folder" input is open
let addFolderDraft = ""; // in-progress folder path, kept across background re-renders
let addFolderError = null; // error from the last add attempt, if any
let addFolderFocusPending = false; // focus the add-folder input once when opened
const expandedHealth = new Set(); // diagnostic rows currently expanded (Settings)
let windowHistory = null; // recorded quota windows, from the Rust recorder
let windowRange = localStorage.getItem("windowRange") || "day"; // day | week | two
// Which accounts each surface hides. Only the panel half is read here — the
// widget keeps its own, so an account can be on the taskbar and out of the
// panel or the other way round — but the whole record is held so Settings can
// draw both switches.
let accountVisibility = { panel_hidden: [], widget_hidden: [] };
let panelExpanded = false; // timeline showing its wide layout in a widened panel

// ---------- account visibility ----------
// Whether an account is drawn in the panel. Settings deliberately ignores this:
// it is the screen the switches live on, so it always lists every account.
const panelShows = (id) => accountShown(accountVisibility, "panel", id);
// The Claude accounts the panel is currently drawing.
const shownClaudeAccounts = () =>
  (data?.claude?.accounts || []).filter((a) => panelShows(claudeAccount(a.id)));
// Whether the Claude side has anything to show. A snapshot with no per-account
// list at all still stands for one account — the default folder — which the
// empty id keys, matching the widget and the Settings row that toggles it.
const claudeInPanel = () =>
  (data?.claude?.accounts || []).length
    ? shownClaudeAccounts().length > 0
    : panelShows(claudeAccount(""));
const codexInPanel = () => panelShows(CODEX_ACCOUNT);

// ---------- widget layout ----------
// The accounts the widget is drawing, in its own order — Codex first, then each
// Claude account — so Settings can offer a row for each and preview where they
// land. Identity and order only; the widget itself adds the gauges.
function widgetAccounts() {
  const out = [];
  const cx = data?.codex;
  if (cx?.available && accountShown(accountVisibility, "widget", CODEX_ACCOUNT))
    out.push({ id: CODEX_ACCOUNT, label: "Codex", provider: "codex" });
  const cl = data?.claude;
  if (cl?.available) {
    const list = cl.accounts || [];
    if (list.length) {
      for (const a of list) {
        const id = claudeAccount(a.id);
        if (accountShown(accountVisibility, "widget", id))
          out.push({ id, label: a.label || "Claude", provider: "claude" });
      }
      // The same fallback the widget draws when a snapshot carries no
      // per-account list at all.
    } else if (accountShown(accountVisibility, "widget", claudeAccount(""))) {
      out.push({ id: claudeAccount(""), label: "Claude", provider: "claude" });
    }
  }
  return out;
}
const widgetRowOf = (id) => widgetSettings.account_rows?.[id] || "auto";

// ---------- formatting helpers ----------
const usd = (n) => "$" + (Number.isFinite(n) ? n : 0).toFixed(2);
function tokens(n) {
  n = n || 0;
  if (n >= 1e9) return (n / 1e9).toFixed(2) + "B";
  if (n >= 1e6) return (n / 1e6).toFixed(1) + "M";
  if (n >= 1e3) return (n / 1e3).toFixed(1) + "K";
  return String(n);
}

// Wall-clock time in the machine's own 12/24-hour convention. `hour: "numeric"`
// rather than "2-digit" so it reads 4:42 pm, never 04:42 pm.
function clockTime(unix) {
  return new Date(unix * 1000)
    .toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })
    .toLowerCase();
}

// "resets 2h 10m · 12:52 pm". Beyond a day the bare clock time is useless on
// its own, so it picks up a weekday; past a week a weekday would be ambiguous,
// so it takes a date instead.
function resetText(g) {
  if (!g) return "";
  const relative = g.resets_in ? `resets ${g.resets_in}` : "";
  if (typeof g.resets_at !== "number") return esc(relative);
  const when = new Date(g.resets_at * 1000);
  const away = g.resets_at * 1000 - Date.now();
  let absolute = clockTime(g.resets_at);
  if (away >= 24 * 3600 * 1000) {
    const day =
      away >= 6.5 * 24 * 3600 * 1000
        ? when.toLocaleDateString([], { month: "short", day: "numeric" })
        : when.toLocaleDateString([], { weekday: "short" });
    absolute = `${day} ${absolute}`;
  }
  return esc(relative ? `${relative} · ${absolute}` : `resets ${absolute}`);
}

// ---------- icons ----------
// Drawn rather than typed: the ⟳ / ↻ font glyphs render as flattened ellipses
// and, because their ink sits high in the line box, wobble when rotated.
// Expand and shrink as a mirrored pair: arrows out to the corners, arrows back
// in from them. The ⤢/⤡ glyphs both read as "enlarge" — same diagonal, same
// outward heads — so the shrink control looked like a second expand button.
function expandIcon(px = 14, stroke = 1.9) {
  return `<svg viewBox="0 0 24 24" width="${px}" height="${px}" fill="none"
    stroke="currentColor" stroke-width="${stroke}" stroke-linecap="round"
    stroke-linejoin="round" aria-hidden="true">
    <polyline points="15 3 21 3 21 9" />
    <polyline points="9 21 3 21 3 15" />
    <line x1="21" y1="3" x2="14" y2="10" />
    <line x1="3" y1="21" x2="10" y2="14" />
  </svg>`;
}
function shrinkIcon(px = 14, stroke = 1.9) {
  return `<svg viewBox="0 0 24 24" width="${px}" height="${px}" fill="none"
    stroke="currentColor" stroke-width="${stroke}" stroke-linecap="round"
    stroke-linejoin="round" aria-hidden="true">
    <polyline points="20 10 14 10 14 4" />
    <polyline points="4 14 10 14 10 20" />
    <line x1="14" y1="10" x2="21" y2="3" />
    <line x1="10" y1="14" x2="3" y2="21" />
  </svg>`;
}

function refreshIcon(px = 14, stroke = 1.9) {
  return `<svg viewBox="0 0 24 24" width="${px}" height="${px}" fill="none"
    stroke="currentColor" stroke-width="${stroke}" stroke-linecap="round"
    stroke-linejoin="round" aria-hidden="true">
    <path d="M20.49 15a9 9 0 1 1-2.12-9.36L23 10" />
    <polyline points="23 4 23 10 17 10" />
  </svg>`;
}

// ---------- window identity ----------
const codexKey = (id) => historyKey("codex", "", id);
const claudeKey = (account, id) => historyKey("claude", account, id);

// ---------- recent usage ----------
// What one window burned lately, as the recorder measured it (see `recent_in`
// in windows_history.rs). Null when the band is switched off, the gauge has no
// recorded window behind it, or nothing has been recorded for it yet.
function recentBurn(key) {
  if (!showRecentBurn || !key) return null;
  return recentBurns?.[key] ?? null;
}

// "12% in the last 45m". The span is what the record covers, not what was asked
// for, so a figure that can only speak for ten minutes says so.
function recentText(recent) {
  const span = agoText(recent.covered_seconds * 1000);
  return recent.spent < 0.05
    ? `no usage in the last ${span}`
    : `${pctText(recent.spent)} in the last ${span}`;
}

// ---------- meters ----------
// One bar with its text inside: label + headline on the left, reset time on
// the right.
//
// Every meter is driven by the *used* percent, so the warn colouring is the
// same reading in both fill directions; only the width and the headline flip.
// Draining (the default) reads as "fuel left"; filling reads as "how much of
// the allowance is gone".
function meterBar(
  cls,
  usedPct,
  leftHtml,
  rightHtml,
  tip,
  attrs = "",
  elapsedPct = null,
  recentPct = null
) {
  const used = Math.min(100, Math.max(0, usedPct));
  const fill = meterFillsUp ? used : 100 - used;
  // Anything that rounds to 0% in the headline renders as a bare track: the
  // fill's edge line would otherwise leave a 2px sliver reading as "nearly
  // empty" when the meter is empty.
  const empty = fill < 0.5;
  // The pace line marks where the clock has got to in the same direction the
  // bar runs, so it can be read against the fill edge without translating:
  // whichever is further along is the one being spent faster.
  const mark =
    elapsedPct === null
      ? ""
      : `<div class="bmark" style="left:${meterFillsUp ? elapsedPct : 100 - elapsedPct}%"></div>`;
  // The recent band always ends at the fill edge, so it reads as the chunk just
  // burned off whichever way the bar runs: over the fill when meters fill up,
  // and into the emptied track when they drain. It can't exceed what has been
  // spent in total — a window that rolled mid-range would otherwise report more
  // recent usage than the meter shows.
  const recent = Math.min(recentPct ?? 0, used);
  // Pinned to the fill edge from whichever side it grows away from, so the
  // minimum width a thin band gets in CSS extends it into the bar rather than
  // pushing it out past the level it is measured from. Which side it hangs from
  // is the only thing the fill direction changes — the band itself is the same
  // colour over the fill as it is in the emptied track.
  const band =
    recent < 0.3
      ? ""
      : `<div class="brecent" style="${
          meterFillsUp ? "right" : "left"
        }:${100 - used}%;width:${recent}%"></div>`;
  return `
    <div class="bgauge ${cls}" title="${tip}" ${attrs}>
      <div class="bfill${empty ? " empty" : ""}" style="width:${empty ? 0 : fill}%"></div>
      ${band}
      ${mark}
      <div class="btxt"><span class="bl">${leftHtml}</span><span class="rr">${rightHtml}</span></div>
    </div>`;
}

// usedPercent 0..100. The headline follows the fill direction so the number
// and the bar always describe the same thing; the other figure is in the
// tooltip either way.
// `opts.key` is the recorder's key for this window (see `liveWindows`), which
// is what the recent-usage band is looked up by. A gauge without one — usage
// credits, or a window the provider reports no length for — simply goes
// without the band.
//
// Three tiers on top of the provider colour: amber from 80% used, red once the
// window will run out before it resets (or past 95%), and a striped "used up"
// state whose headline is when it comes back — a full red bar only says
// "stop", not "until when". Each tier also carries a word, so colour is never
// the only signal. `opts.forecast: false` leaves out the "out by" line, for
// places that already say it.
function gauge(label, g, opts = {}) {
  if (!g) return "";
  const usedPercent = Math.min(100, Math.max(0, Number(g.used_percent) || 0));
  const left = (100 - usedPercent).toFixed(0);
  const tier = meterTier(g);
  const runsOut = runsOutAt(g);
  const cls = [opts.claude ? "claude" : "", tier === "ok" ? "" : tier].join(" ").trim();
  const elapsed = meterPaceLine ? elapsedPercent(g) : null;
  const recent = recentBurn(opts.key);
  const tip = `${usedPercent.toFixed(0)}% used · ${left}% left${
    g.resets_in ? ` · resets in ${esc(g.resets_in)}` : ""
  }${elapsed === null ? "" : ` · ${elapsed.toFixed(0)}% of the window gone`}${
    runsOut === null ? "" : ` · runs out about ${atText(runsOut)} at this pace`
  }${recent === null ? "" : ` · ${recentText(recent)}`}`;
  const tag = {
    low: `<span class="state low">low</span>`,
    crit: `<span class="state crit">${runsOut === null ? "almost out" : "runs out"}</span>`,
  }[tier] || "";
  const headline =
    tier === "out"
      ? `<span class="state out">used up</span>`
      : (meterFillsUp
          ? `<b>${usedPercent.toFixed(0)}%</b><span class="sub"> used</span>`
          : `<b>${left}%</b><span class="sub"> left</span>`) + tag;
  const bar = meterBar(
    cls,
    usedPercent,
    `${esc(label)} ${headline}`,
    tier === "out" ? backText(g) : resetText(g),
    tip,
    meterAria(label, usedPercent, tip),
    elapsed,
    recent?.matched ? recent.spent : null
  );
  return opts.forecast === false || runsOut === null ? bar : bar + forecastLine(runsOut, g);
}

// What a screen reader announces for one meter: its name, the number the bar
// shows, and the same detail the tooltip gives a mouse.
function meterAria(label, usedPercent, tip) {
  const value = meterFillsUp ? usedPercent : 100 - usedPercent;
  return `role="meter" aria-valuemin="0" aria-valuemax="100" aria-valuenow="${value.toFixed(
    0
  )}" aria-label="${esc(label)}: ${tip}"`;
}

// "back in 58m · 5:10 pm" — a used-up window's headline is its return.
function backText(g) {
  if (typeof g.resets_at !== "number") return "used up";
  return esc(`back in ${g.resets_in || durText(g.resets_at * 1000 - Date.now())} · ${atText(g.resets_at)}`);
}

// "18 min", "1 h 20 min", "3 d 4 h" — a gap for the middle of a sentence.
function gapText(seconds) {
  const minutes = Math.max(0, Math.round(seconds / 60));
  const d = Math.floor(minutes / 1440);
  const h = Math.floor((minutes % 1440) / 60);
  const m = minutes % 60;
  if (d) return h ? `${d} d ${h} h` : `${d} d`;
  if (!h) return `${m} min`;
  return m ? `${h} h ${m} min` : `${h} h`;
}

// "4:52 pm" today, "Thu 9:00 am" within the week, "Oct 21 9:00 am" beyond. A
// bare clock time days away reads as today.
function atText(unix) {
  const when = new Date(unix * 1000);
  const days = Math.round((new Date(when).setHours(0, 0, 0, 0) - new Date().setHours(0, 0, 0, 0)) / 86400000);
  if (days === 0) return clockTime(unix);
  const day =
    days > 0 && days < 7
      ? when.toLocaleDateString([], { weekday: "short" })
      : when.toLocaleDateString([], { month: "short", day: "numeric" });
  return `${day} ${clockTime(unix)}`;
}

const trendIcon = `<svg viewBox="0 0 24 24" width="12" height="12" fill="none" stroke="currentColor"
  stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
  <polyline points="3 17 9 11 13 15 21 7" /><polyline points="15 7 21 7 21 13" /></svg>`;

// The line under a meter whose pace runs it out early. Only drawn then: a
// forecast under every calm meter would be noise.
function forecastLine(at, g) {
  return `<div class="fc">${trendIcon}<span>Out by <b>${esc(atText(at))}</b>, ${esc(
    gapText(g.resets_at - at)
  )} before reset</span></div>`;
}

// ---------- usage credits ----------
// Accounts whose live snapshot reported an extra_usage block at all.
function creditAccounts() {
  return shownClaudeAccounts().filter((a) => a.extra_usage);
}
// Credits are worth a meter only when the switch is on and a cap is set.
function creditsActive(a) {
  const extra = a.extra_usage;
  return !!(extra && extra.is_enabled && (extra.monthly_limit || 0) > 0);
}
function creditsUsedPercent(extra) {
  const cap = extra.monthly_limit || 0;
  const used = Math.min(cap, extra.used_credits || 0);
  const pct = typeof extra.utilization === "number"
    ? extra.utilization
    : cap > 0 ? (used / cap) * 100 : 0;
  return Math.min(100, Math.max(0, pct));
}

// The violet money meter: "Credits $13.20 of $20". With `link`, the whole bar
// navigates to the Credits screen.
function creditsBar(a, opts = {}) {
  if (!creditsActive(a)) return "";
  const extra = a.extra_usage;
  const cap = extra.monthly_limit;
  const used = Math.min(cap, extra.used_credits || 0);
  const usedPct = creditsUsedPercent(extra);
  // Money has no clock to forecast against, so only the plain thresholds apply.
  const tier = usedPct >= 95 ? "crit" : usedPct >= 80 ? "low" : "";
  const cls = ["credit", tier, opts.link ? "link" : ""].join(" ").trim();
  const tip = `${usd(used)} of the ${usd(cap)} monthly cap spent`;
  const right = `${usedPct.toFixed(0)}% used${opts.link ? ` <span class="go">›</span>` : ""}`;
  const amount = meterFillsUp ? used : Math.max(0, cap - used);
  return meterBar(
    cls,
    usedPct,
    `${esc(opts.label || "Credits")} <b>${usd(amount)}</b><span class="sub"> of ${usd(cap)}</span>`,
    right,
    tip,
    // A meter that opens the Credits screen is a button to a screen reader.
    opts.link
      ? `data-credits tabindex="0" role="button" aria-label="${esc(`Credits: ${tip}. Open the Credits screen`)}"`
      : meterAria(opts.label || "Credits", usedPct, tip)
  );
}

// ---------- bar chart ----------
// Value per day for the last two weeks, in the same unit as the figures above
// it. A dashed line marks the peak with its amount, today's bar is picked out,
// and the ends of the axis are dated — without those the bars are a shape with
// no scale.
function chart(daily, claude) {
  const days = (daily || []).slice(-14);
  if (!days.length) return "";
  const max = Math.max(...days.map((d) => d.cost), 0.01);
  const bars = days
    .map((d, i) => {
      const h = Math.round((d.cost / max) * 100);
      const today = i === days.length - 1;
      const cls = ["bar", d.cost <= 0 ? "empty" : "", today ? "today" : ""].join(" ").trim();
      const title = `${esc(d.date)}: ${usd(d.cost)} · ${tokens(d.tokens)} tok`;
      return `<div class="${cls}" style="height:${Math.max(4, h)}%" title="${title}"></div>`;
    })
    .join("");
  const first = new Date(`${days[0].date}T00:00`).toLocaleDateString([], {
    month: "short",
    day: "numeric",
  });
  return `<div class="chart ${claude ? "claude" : ""}" role="img"
      aria-label="${esc(`Value per day, last ${days.length} days. Peak ${usd(max)}.`)}">
      <div class="maxline"></div><span class="maxlab">${esc(usd(max))} peak</span>${bars}</div>
    <div class="chart-x"><span>${esc(first)}</span><span>value per day</span><span class="t">Today</span></div>`;
}

// The working directories the value came from, most expensive first: today's
// when there is any yet, otherwise the month's. The top few by name, the rest
// folded into one row.
function projectList(projects) {
  const list = projects || [];
  if (!list.length) return "";
  const today = list.some((p) => p.cost_today > 0);
  const cost = (p) => (today ? p.cost_today : p.cost_30d);
  const rows = [...list].filter((p) => cost(p) > 0).sort((a, b) => cost(b) - cost(a));
  if (!rows.length) return "";
  const shown = rows.slice(0, 3);
  const rest = rows.slice(3);
  const top = cost(shown[0]);
  const row = (name, title, value) => `<div class="proj-row" title="${esc(title)}">
      <span class="nm">${esc(name)}</span>
      <span class="tr"><i style="width:${Math.max(2, (value / top) * 100)}%"></i></span>
      <span class="v">${usd(value)}</span></div>`;
  let html = shown.map((p) => row(p.name, p.path || "Folder not recorded", cost(p))).join("");
  if (rest.length)
    html += row(
      `${rest.length} other${rest.length === 1 ? "" : "s"}`,
      rest.map((p) => p.name).join(", "),
      rest.reduce((sum, p) => sum + cost(p), 0)
    );
  return `<div class="proj">
    <div class="proj-lab"><span>${today ? "Today" : "Last 30 days"} by project</span></div>
    ${html}</div>`;
}

// ---------- data-source status ----------
function ageText(seconds) {
  seconds = Math.max(0, Number(seconds) || 0);
  if (seconds < 60) return `${Math.round(seconds)} sec`;
  if (seconds < 3600) return `${Math.round(seconds / 60)} min`;
  return `${(seconds / 3600).toFixed(seconds < 7200 ? 1 : 0)} hr`;
}

function healthTitle(health) {
  if (!health) return "Data unavailable";
  if (health.source === "live_api") return "Live API";
  if (health.source === "memory_cache")
    return `Cached API result · ${ageText(health.stale_age_seconds)} old`;
  if (health.source === "local_logs")
    return `Local logs${health.files_scanned ? ` · ${health.files_scanned} files` : ""}`;
  return "Data unavailable";
}

// Local logs are the expected source for cost history but a fallback for live
// quotas, so the caller says which reading counts as healthy.
function healthOk(health, localIsNormal = false) {
  if (!health) return false;
  if (health.source === "live_api") return true;
  if (health.source === "local_logs") return localIsNormal;
  return false;
}

// Warn-only health: a healthy live source renders nothing, and an amber pill
// names the problem when data is cached, logs-only, or missing. The full
// diagnostics stay in Settings → Data sources.
function healthBadge(health, opts = {}) {
  const ok = healthOk(health, opts.localIsNormal) && !opts.forceWarn;
  if (ok) return "";
  let text = "no data";
  if (health?.source === "memory_cache") text = `cached ${ageText(health.stale_age_seconds)}`;
  else if (health?.source === "local_logs") text = "logs only";
  if (opts.warnText && healthOk(health, opts.localIsNormal)) text = opts.warnText;
  const detail = opts.extra ? ` · ${opts.extra}` : "";
  return `<span class="pill warn-pill"
    title="${esc(healthTitle(health) + detail)}">${esc(text)}</span>`;
}

// ---------- panel header (absorbs the old footer) ----------
// The cluster at the right of every header: when the data was collected, then
// the buttons that act on the panel. Shared by the compact headers and the
// expanded timeline's own header so the controls keep one size, one order and
// one set of hover states wherever they appear.
function headerControls(opts = {}) {
  const stamp = data?.generated_at ? clockTime(data.generated_at) : "—";
  const busy = spinning() ? " busy" : "";
  // `rightExtra` is for controls that act on the panel itself, like the
  // timeline's expand; it sits next to refresh rather than beside the title.
  const extra = opts.rightExtra || "";
  if (opts.hideRefresh)
    return `<span class="hd-right"><span>${esc(opts.rightText || "")}</span>${extra}</span>`;
  const failed = refreshFailedAt && !busy ? " fail" : "";
  const label = failed ? "Refresh failed — try again" : "Refresh";
  return `<span class="hd-right"><span title="Last updated">${esc(stamp)}</span>${extra}
    <button class="hd-refresh${busy}${failed}" id="refreshBtn" title="${label}"
      aria-label="${label}">${refreshIcon()}</button></span>`;
}

function header(title, extras = "", opts = {}) {
  // A failed refresh leaves the last numbers up, which is right, but the
  // header's timestamp alone reads like they are current.
  const failed =
    refreshFailedAt && !opts.hideRefresh && data?.generated_at
      ? `<div class="banner amber small" role="status">Couldn't refresh — showing numbers from ${esc(
          clockTime(data.generated_at)
        )}.</div>`
      : "";
  return `<div class="hd"><span class="hd-title">${esc(title)}</span>${extras}${headerControls(
    opts
  )}</div>${failed}`;
}

// ---------- merged value + history card ----------
function valueCard(estimate, health, today, todayTok, m30, tok30, daily, claude, projects) {
  const confidence = estimate?.confidence || "low";
  const shaky = confidence !== "high" || !!estimate?.pricing_stale;
  const reviewed = estimate?.pricing_reviewed_at
    ? ` Pricing reviewed ${estimate.pricing_reviewed_at}.`
    : "";
  const unknown = (estimate?.unknown_models || []).length
    ? ` Unknown models using fallback pricing: ${estimate.unknown_models.join(", ")}.`
    : "";
  const stale = estimate?.pricing_stale ? " Pricing may be stale." : "";
  const explanation =
    `Not your subscription bill. Estimated from local token logs using API list prices.${reviewed}${unknown}${stale}`;
  return `<div class="stat-card">
    <div class="stat-card-head">
      <span>API-equivalent value</span>
      <span class="info-tip" tabindex="0" role="img"
        aria-label="${esc(explanation)}" title="${esc(explanation)}">i</span>
      ${healthBadge(health, {
        localIsNormal: true,
        forceWarn: shaky,
        warnText: `${confidence} estimate`,
        extra: `${confidence} confidence`,
      })}
    </div>
    <div class="stat-pair">
      <div class="stat">
        <div class="stat-lab">Today</div>
        <div class="val">${usd(today)} <small>${tokens(todayTok)}</small></div>
      </div>
      <div class="stat">
        <div class="stat-lab">Last 30 days</div>
        <div class="val">${usd(m30)} <small>${tokens(tok30)}</small></div>
      </div>
    </div>
    ${chart(daily, claude)}
    ${projectList(projects)}
  </div>`;
}

// Local calendar date like "Jul 18, 2026" from a unix-seconds timestamp.
function fmtDate(unix) {
  if (typeof unix !== "number") return "";
  return new Date(unix * 1000).toLocaleDateString([], {
    year: "numeric",
    month: "short",
    day: "numeric",
  });
}

// Count of currently-redeemable reset credits in a Codex snapshot.
function availableResets(c) {
  return ((c.resets && c.resets.credits) || []).filter((x) => x.status === "available");
}

// Whether a deadline is within a day, which tints a reset card amber.
const expiresSoon = (unix) => typeof unix === "number" && unix * 1000 - Date.now() <= 24 * 3600 * 1000;

// One reset card: title, expiry line (already escaped), and a Use action that
// expands into an inline two-step confirm (no native dialog).
function resetCard(key, title, meta, soon, extraClass = "") {
  const action =
    usingReset === key
      ? `<button class="reset-use" disabled>Using…</button>`
      : confirmingReset === key
        ? `<div class="reset-confirm">
            <span class="reset-q">Use it?</span>
            <button class="acct-btn save" data-reset-confirm="${esc(key)}" title="Confirm">✓</button>
            <button class="acct-btn" data-reset-cancel title="Cancel">✕</button>
          </div>`
        : `<button class="reset-use" data-reset-use="${esc(key)}"${
            usingReset ? " disabled" : ""
          }>Use reset</button>`;
  return `<div class="reset-card ${extraClass} ${soon ? "soon" : ""}">
    <div class="reset-info">
      <div class="reset-title">${esc(title)}</div>
      ${meta ? `<div class="reset-meta ${soon ? "soon" : ""}">${meta}</div>` : ""}
    </div>
    ${action}
  </div>`;
}

function codexResetCard(cr) {
  const meta = cr.expires_at
    ? `Expires ${fmtDate(cr.expires_at)}${cr.expires_in ? ` · ${esc(cr.expires_in)} left` : ""}`
    : cr.granted_at
      ? `Granted ${fmtDate(cr.granted_at)}`
      : "";
  return resetCard(
    `codex:${cr.id}`,
    cr.title || "Free rate-limit reset",
    meta,
    expiresSoon(cr.expires_at)
  );
}

// What a Claude claim came back with, in T3 Code's words.
const CLAUDE_RESET_OUTCOMES = {
  reset: "Reset applied. Your windows have cleared.",
  nothingToReset: "Nothing to reset right now — you're not at a limit, so the reset was kept.",
  noCredit: "No reset credit left.",
  alreadyRedeemed: "That credit was already redeemed.",
};

const claudeResetCount = (a) => (a.live && a.resets ? a.resets.available_count || 0 : 0);

// Banked resets for one Claude account, under its meters. Nothing at all for
// an account that isn't eligible; a quiet line when it is but has none banked.
function claudeResetBlock(a) {
  const r = a.resets;
  const notice = claudeResetNotice && claudeResetNotice.acct === a.id ? claudeResetNotice : null;
  let html = notice
    ? `<div class="${notice.error ? "banner small" : "sec-sub"}">${
        notice.error ? `Couldn't use that reset — ${esc(notice.text)}` : esc(notice.text)
      }</div>`
    : "";
  if (!r) return html;
  const count = claudeResetCount(a);
  if (!count || !r.next_credit_id) return html + `<div class="sec-sub">No resets banked.</div>`;
  const meta = r.next_expires_at
    ? `Use by ${fmtDate(r.next_expires_at)}${
        r.next_expires_in ? ` · ${esc(r.next_expires_in)} left` : ""
      }`
    : "Refills your limits now";
  const title = `${count} reset${count === 1 ? "" : "s"} banked`;
  return html + resetCard(`claude:${a.id}`, title, meta, expiresSoon(r.next_expires_at), "claude");
}

// The "Reset credits" section for the Codex tab. The notify toggle that used to
// live here is in Settings → Notifications.
function resetSection(c) {
  const r = c.resets;
  const credits = availableResets(c);
  let html = `<div class="grp-label">Reset credits${
    credits.length ? ` · ${credits.length} available` : ""
  }</div>`;
  if (resetError)
    html += `<div class="banner small">Couldn't use that reset — ${esc(resetError)}</div>`;
  if (!r) html += `<div class="sec-sub">Live reset-credit info unavailable.</div>`;
  else if (!credits.length) html += `<div class="sec-sub">No reset credits right now.</div>`;
  else for (const cr of credits) html += codexResetCard(cr);
  return html;
}

// Luna Reserve's meters, with a line saying where it stands. It only spends
// once the regular limit is drained, so "how full" means little without "is it
// in use yet" beside it.
function reserveBlock(cx) {
  const reserve = codexReserve(cx);
  if (!reserve) return "";
  let html = "";
  for (const quota of reserve.quotas)
    html += gauge(quota.label, quota.gauge, { key: codexKey(quota.id) });
  html += `<div class="sec-sub">${esc(reserveStatus(reserve))}</div>`;
  return html;
}

function reserveStatus(reserve) {
  if (reserve.spent)
    return reserve.drained
      ? "Reserve used up too — both come back when their windows reset."
      : "Reserve used up — it refills when its window resets.";
  return reserve.drained
    ? "In use — your regular limit is used up, so Codex is drawing on the reserve."
    : "Standing by — used once your regular limit runs out.";
}

// ---------- tab renderers ----------
function renderCodex() {
  const c = data.codex;
  if (!c.available)
    return (
      header("Codex") +
      `<div class="empty-state">No Codex sessions found.<br/>Looked in <code>~/.codex/sessions</code>.</div>`
    );

  const extras = `${healthBadge(c.health)}${
    c.plan_type ? `<span class="pill">${esc(c.plan_type)}</span>` : ""
  }`;
  let html = header("Codex", extras);

  if ((c.quotas || []).length) {
    for (const quota of c.quotas)
      if (quota.group !== "reserve")
        html += gauge(quota.label, quota.gauge, { key: codexKey(quota.id) });
    html += reserveBlock(c);
  } else {
    // No quota list means no reported window length, so the session label
    // can't name its own hours here.
    html += gauge("Session", c.primary, { key: codexKey("session") });
    html += gauge("Weekly", c.secondary, { key: codexKey("weekly") });
  }
  if (!c.live)
    html += `<div class="banner">Live usage unavailable — showing the last numbers from local session logs.</div>`;

  if (typeof c.credits === "number") {
    html += `<button class="row-link" data-credits>
      <span class="k">Credits</span>
      <span class="v">${esc(c.credits.toFixed(2))}</span>
      <span class="go">›</span>
    </button>`;
  }

  html += resetSection(c);
  html += valueCard(
    c.estimate,
    c.history_health,
    c.cost_today,
    c.tokens_today,
    c.cost_30d,
    c.tokens_30d,
    c.daily,
    false,
    c.projects
  );
  return html;
}

// One account's gauges. `sessionLabel` differs between the detailed tab
// ("Session (5h)") and the compact overview card ("Session").
function claudeGauges(acct, sessionLabel) {
  let h = "";
  const key = (id) => claudeKey(acct.id, id);
  if ((acct.quotas || []).length) {
    for (const quota of acct.quotas) {
      const scoped = quota.scope_model || quota.scope_surface;
      if (scoped && !showModelWeekly) continue;
      const label = quota.group === "session" ? sessionLabel : quota.label;
      h += gauge(label, quota.gauge, { claude: true, key: key(quota.id) });
    }
    return h;
  }
  h += gauge(sessionLabel, acct.five_hour, { claude: true, key: key("session") });
  h += gauge("Weekly", acct.seven_day, { claude: true, key: key("weekly") });
  const mg = acct.seven_day_model;
  if (mg && showModelWeekly)
    h += gauge(`Weekly (${mg.model})`, mg.gauge, { claude: true, key: key("weekly_model") });
  return h;
}

// The scoped-limit names, used to label the Settings → Display toggle. Empty
// when no account reports one, so the toggle never shows as dead UI.
function scopedLimitNames(accounts) {
  const scopes = [
    ...new Set(
      (accounts || []).flatMap((a) =>
        (a.quotas || [])
          .filter((q) => q.scope_model || q.scope_surface)
          .map((q) => q.scope_model || q.scope_surface)
      )
    ),
  ];
  if (!scopes.length) {
    for (const account of accounts || [])
      if (account.seven_day_model) scopes.push(account.seven_day_model.model);
  }
  return scopes;
}

// One account block on the Claude tab: name, status dot, then its gauges.
// Rename/remove moved to Settings → Claude accounts.
function renderClaudeAccount(a, multi) {
  const head = `<div class="acct-head">
    <span class="acct-label">${esc(a.label)}</span>
    ${healthBadge(a.health)}
    ${a.active && multi ? `<span class="pill">default</span>` : ""}
  </div>`;
  const body = a.live
    ? claudeGauges(a, "Session (5h)") + creditsBar(a, { link: true }) + claudeResetBlock(a)
    : `<div class="banner small">Live usage unavailable — open Claude Code signed in as this account to refresh it.</div>`;
  return `<div class="acct" data-acct="${esc(a.id)}">${multi ? head : ""}${body}</div>`;
}

function renderClaude() {
  const c = data.claude;
  if (!c.available)
    return (
      header("Claude") +
      `<div class="empty-state">No Claude data.<br/>Sign in with Claude Code, or check <code>~/.claude</code>.</div>`
    );

  const accounts = shownClaudeAccounts();
  const multi = accounts.length > 1;
  const primaryHealth = accounts.length ? accounts[0].health : null;

  let html = header("Claude", multi ? "" : healthBadge(primaryHealth));

  if (accounts.length) {
    for (const a of accounts) html += renderClaudeAccount(a, multi);
  } else {
    html += `<div class="banner">Live usage unavailable — token expired or offline. Open Claude Code to refresh it. Showing estimated cost from logs.</div>`;
  }

  html += valueCard(
    c.estimate,
    c.history_health,
    c.cost_today,
    c.tokens_today,
    c.cost_30d,
    c.tokens_30d,
    c.daily,
    true,
    c.projects
  );
  if (multi)
    html += `<div class="sec-sub">Combined across all accounts — local logs aren't per-account.</div>`;
  return html;
}

// ---------- the limit to worry about ----------
// The reset that would get a used-up Codex window back, in the reset key form
// the panel's confirm uses.
function codexResetKey(cx) {
  const credit = availableResets(cx)[0];
  return credit ? `codex:${credit.id}` : null;
}

// Every live window the panel is showing, flattened so they can be compared:
// which one runs out first, and what each account's tightest is. The tray icon
// makes the same choice in tray.rs, through the same rule in shared.js.
function panelLimits() {
  const out = [];
  const cx = data?.codex;
  if (cx?.live && codexInPanel()) {
    const reserve = codexReserve(cx);
    const quotas = (cx.quotas || []).length
      ? cx.quotas
      : [
          cx.primary && { label: "Session", group: "session", gauge: cx.primary },
          cx.secondary && { label: "Weekly", group: "weekly", gauge: cx.secondary },
        ].filter(Boolean);
    for (const q of quotas) {
      if (q.group === "additional") continue;
      // A reserve only matters while the main limit is drained.
      if (q.group === "reserve" && !reserve?.drained) continue;
      out.push({
        account: CODEX_ACCOUNT,
        tab: "codex",
        who: "Codex",
        name: "Codex",
        sub: cx.plan_type || "",
        provider: "codex",
        label: q.label,
        gauge: q.gauge,
        resetKey: codexResetKey(cx),
      });
    }
  }
  const accounts = shownClaudeAccounts().filter((a) => a.live);
  const multi = shownClaudeAccounts().length > 1;
  for (const a of accounts) {
    const quotas = (a.quotas || []).length
      ? a.quotas.filter((q) => showModelWeekly || !(q.scope_model || q.scope_surface))
      : [
          a.five_hour && { label: "Session", gauge: a.five_hour },
          a.seven_day && { label: "Weekly", gauge: a.seven_day },
          a.seven_day_model && showModelWeekly && {
            label: `Weekly (${a.seven_day_model.model})`,
            gauge: a.seven_day_model.gauge,
          },
        ].filter(Boolean);
    for (const q of quotas) {
      out.push({
        account: claudeAccount(a.id),
        tab: "claude",
        who: multi ? `Claude · ${a.label}` : "Claude",
        name: a.label || "Claude",
        sub: "Claude",
        provider: "claude",
        label: q.label,
        gauge: q.gauge,
        resetKey: claudeResetCount(a) && a.resets?.next_credit_id ? `claude:${a.id}` : null,
      });
    }
  }
  return out;
}

// Top of the Overview: the one window most likely to stop you, what its pace
// says, and the way out when there is one. Absent while everything is calm —
// the meters below say that on their own.
function tightCard(limits) {
  const top = tightestLimit(limits);
  if (!top) return "";
  const tier = meterTier(top.gauge);
  if (tier === "ok") return "";
  const g = top.gauge;
  const left = Math.max(0, 100 - (Number(g.used_percent) || 0));
  const runsOut = runsOutAt(g);
  const resets = typeof g.resets_at === "number" ? g.resets_at : null;
  // A strip, not a card: the meter it's about is right below with the full
  // picture, so this only has to say which window and when.
  const state =
    tier === "out" ? "used up" : runsOut !== null ? "runs out" : `${left.toFixed(0)}% left`;
  let when = "";
  if (tier === "out" && resets)
    when = `Back at <b>${esc(atText(resets))}</b>, in ${esc(durText(resets * 1000 - Date.now()))}`;
  else if (runsOut !== null)
    when = `Out by <b>${esc(atText(runsOut))}</b>, ${esc(gapText(resets - runsOut))} before reset`;
  else if (resets) when = esc(resetText(g).replace(/^resets/, "Resets"));
  const action = top.resetKey
    ? `<button class="btn small ${top.provider === "codex" ? "teal" : ""}" data-use-reset-go="${esc(
        top.resetKey
      )}">Use a reset</button>`
    : "";
  return `<div class="tight ${tier}" role="status">
    <div class="tight-txt">
      <div class="tight-line"><b>${esc(top.who)}</b> ${esc(top.label)} <span class="tight-state">${esc(
        state
      )}</span></div>
      ${when ? `<div class="tight-fc">${when}</div>` : ""}
    </div>${action}
  </div>`;
}

// Compact Overview: one row per account, its tightest window, worst first.
function compactOverview(limits) {
  const byAccount = new Map();
  for (const limit of limits) {
    if (!byAccount.has(limit.account)) byAccount.set(limit.account, []);
    byAccount.get(limit.account).push(limit);
  }
  const rows = [...byAccount.values()].map((list) => tightestLimit(list));
  const order = (l) => [
    ["ok", "low", "crit", "out"].indexOf(meterTier(l.gauge)),
    Number(l.gauge.used_percent) || 0,
  ];
  rows.sort((a, b) => {
    const [ta, ua] = order(a);
    const [tb, ub] = order(b);
    return tb - ta || ub - ua;
  });
  const html = rows
    .map((l) => {
      const g = l.gauge;
      const tier = meterTier(g);
      const left = Math.max(0, 100 - (Number(g.used_percent) || 0));
      const runsOut = runsOutAt(g);
      const span = g.window_minutes ? spanShort(g.window_minutes * MINUTE) : "";
      const when =
        tier === "out" && typeof g.resets_at === "number"
          ? `back ${whenShort(g.resets_at)}`
          : runsOut !== null
            ? `out ${whenShort(runsOut)}`
            : typeof g.resets_at === "number"
              ? `resets ${whenShort(g.resets_at)}`
              : "";
      return `<button class="cmp-row" data-goto="${l.tab}">
        <span class="d ${l.provider}"></span>
        <span class="cmp-mid"><span class="nm">${esc(l.name)}<span>${esc(
          [l.sub, span].filter(Boolean).join(" · ")
        )}</span></span>
          <span class="mb"><i class="${l.provider} ${tier}" style="width:${left}%"></i></span></span>
        <span class="r"><b class="${tier}">${tier === "out" ? "used up" : `${left.toFixed(0)}% left`}</b><br/>${esc(
          when
        )}</span>
      </button>`;
    })
    .join("");
  return `<div class="ov-card cmp">${html}</div>
    <div class="sec-sub">Worst first. Each row opens its tab.</div>`;
}

// "4:10 pm" today, "Thu" within a week, else a date.
function whenShort(unix) {
  const away = unix * 1000 - Date.now();
  if (away < 20 * HOUR) return clockTime(unix);
  if (away < 6.5 * DAY) return new Date(unix * 1000).toLocaleDateString([], { weekday: "short" });
  return new Date(unix * 1000).toLocaleDateString([], { month: "short", day: "numeric" });
}

function shownAccountCount() {
  return (codexInPanel() && data?.codex?.available ? 1 : 0) + shownClaudeAccounts().length;
}

const useCompact = () =>
  compactMode === "on" || (compactMode === "auto" && shownAccountCount() >= COMPACT_FROM);

// First launch: the widget, alerts and start-on-login are all off or out of
// sight by default, and most people never open Settings to find them.
function welcomeCard() {
  if (welcomeDismissed || !data) return "";
  const alertsOn = notificationSettings.codex || notificationSettings.claude;
  const rows = [
    ["widget", "▤", "Show meters on the taskbar", "Always visible, next to the clock", widgetSettings.enabled],
    ["alerts", "!", "Alert me before a limit runs out", "When a window is low, runs hot, or is used up", alertsOn],
  ];
  if (startOnLogin !== null)
    rows.push(["start", "↻", "Start with Windows", "Opens quietly in the tray", startOnLogin]);
  if (rows.every((r) => r[4])) return "";
  return `<div class="welcome">
    <div class="welcome-h">Set up AI Usage
      <button class="welcome-x" data-welcome-dismiss title="Dismiss" aria-label="Dismiss">✕</button></div>
    <p>Three things most people turn on:</p>
    ${rows
      .map(
        ([key, icon, title, sub, on]) => `<div class="setup-row">
          <span class="ic" aria-hidden="true">${icon}</span>
          <span class="tx">${esc(title)}<span>${esc(sub)}</span></span>
          ${
            on
              ? `<span class="setup-done">On</span>`
              : `<button class="btn teal" data-welcome="${key}">Turn on</button>`
          }</div>`
      )
      .join("")}
  </div>`;
}

// One provider card on the Overview. Split out of `renderOverview` because
// either can now be absent — a provider whose accounts are all hidden here
// draws nothing at all rather than an empty card.
function overviewCodexCard(cx) {
  let html = `<div class="ov-card" data-goto="codex" role="button" tabindex="0">
    <div class="ov-head">
      <div class="ov-name">Codex ${healthBadge(cx.health)}
        ${cx.plan_type ? `<span class="pill">${esc(cx.plan_type)}</span>` : ""}</div>
      <span class="ov-cost">${usd(cx.cost_today)} today</span>
    </div>`;
  if ((cx.quotas || []).length) {
    for (const quota of cx.quotas) {
      if (quota.group === "reserve") continue;
      html += gauge(quota.group === "session" ? "Session" : quota.label, quota.gauge, {
        key: codexKey(quota.id),
      });
    }
    // The overview is a glance, so the reserve only earns a line here once
    // it's the thing being spent.
    const reserve = codexReserve(cx);
    if (reserve?.drained) {
      html += gauge(reserve.primary.label, reserve.primary.gauge, {
        key: codexKey(reserve.primary.id),
      });
      html += `<div class="sec-sub">${esc(reserveStatus(reserve))}</div>`;
    }
  } else {
    html += gauge("Session", cx.primary, { key: codexKey("session") });
    html += gauge("Weekly", cx.secondary, { key: codexKey("weekly") });
  }
  if (!cx.available) html += `<div class="sec-sub">No data</div>`;
  const resetCount = availableResets(cx).length;
  if (resetCount)
    html += `<div class="ov-reset">${resetCount} reset credit${
      resetCount > 1 ? "s" : ""
    } available</div>`;
  return html + `</div>`;
}

function overviewClaudeCard(cl, accounts) {
  const multi = accounts.length > 1;
  let html = `<div class="ov-card" data-goto="claude" role="button" tabindex="0">
    <div class="ov-head">
      <div class="ov-name">Claude
        ${multi ? "" : healthBadge(accounts.length ? accounts[0].health : null)}</div>
      <span class="ov-cost">${usd(cl.cost_today)} today</span>
    </div>`;
  if (accounts.length) {
    for (const a of accounts) {
      if (multi)
        html += `<div class="ov-acct">${esc(a.label)}
          ${healthBadge(a.health)}${a.active ? ` <span class="pill">default</span>` : ""}</div>`;
      // Credits stay inside the account's own block: a combined bar appended
      // after the loop reads as belonging to the last account listed.
      if (a.live) html += claudeGauges(a, "Session") + creditsBar(a, { link: true });
      else html += `<div class="sec-sub">Live unavailable — open Claude Code</div>`;
    }
  } else {
    html += `<div class="sec-sub">Live unavailable — open Claude Code</div>`;
  }
  const resetCount = accounts.reduce((sum, a) => sum + claudeResetCount(a), 0);
  if (resetCount)
    html += `<div class="ov-reset claude">${resetCount} reset${
      resetCount > 1 ? "s" : ""
    } banked</div>`;
  return html + `</div>`;
}

function renderOverview() {
  const cx = data.codex,
    cl = data.claude;
  const codex = codexInPanel();
  const accounts = shownClaudeAccounts();
  const claude = claudeInPanel();
  // The headline follows the cards below it: a provider that isn't on this
  // screen shouldn't be quietly inside its total. It can only be split by
  // provider, not by account — the local logs the cost comes from carry no
  // account id — so hiding one of several Claude accounts leaves it whole.
  const today = (codex ? cx.cost_today : 0) + (claude ? cl.cost_today : 0);
  // "API value", not "today" alone: a bare dollar figure in the header reads as
  // a bill, which is the one thing it isn't.
  const valueTip =
    "API-equivalent value of today's usage: local token logs priced at API list prices. Not your subscription bill.";
  let html = header(
    "Overview",
    `<span class="pill" title="${esc(valueTip)}">${usd(today)} API value
      <span class="info-tip" tabindex="0" role="img" aria-label="${esc(valueTip)}">i</span></span>`
  );

  if (!codex && !claude) {
    return (
      html +
      `<div class="empty-state">Every account is hidden here.<br/>Turn one back on in Settings → Accounts.</div>`
    );
  }
  html += welcomeCard();
  const limits = panelLimits();
  html += tightCard(limits);
  if (useCompact() && limits.length) return html + compactOverview(limits);
  if (codex) html += overviewCodexCard(cx);
  if (claude) html += overviewClaudeCard(cl, accounts);
  return html;
}

// ---------- Credits screen ----------
const BILLING_URL = "https://claude.ai/settings/billing";
// All credit detail and config state lives here, opened from the one-line
// meters on the tabs, so the tabs themselves stay lean. The past-limits
// switch is read-only: it can only be changed in claude.ai billing settings.
function renderCredits() {
  const cl = data.claude;
  const cx = data.codex;
  const stamp = data?.generated_at ? clockTime(data.generated_at) : "—";
  let html = `<div class="hd">
    <button class="hd-back" id="creditsBack" title="Back" aria-label="Back">‹</button>
    <span class="hd-title">Credits</span>
    <span class="hd-right"><span title="Last updated">${esc(stamp)}</span></span>
  </div>`;

  const accounts = creditAccounts();
  const multi = shownClaudeAccounts().length > 1;
  if (accounts.length) {
    html += `<div class="grp-label">Claude</div>`;
    for (const a of accounts) {
      const extra = a.extra_usage;
      html += `<div class="acct">
        <div class="acct-head">
          <span class="acct-label">${esc(a.label)}</span>
          ${healthBadge(a.health)}
          ${a.active && multi ? `<span class="pill">default</span>` : ""}
        </div>`;
      if (creditsActive(a)) {
        const cap = extra.monthly_limit;
        html += creditsBar(a, { label: "This month" });
        html += `<div class="row"><span class="k">Spent</span>
          <span class="v">${usd(Math.min(cap, extra.used_credits || 0))}</span></div>`;
        html += `<div class="row"><span class="k">Monthly cap</span>
          <span class="v">${usd(cap)}</span></div>`;
      } else if (extra.is_enabled) {
        html += `<div class="sec-sub">Credits are on, but no monthly cap was reported.</div>`;
      }
      html += `<div class="row"><span class="k">Use credits past plan limits</span>
        <span class="v">${
          extra.is_enabled
            ? `<span class="pill on-pill">On</span>`
            : `<span class="pill">Off</span>`
        }</span></div>`;
      html += `</div>`;
    }
    html += `<div class="row-action">
      <span class="sec-sub">Caps and the past-limits switch live in Claude's billing settings.</span>
      <button class="row-btn" data-open-url="${BILLING_URL}">Open billing</button>
    </div>`;
  } else {
    html += `<div class="empty-state">No usage-credit info from Claude yet.<br/>It appears after a live refresh on plans with credits.</div>`;
  }

  if (codexInPanel() && typeof cx.credits === "number") {
    html += `<div class="grp-label">Codex</div>`;
    html += `<div class="row"><span class="k">Balance</span>
      <span class="v">${esc(cx.credits.toFixed(2))}</span></div>`;
    html += `<div class="sec-sub">Codex spends credits automatically when a limit is hit.</div>`;
  }

  return html;
}

// ---------- Quota windows (timeline) ----------
// Every bar is a window in time: its length is the window's span (start →
// reset), the fill is the quota spent inside it, and the notch marks now. Fill
// running ahead of the notch means the allowance is going faster than the
// window refills it — the reading a flat meter can't give.
//
// The range decides which windows can be drawn honestly. On the day axis a
// 5-hour window is a full-size bar and a chain of them shows the shape of the
// day; at week scale the same window is three pixels wide, so it drops to one
// tick per window instead.

const MINUTE = 60000;
const HOUR = 3600000;
const DAY = 86400000;
// `fit` sizes the range to the windows themselves. A seven-day column count
// can't hold a seven-day window *and* the day it started, so a literal week
// would clip every reset off the right edge; "Week" instead means "the windows
// you're currently in", which for weekly limits lands around 8–11 days.
const RANGES = {
  day: { label: "Today", days: 1, cols: 6 },
  week: { label: "Week", fit: true, min: 7, max: 12 },
  two: { label: "2 weeks", days: 14 },
};
/// Windows this short are "sessions": too brief to read at week scale.
const SESSION_MAX_MS = 12 * HOUR;

const clampPct = (n) => clamp(Number(n) || 0, 0, 100);
const pad2 = (n) => String(n).padStart(2, "0");
// 12-hour, matching the clock times the rest of the panel prints (clockTime).
const hhmm = (ms) =>
  new Date(ms)
    .toLocaleTimeString([], { hour: "numeric", minute: "2-digit", hour12: true })
    .toLowerCase();
// Axis ticks have no room for minutes that are always :00 — "8am", "12pm".
const hourTick = (ms) => {
  const hour = new Date(ms).getHours();
  return `${((hour + 11) % 12) + 1}${hour < 12 ? "am" : "pm"}`;
};
const mmdd = (ms) => `${pad2(new Date(ms).getMonth() + 1)}/${pad2(new Date(ms).getDate())}`;
const startOfDay = (ms) => new Date(ms).setHours(0, 0, 0, 0);
const sameDay = (a, b) => startOfDay(a) === startOfDay(b);
// Inside today the date adds nothing; past midnight it's the only thing that
// disambiguates "resets 9:18 am".
const whenText = (ms) => (sameDay(ms, Date.now()) ? hhmm(ms) : `${mmdd(ms)} ${hhmm(ms)}`);

function durText(ms) {
  const m = Math.max(0, Math.round(ms / MINUTE));
  const d = Math.floor(m / 1440);
  const h = Math.floor((m % 1440) / 60);
  if (d) return `${d}d ${h}h`;
  if (h) return `${h}h ${m % 60}m`;
  return `${m}m`;
}
const pctText = (n) => (n >= 99.95 ? "100%" : `${n < 10 ? n.toFixed(1) : n.toFixed(0)}%`);
// How long ago something was, for a sentence rather than a duration column:
// "45m", "1h", "1h 20m". durText keeps the zero minutes ("1h 0m"), which reads
// oddly mid-sentence.
function agoText(ms) {
  const m = Math.max(0, Math.round(ms / MINUTE));
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  return m % 60 ? `${h}h ${m % 60}m` : `${h}h`;
}
// "5h" / "7d" — the row's own key, derived from the length the provider reports
// rather than assumed.
function spanShort(span) {
  if (span < DAY) return `${Math.round(span / HOUR)}h`;
  return `${Math.round(span / DAY)}d`;
}

// Every live window in the current snapshot, keyed the same way the Rust
// recorder keys them so the two can be matched up.
function liveWindows() {
  const out = [];
  const add = (provider, account, accountLabel, id, label, group, gauge) => {
    if (!gauge || typeof gauge.resets_at !== "number") return;
    const span = (Number(gauge.window_minutes) || 0) * MINUTE;
    if (span <= 0) return;
    const end = gauge.resets_at * 1000;
    out.push({
      key: historyKey(provider, account, id),
      credential: `${provider}|${account}`,
      provider,
      accountLabel,
      label,
      group,
      span,
      start: end - span,
      end,
      used: clampPct(gauge.used_percent),
      live: true,
    });
  };

  const cx = data?.codex;
  if (cx?.live && codexInPanel()) {
    if ((cx.quotas || []).length) {
      for (const q of cx.quotas) add("codex", "", "Codex", q.id, q.label, q.group, q.gauge);
    } else {
      add("codex", "", "Codex", "session", "Session", "session", cx.primary);
      add("codex", "", "Codex", "weekly", "Weekly", "weekly", cx.secondary);
    }
  }
  for (const a of shownClaudeAccounts().filter((x) => x.live)) {
    if ((a.quotas || []).length) {
      for (const q of a.quotas) {
        // Scoped limits follow the same Display setting as the gauges.
        if ((q.scope_model || q.scope_surface) && !showModelWeekly) continue;
        add("claude", a.id, a.label, q.id, q.label, q.group, q.gauge);
      }
      continue;
    }
    add("claude", a.id, a.label, "session", "Session", "session", a.five_hour);
    add("claude", a.id, a.label, "weekly", "Weekly", "weekly", a.seven_day);
    if (a.seven_day_model && showModelWeekly) {
      const mg = a.seven_day_model;
      add("claude", a.id, a.label, "weekly_model", `Weekly (${mg.model})`, "weekly", mg.gauge);
    }
  }
  return out;
}

// Recorded instances of one window, newest last. Unix seconds on the wire.
function recordedInstances(key) {
  const series = (windowHistory?.series || []).find((s) => s.key === key);
  return (series?.instances || []).map((i) => ({
    start: i.start * 1000,
    end: i.end * 1000,
    used: clampPct(i.used),
    samples: (i.samples || []).map((s) => ({ at: s.at * 1000, used: clampPct(s.used) })),
  }));
}

// The multi-day ranges begin on the day the earliest window in view opened, so
// a weekly bar shows where it started as well as where it resets. Nothing is
// reserved for history the windows don't reach into. A fixed range then runs
// its full length from there — a fortnight holds a seven-day window and the
// dashed one after it — while a `fit` range stops at the last reset. The floor
// keeps today on the chart if a window opened further back than the range runs.
function axisFor(range, now, rows = []) {
  const spec = RANGES[range];
  const today = startOfDay(now);
  if (spec.days === 1) return { from: today, to: today + DAY, days: 1, cols: spec.cols };

  const earliest = rows.reduce((first, row) => Math.min(first, row.start), now);
  const furthest = rows.reduce((last, row) => Math.max(last, row.end), now);
  const longest = spec.days || spec.max;
  const from = Math.max(Math.min(startOfDay(earliest), today), today - (longest - 1) * DAY);
  const days = spec.days
    ? spec.days
    : clamp(Math.ceil((startOfDay(furthest) + DAY - from) / DAY), spec.min, spec.max);
  return { from, to: from + days * DAY, days, cols: days };
}
const axisPos = (axis, t) => ((t - axis.from) / (axis.to - axis.from)) * 100;
const axisClamp = (axis, t) => Math.min(100, Math.max(0, axisPos(axis, t)));

// The window occurrences visible in `axis`.
//
// What was recorded is drawn where it actually happened — rolling windows start
// when you first use them, so real occurrences don't sit on a neat grid. The
// leftover space is then filled from the grid, which is only a guess about
// where windows would fall: ahead of now that's the next reset, behind it it's
// a stretch the app wasn't running for.
function windowSlots(row, axis, now) {
  const span = row.span;
  const inRange = (start, end) => end > axis.from && start < axis.to;
  const slots = [];

  if (row.live && inRange(row.start, row.end)) {
    slots.push({
      start: row.start,
      end: row.end,
      used: row.used,
      state: now < row.end ? "live" : "done",
    });
  }
  for (const instance of recordedInstances(row.key)) {
    if (!inRange(instance.start, instance.end)) continue;
    // The newest instance is usually the live window seen a moment ago.
    if (slots.some((s) => s.start < instance.end && instance.start < s.end)) continue;
    slots.push({ start: instance.start, end: instance.end, used: instance.used, state: "done" });
  }

  const anchor = slots.length ? slots[slots.length - 1].end : row.live ? row.end : 0;
  if (anchor) {
    const first = Math.floor((axis.from - anchor) / span);
    const last = Math.ceil((axis.to - anchor) / span);
    for (let k = first; k <= last && slots.length < 80; k++) {
      const end = anchor + k * span;
      const start = end - span;
      if (!inRange(start, end)) continue;
      // Only the next couple of windows are worth sketching; past that the
      // dashes are just noise.
      if (start > now + 2 * span) continue;
      if (slots.some((s) => s.start < end && start < s.end)) continue;
      slots.push({ start, end, used: 0, state: start > now ? "ghost" : "unrecorded" });
    }
  }

  return slots.sort((a, b) => a.start - b.start);
}

// Spend per bucket, read off the recorded samples: what the percentage climbed
// by, hour to hour (day view) or day to day. A fall means the window rolled,
// not that quota came back, so only rises count.
function burnBuckets(row, axis) {
  const bucketMs = axis.days === 1 ? HOUR : DAY;
  const count = Math.round((axis.to - axis.from) / bucketMs);
  const samples = recordedInstances(row.key)
    .flatMap((i) => i.samples)
    .filter((s) => s.at >= axis.from - bucketMs && s.at <= axis.to)
    .sort((a, b) => a.at - b.at);
  if (samples.length < 3) return null;

  const buckets = new Array(count).fill(0);
  for (let i = 1; i < samples.length; i++) {
    const rise = samples[i].used - samples[i - 1].used;
    if (rise <= 0) continue;
    const index = Math.floor((samples[i].at - axis.from) / bucketMs);
    if (index >= 0 && index < count) buckets[index] += rise;
  }
  return buckets.some((v) => v > 0.01) ? buckets : null;
}

// Which of a credential's windows belong in a range. A five-hour window is 1.5%
// of a fortnight — unreadable at that scale — so the multi-day ranges carry the
// long windows only and Today keeps the short ones.
const rowsForRange = (credential, range) =>
  credential.rows.filter((row) => RANGES[range].days === 1 || row.span > SESSION_MAX_MS);

// Why the timeline has nothing to draw. "Sign in and refresh" is the wrong
// advice when the accounts are signed in and simply switched off here.
function timelineEmptyText() {
  if (timelineCredentials().length) return "Only short windows are live — see Today.";
  if (!codexInPanel() && !claudeInPanel())
    return "Every account is hidden here.<br/>Turn one back on in Settings → Accounts.";
  return "No live quota windows.<br/>Sign in to Codex or Claude Code and refresh.";
}

// One credential per Codex install / Claude account, with its windows split
// into the short ones and the long ones.
function timelineCredentials() {
  const groups = new Map();
  for (const w of liveWindows()) {
    if (!groups.has(w.credential)) {
      groups.set(w.credential, {
        id: w.credential,
        provider: w.provider,
        label: w.accountLabel,
        rows: [],
      });
    }
    groups.get(w.credential).rows.push(w);
  }
  for (const credential of groups.values()) {
    credential.rows.sort((a, b) => a.span - b.span);
    credential.next = credential.rows.reduce(
      (soonest, row) => (soonest === null || row.end < soonest ? row.end : soonest),
      null
    );
  }
  return [...groups.values()];
}

function pace(slot, now) {
  const elapsed = ((now - slot.start) / (slot.end - slot.start)) * 100;
  const delta = slot.used - elapsed;
  if (slot.used >= 99.95) return { cls: "hot", text: "exhausted", elapsed };
  if (delta > 20) return { cls: "hot", text: "burning fast", elapsed };
  if (delta > 8) return { cls: "warm", text: "ahead of pace", elapsed };
  if (delta < -8) return { cls: "", text: "under pace", elapsed };
  return { cls: "", text: "on pace", elapsed };
}

const barClass = (row, slot) =>
  [
    row.provider === "claude" ? "claude" : "codex",
    slot.used >= 80 ? "warn" : "",
    slot.state === "done" ? "done" : "",
  ]
    .filter(Boolean)
    .join(" ");

function slotTitle(row, slot, now) {
  if (slot.state === "unrecorded")
    return `${row.label} · ${whenText(slot.start)} → ${whenText(slot.end)} · not recorded`;
  if (slot.state === "ghost")
    return `next ${row.label.toLowerCase()} window · ${whenText(slot.start)} → ${whenText(slot.end)}`;
  const p = pace(slot, now);
  const tail = slot.state === "live" ? ` · ${p.text}` : " · final";
  return `${row.label} · ${whenText(slot.start)} → ${whenText(slot.end)} · ${pctText(
    slot.used
  )} used${tail}`;
}

// ---------- compact timeline (the popover) ----------
function windowRangeControl() {
  return `<div class="tsegs">${Object.entries(RANGES)
    .map(
      ([key, r]) =>
        `<button data-window-range="${key}" class="${key === windowRange ? "on" : ""}">${esc(
          r.label
        )}</button>`
    )
    .join("")}</div>`;
}

function compactAxis(axis) {
  const today = startOfDay(Date.now());
  let out = "";
  for (let i = 0; i < axis.cols; i++) {
    const t = axis.from + i * DAY;
    const label =
      axis.days === 1
        ? hourTick(axis.from + (i * DAY) / axis.cols)
        : axis.cols > 7 && i % 2
          ? ""
          : mmdd(t);
    const on = axis.days > 1 && t === today ? " class=\"on\"" : "";
    out += `<span${on}>${esc(label)}</span>`;
  }
  return `<div class="tw-axis">${out}</div>`;
}

// Dim what's finished, bright what's running, dashed what's next. Stretches the
// app wasn't watching are marked on the day view, where the gap is small and
// tells you something; across a fortnight they'd be a wall of dashes, so the
// multi-day ranges just leave that space empty.
function compactChain(row, axis, now) {
  let html = "";
  for (const slot of windowSlots(row, axis, now)) {
    if (slot.state === "unrecorded" && axis.days > 1) continue;
    const left = axisClamp(axis, slot.start);
    const width = Math.max(1, axisClamp(axis, slot.end) - left);
    const title = esc(slotTitle(row, slot, now));
    if (slot.state === "ghost" || slot.state === "unrecorded") {
      html += `<div class="tw-ghost ${slot.state}" style="left:${left}%;width:${width}%"
        title="${title}"></div>`;
      continue;
    }
    const label = width > 13 ? `<span class="n">${pctText(slot.used)}</span>` : "";
    html += `<div class="tw-seg ${barClass(row, slot)}" style="left:${left}%;width:${width}%"
      title="${title}">
      <div class="f" style="width:${slot.used}%"></div>${label}</div>`;
  }
  return html;
}

function compactBurn(row, axis, buckets) {
  const max = Math.max(...buckets, 0.5);
  const width = 100 / buckets.length;
  return buckets
    .map((v, i) =>
      v <= 0.01
        ? ""
        : `<div class="tw-burn ${row.provider === "claude" ? "claude" : "codex"}"
             style="left:${i * width + width * 0.12}%;width:${width * 0.76}%;height:${
               3 + (v / max) * 15
             }px"
             title="${esc(
               `${axis.days === 1 ? hhmm(axis.from + i * HOUR) : mmdd(axis.from + i * DAY)} · ${v.toFixed(
                 1
               )}% of ${row.label.toLowerCase()}`
             )}"></div>`
    )
    .join("");
}

function compactRow(row, axis, now, colPct) {
  const nowMark = `<div class="tw-now" style="left:${axisClamp(axis, now)}%"></div>`;
  const short = spanShort(row.span);
  const session = row.span <= SESSION_MAX_MS;

  // Day view, long window: it runs off both edges of a single day, so the row
  // shows what was actually spent hour by hour and keeps the total in a chip.
  if (axis.days === 1 && !session) {
    const buckets = burnBuckets(row, axis);
    const chip = `<span class="tw-chip"><b>${pctText(row.used)}</b> · ${esc(
      durText(row.end - now)
    )}</span>`;
    const body = buckets
      ? compactBurn(row, axis, buckets)
      : `<div class="tw-band" title="${esc(
          `${row.label} · ${whenText(row.start)} → ${whenText(row.end)}`
        )}"><span class="tw-band-txt">window continues</span></div>`;
    return `<div class="tw-row"><span class="k">${esc(short)}</span>
      <div class="tw-plot tall" style="--col:${colPct}%">${body}${nowMark}${chip}</div></div>`;
  }

  return `<div class="tw-row"><span class="k">${esc(short)}</span>
    <div class="tw-plot" style="--col:${colPct}%">${compactChain(
      row,
      axis,
      now
    )}${nowMark}</div></div>`;
}

function renderWindows() {
  const now = Date.now();
  const credentials = timelineCredentials()
    .map((credential) => ({ ...credential, visible: rowsForRange(credential, windowRange) }))
    .filter((credential) => credential.visible.length);
  const axis = axisFor(windowRange, now, credentials.flatMap((c) => c.visible));
  const colPct = 100 / axis.cols;

  const expand = `<button class="hd-expand" id="windowExpand"
    title="Expand" aria-label="Expand">${expandIcon()}</button>`;
  let html = header("Quota windows", "", { rightExtra: expand });
  html += windowRangeControl();

  if (!credentials.length) {
    html += `<div class="empty-state">${timelineEmptyText()}</div>`;
    return html;
  }

  html += compactAxis(axis);
  for (const credential of credentials) {
    html += `<div class="tw-cred">
      <span class="dot${credential.provider === "claude" ? " claude" : ""}"></span>
      <span class="nm">${esc(credential.label)}</span>
      <span class="rt">next ${esc(durText(credential.next - now))}</span>
    </div>`;
    for (const row of credential.visible) html += compactRow(row, axis, now, colPct);
  }
  html += `<div class="sec-sub">${
    axis.days === 1
      ? "Each bar is one window — dim is finished, bright is live, dashed is next."
      : "Bars run to their reset; dashed is the window after it. Short windows are on Today."
  }</div>`;
  html += historyNote(now);
  return html;
}

// Says how far back the record can speak for, so an empty stretch reads as
// "wasn't watching" rather than "nothing happened".
function historyNote(now) {
  const since = windowHistory?.recording_since;
  if (!since) {
    return `<div class="sec-sub">History starts now — past windows fill in as the app runs.</div>`;
  }
  const age = now - since * 1000;
  if (age > 3 * DAY) return "";
  return `<div class="sec-sub">Recording since ${esc(
    sameDay(since * 1000, now) ? hhmm(since * 1000) : mmdd(since * 1000)
  )}; anything earlier is drawn hollow.</div>`;
}

// ---------- expanded timeline ----------
function wideAxisHead(axis) {
  let out = "";
  if (axis.days === 1) {
    const step = 24 / 12;
    for (let i = 0; i < 12; i++) {
      const t = axis.from + i * step * HOUR;
      const on = Date.now() >= t && Date.now() < t + step * HOUR;
      out += `<div class="col${on ? " on" : ""}"><div class="c1">&nbsp;</div>
        <div class="c2">${esc(hourTick(t))}</div></div>`;
    }
    return out;
  }
  const today = startOfDay(Date.now());
  for (let i = 0; i < axis.days; i++) {
    const t = axis.from + i * DAY;
    out += `<div class="col${t === today ? " on" : ""}">
      <div class="c1">${esc(new Date(t).toLocaleDateString([], { weekday: "short" }))}</div>
      <div class="c2">${esc(mmdd(t))}</div></div>`;
  }
  return out;
}

function wideBars(row, axis, now, top, height) {
  let html = "";
  for (const slot of windowSlots(row, axis, now)) {
    if (slot.state === "unrecorded" && axis.days > 1) continue;
    const left = axisClamp(axis, slot.start);
    const width = Math.max(0.6, axisClamp(axis, slot.end) - left);
    const clipL = axisPos(axis, slot.start) < -0.01;
    const clipR = axisPos(axis, slot.end) > 100.01;
    const edges = `${clipL ? " clipL" : ""}${clipR ? " clipR" : ""}`;
    const chevrons = `${clipL ? `<span class="chev l">‹</span>` : ""}${
      clipR ? `<span class="chev r">›</span>` : ""
    }`;
    const title = esc(slotTitle(row, slot, now));
    const style = `left:${left}%;width:${width}%;top:${top}px;height:${height}px`;

    if (slot.state === "ghost" || slot.state === "unrecorded") {
      html += `<div class="ghost ${slot.state}${edges}" style="${style}" title="${title}">${
        width > 11 ? esc(slot.state === "ghost" ? whenText(slot.end) : "not recorded") : ""
      }</div>`;
      continue;
    }
    const label =
      width > 5
        ? `<div class="bt"${clipL ? ` style="padding-left:20px"` : ""}><b>${pctText(
            slot.used
          )}</b>&nbsp;· ${esc(slot.state === "live" ? whenText(slot.end) : "spent")}</div>`
        : "";
    const notch =
      slot.state === "live"
        ? `<div class="notch" style="left:${pace(slot, now).elapsed}%"></div>`
        : "";
    html += `<div class="bar ${barClass(row, slot)}${edges}" style="${style}" title="${title}">
      <div class="bf" style="width:${slot.used}%"></div>${notch}${chevrons}${label}</div>`;
  }
  return html;
}

function wideBurn(row, axis, buckets) {
  const max = Math.max(...buckets, 0.5);
  const width = 100 / buckets.length;
  return buckets
    .map((v, i) =>
      v <= 0.01
        ? ""
        : `<div class="burn ${row.provider === "claude" ? "claude" : "codex"}" style="left:${
            i * width + width * 0.12
          }%;width:${width * 0.76}%;height:${3 + (v / max) * 27}px" title="${esc(
            `${axis.days === 1 ? hhmm(axis.from + i * HOUR) : mmdd(axis.from + i * DAY)} · ${v.toFixed(
              1
            )}% of ${row.label.toLowerCase()}`
          )}"></div>`
    )
    .join("");
}

function wideRow(credential, row, axis, now, first) {
  const nowMark = `<div class="nowline" style="left:${axisClamp(axis, now)}%"></div>`;
  const session = row.span <= SESSION_MAX_MS;
  const p = pace(row, now);
  const head = first
    ? `<div class="cred-line">
        <span class="dot${credential.provider === "claude" ? " claude" : ""}"></span>
        <span class="nm">${esc(credential.label)}</span>
        <span class="pill tiny ${p.cls}">${esc(p.text)}</span>
      </div>
      <div class="cred-sub">${esc(row.label)} <b>${pctText(row.used)}</b> · resets in ${esc(
        durText(row.end - now)
      )}</div>`
    : `<div class="kind">${esc(row.label)} · ${esc(spanShort(row.span))}</div>
       <div class="kmeta">now <b>${pctText(row.used)}</b> · resets ${esc(whenText(row.end))}</div>`;

  let body;
  let height = 62;
  if (axis.days === 1 && !session) {
    const buckets = burnBuckets(row, axis);
    height = 58;
    body = buckets
      ? `<div class="dens-base"></div>${wideBurn(row, axis, buckets)}`
      : wideBars(row, axis, now, 14, 30);
  } else {
    body = wideBars(row, axis, now, 16, 30);
  }

  return `<div class="grow${first ? " first" : " sub"}">
    <div class="cred-col">${head}</div>
    <div class="plot" style="--col:${100 / axis.cols}%;height:${height}px">${body}${nowMark}</div>
  </div>`;
}

function renderWindowsWide() {
  const now = Date.now();
  const credentials = timelineCredentials()
    .map((credential) => ({ ...credential, visible: rowsForRange(credential, windowRange) }))
    .filter((credential) => credential.visible.length);
  const axis = axisFor(windowRange, now, credentials.flatMap((c) => c.visible));

  const segs = Object.entries(RANGES)
    .map(
      ([key, r]) =>
        `<button data-window-range="${key}" class="${key === windowRange ? "on" : ""}">${esc(
          r.label
        )}</button>`
    )
    .join("");
  const sub =
    axis.days === 1
      ? `${mmdd(axis.from)} · 24 hours · short windows full size`
      : `${mmdd(axis.from)} – ${mmdd(axis.to - DAY)} · ${axis.days} days · long windows`;

  const shrink = `<button class="hd-expand" id="windowCollapse"
    title="Shrink" aria-label="Shrink">${shrinkIcon()}</button>`;
  let html = `<div class="wide-hd">
    <div>
      <h2>Quota windows</h2>
      <div class="wide-sub">${esc(sub)} · now ${esc(hhmm(now))}</div>
    </div>
    <div class="segs">${segs}</div>
    ${headerControls({ rightExtra: shrink })}
  </div>`;

  if (!credentials.length) {
    return html + `<div class="empty-state">${timelineEmptyText()}</div>`;
  }

  let rows = "";
  for (const credential of credentials) {
    credential.visible.forEach((row, index) => {
      rows += wideRow(credential, row, axis, now, index === 0);
    });
  }
  html += `<div class="gantt">
    <div class="gantt-hd">
      <div class="cred-col">CREDENTIAL</div>
      <div class="cols">${wideAxisHead(axis)}</div>
    </div>
    ${rows}
  </div>`;
  html += `<div class="wide-legend">
    <i><span class="key used"></span> quota spent</i>
    <i><span class="key span"></span> window span (start → reset)</i>
    <i><span class="key next"></span> next window, or a stretch not recorded</i>
    <i>┆ notch = now inside the window · │ line = now on the axis · ‹ › = continues past the range</i>
  </div>`;
  return html;
}

// Settings → Data sources. Unlike the log indexes this record can't be
// rebuilt, so the row says how much of it exists before offering to bin it.
function windowHistoryRow() {
  const series = windowHistory?.series || [];
  const recorded = series.reduce((total, s) => total + (s.instances || []).length, 0);
  const since = windowHistory?.recording_since;
  const detail = recorded
    ? `${recorded} window${recorded === 1 ? "" : "s"} across ${series.length} limit${
        series.length === 1 ? "" : "s"
      }`
    : "Nothing recorded yet";
  const sub = since ? `${detail} · since ${fmtDate(since)}` : detail;
  return `<div class="grp-row static-row">
    <span class="rlab">Quota window history<span class="rsub">${esc(sub)}</span></span>
    <button class="row-btn" data-window-history-clear>Clear</button>
  </div>`;
}

async function loadWindowHistory() {
  try {
    windowHistory = await invoke("get_window_history");
  } catch (_) {
    // The screen still works from live gauges alone.
  }
  if (activeTab === "windows") render();
}

// Kept separate from the history above: this is a handful of numbers the meters
// need on every tab, where the full record is only the timeline's business.
async function loadRecentBurn(repaint = false) {
  if (!showRecentBurn) return;
  try {
    recentBurns = await invoke("get_recent_burn", { minutes: recentMinutes() });
  } catch (_) {
    // The meters simply go without their band.
    return;
  }
  if (repaint) render();
}

async function setPanelExpanded(expanded) {
  if (panelExpanded === expanded) return;
  panelExpanded = expanded;
  try {
    await invoke("set_panel_expanded", { expanded });
  } catch (_) {
    // Falls back to the compact layout in a compact window — still usable.
    panelExpanded = false;
  }
  render();
}

// ---------- settings ----------
// One reach, both surfaces — which is why it sits under Display rather than
// being repeated in the widget's own group, and why it is worth saying out loud
// that turning it here moves the widget's band too.
function recentRangeRow() {
  const sub = !showRecentBurn
    ? "The stretch the widget's highlight covers"
    : widgetSettings.show_recent
      ? "The stretch the highlight covers, here and on the widget"
      : "The stretch the highlight covers";
  return `<div class="grp-row col">
    <span class="rlab">How far back<span class="rsub">${esc(sub)}</span></span>
    <div class="choices">${RECENT_CHOICES.map(
      (choice) =>
        `<button class="choice${
          choice.minutes === recentMinutes() ? " on" : ""
        }" data-recent-minutes="${choice.minutes}">${esc(choice.label)}</button>`
    ).join("")}</div>
  </div>`;
}

function settingRow(label, sub, attrs, enabled, claude) {
  return `<label class="grp-row">
    <span class="rlab">${esc(label)}${sub ? `<span class="rsub">${esc(sub)}</span>` : ""}</span>
    <input class="sw${claude ? " claude" : ""}" type="checkbox" ${attrs} ${
      enabled ? "checked" : ""
    } />
  </label>`;
}

function renameField() {
  return `<input class="acct-input" data-rename-input type="text" maxlength="40"
      value="${esc(editingDraft)}" placeholder="Account name" />
    <div class="acct-actions">
      <button class="acct-btn save" data-rename-save="${esc(editingAccount)}" title="Save">✓</button>
      <button class="acct-btn" data-rename-cancel title="Cancel">✕</button>
    </div>`;
}

// "Add Claude folder": a button that expands into a path input. Lets a user
// track a second login kept in a separate CLAUDE_CONFIG_DIR folder.
function addFolderControl() {
  if (!addingFolder)
    return `<button class="ghost-btn" data-add-folder-open>+ Add Claude folder</button>`;
  const err = addFolderError ? `<div class="banner small">${esc(addFolderError)}</div>` : "";
  return `<div class="add-folder">
      <input class="acct-input" data-add-folder-input type="text"
        value="${esc(addFolderDraft)}" placeholder="Path to a Claude config folder" />
      <div class="acct-actions">
        <button class="acct-btn save" data-add-folder-save title="Add">✓</button>
        <button class="acct-btn" data-add-folder-cancel title="Cancel">✕</button>
      </div>
    </div>
    <div class="sec-sub">A folder containing its own <code>.credentials.json</code> (e.g. a second <code>CLAUDE_CONFIG_DIR</code>).</div>
    ${err}`;
}

// One expandable diagnostic row: the detail that used to sit inline on every
// data tab.
function diagRow(key, label, health, estimate = null, localIsNormal = false) {
  if (!health) return "";
  const expanded = expandedHealth.has(key);
  const ok = healthOk(health, localIsNormal);
  const error = health.error_message
    ? `<div><span>Last error</span><strong>${esc(health.error_message)}</strong></div>`
    : "";
  const fetched = health.fetched_at
    ? `<div><span>Last successful update</span><strong>${esc(
        clockTime(health.fetched_at)
      )}</strong></div>`
    : "";
  const attempted = health.attempted_at
    ? `<div><span>Last attempt</span><strong>${esc(clockTime(health.attempted_at))}</strong></div>`
    : "";
  const files =
    health.files_scanned || health.files_cached || health.files_skipped
      ? `<div><span>Files</span><strong>${health.files_scanned || 0} scanned · ${
          health.files_cached || 0
        } cached · ${health.files_skipped || 0} skipped</strong></div>`
      : "";
  const pricing = estimate?.pricing_reviewed_at
    ? `<div><span>Pricing catalog</span><strong>${esc(estimate.catalog_version)} · reviewed ${esc(
        estimate.pricing_reviewed_at
      )}</strong></div>`
    : "";
  const diagnostic = esc(JSON.stringify({ health, estimate }, null, 2));
  const clearCache = key.endsWith("history")
    ? `<button class="health-copy" data-history-cache-clear>Clear scan cache</button>`
    : "";
  return `<div class="diag">
    <button class="diag-row" data-health-toggle="${esc(key)}" aria-expanded="${expanded}">
      <span class="status-dot ${ok ? "" : "warn"}"></span>
      <span class="diag-name">${esc(label)}</span>
      <span class="diag-src">${esc(healthTitle(health))}</span>
      <span class="diag-chevron">⌄</span>
    </button>
    ${
      expanded
        ? `<div class="health-detail">${fetched}${attempted}${error}${files}${pricing}
      <button class="health-copy" data-health-copy="${diagnostic}">Copy diagnostics</button>${clearCache}
    </div>`
        : ""
    }
  </div>`;
}

// One account's two visibility switches, as a pair of chips: which surfaces
// draw it. Chips rather than the `.sw` toggles the rest of Settings uses —
// two switches on one row read as two unrelated settings, where a labelled
// pair reads as the one choice it is.
function visibilityChips(id) {
  const surfaces = [
    ["panel", "App"],
    ["widget", "Widget"],
  ];
  return `<div class="acct-vis">
    <span class="vis-lab">Show in</span>
    <div class="choices">${surfaces
      .map(([surface, label]) => {
        const on = accountShown(accountVisibility, surface, id);
        return `<button class="choice${on ? " on" : ""}" role="switch"
          aria-checked="${on}" data-vis-surface="${surface}" data-vis-id="${esc(id)}"
          title="${on ? "Hide from" : "Show in"} the ${label.toLowerCase()}">${esc(label)}</button>`;
      })
      .join("")}</div>
  </div>`;
}

function codexAccountRow() {
  return `<div class="acct-row col">
    <div class="acct-line">
      <span class="rlab"><span class="nm">Codex</span>${
        data.codex.plan_type ? ` <span class="pill">${esc(data.codex.plan_type)}</span>` : ""
      }<span class="pth">~/.codex</span></span>
    </div>
    ${visibilityChips(CODEX_ACCOUNT)}
  </div>`;
}

function claudeAccountRow(a) {
  // Removing forgets the folder, so it takes the same inline two-step confirm
  // as spending a reset rather than going on a single click.
  const confirming = confirmingRemove === a.id;
  const actions = confirming
    ? `<span class="acts">
        <span class="confirm-q">Remove folder?</span>
        <button class="acct-btn danger" data-remove-confirm="${esc(a.id)}" title="Remove"
          aria-label="Remove ${esc(a.label)}">✓</button>
        <button class="acct-btn" data-remove-cancel title="Keep it" aria-label="Keep it">✕</button>
      </span>`
    : `<span class="acts">
        <button class="acct-btn" data-rename="${esc(a.id)}" title="Rename"
          aria-label="Rename ${esc(a.label)}">✎</button>
        ${
          a.removable
            ? `<button class="acct-btn" data-remove="${esc(a.id)}" title="Remove this folder"
                aria-label="Remove ${esc(a.label)}">✕</button>`
            : ""
        }
      </span>`;
  return `<div class="acct-row col${confirming ? " confirming" : ""}">
    <div class="acct-line">
      <span class="rlab"><span class="nm">${esc(a.label)}</span>${
        a.active ? ` <span class="pill">default</span>` : ""
      }${a.id ? `<span class="pth">${esc(a.id)}</span>` : ""}</span>
      ${a.id ? actions : ""}
    </div>
    ${
      confirming
        ? `<div class="sec-sub">Stops tracking it here. Its credentials and logs stay on disk.</div>`
        : visibilityChips(claudeAccount(a.id))
    }
  </div>`;
}

// Settings → Taskbar widget → Layout. One picker per account the widget is
// drawing, over a preview of where they land.
//
// Only offered from two accounts up: with one there is a single full-width bar
// whatever you pick, so the control would be a choice with one outcome.
function widgetLayoutRows() {
  const list = widgetAccounts();
  if (list.length < 2) return "";
  const choices = [
    ["auto", "Auto"],
    ["top", "Top"],
    ["bottom", "Bottom"],
  ];
  let html = `<div class="grp-row col">
    <span class="rlab">Layout<span class="rsub">Which row each account sits on — alone on a row is full width</span></span>
    ${widgetLayoutPreview(list)}
  </div>`;
  for (const a of list) {
    const current = widgetRowOf(a.id);
    html += `<div class="grp-row wlay">
      <span class="rlab"><span class="wdot ${
        a.provider === "claude" ? "claude" : ""
      }"></span>${esc(a.label)}</span>
      <div class="choices">${choices
        .map(
          ([value, label]) =>
            `<button class="choice${
              value === current ? " on" : ""
            }" data-widget-row="${esc(a.id)}" data-widget-row-value="${value}">${esc(
              label
            )}</button>`
        )
        .join("")}</div>
    </div>`;
  }
  return html;
}

// A miniature of the widget's own arrangement, built from the same split rule
// the widget uses (`widgetRows`), so it can't claim a layout the bar won't draw.
function widgetLayoutPreview(list) {
  const rows = widgetRows(list, widgetSettings.account_rows);
  return `<div class="wprev">${rows
    .map(
      (row) =>
        `<div class="wprev-row">${row
          .map(
            (a) =>
              `<span class="wprev-cell${
                a.provider === "claude" ? " claude" : ""
              }" title="${esc(a.label)}">${esc(a.label)}</span>`
          )
          .join("")}</div>`
    )
    .join("")}</div>`;
}

// Settings → Alerts: which levels warn, whether "used up" and "will run out
// early" are said at all, and when to keep quiet. Each change saves the whole
// set, so the backend never holds half of an edit.
const WARN_CHOICES = [50, 75, 90, 95];

function minutesToTime(minutes) {
  return `${pad2(Math.floor(minutes / 60))}:${pad2(minutes % 60)}`;
}

function alertOptionsRows() {
  const n = notificationSettings;
  const levels = new Set(n.warn_levels || []);
  const chip = (attrs, label, on) =>
    `<button class="choice${on ? " on" : ""}" role="switch" aria-checked="${on}" ${attrs}>${esc(
      label
    )}</button>`;
  const warn = WARN_CHOICES.map((level) =>
    chip(`data-warn-level="${level}"`, `${level}%`, levels.has(level))
  ).join("");
  return `<div class="grp-row col">
      <span class="rlab">Warn at<span class="rsub">Each level fires once per window</span></span>
      <div class="choices">${warn}
        ${chip(`data-alert-flag="used_up"`, "Used up", n.used_up)}
        ${chip(`data-alert-flag="forecast"`, "Will run out early", n.forecast)}</div>
    </div>
    ${settingRow(
      "Quiet hours",
      n.quiet_hours
        ? `No alerts ${minutesToTime(n.quiet_start)} – ${minutesToTime(n.quiet_end)}`
        : "Hold alerts overnight",
      "data-alert-flag-switch=\"quiet_hours\"",
      n.quiet_hours,
      true
    )}
    ${
      n.quiet_hours
        ? `<div class="grp-row static-row">
            <span class="rlab">From</span>
            <span class="times">
              <input class="time-input" type="time" data-quiet="quiet_start" value="${minutesToTime(
                n.quiet_start
              )}" aria-label="Quiet hours start" />
              <span class="rsub">to</span>
              <input class="time-input" type="time" data-quiet="quiet_end" value="${minutesToTime(
                n.quiet_end
              )}" aria-label="Quiet hours end" />
            </span>
          </div>`
        : ""
    }`;
}

// Settings → Display → Tray icon: what the ring in the notification area
// follows. An account choice falls back to the tightest window when that
// account has nothing live.
function trayIconRow() {
  const options = [
    ["tightest", "Tightest limit"],
    ["logo", "Plain logo"],
  ];
  if (codexInPanel() && data?.codex?.available) options.push([CODEX_ACCOUNT, "Codex"]);
  for (const a of shownClaudeAccounts()) options.push([claudeAccount(a.id), a.label || "Claude"]);
  return `<div class="grp-row col">
    <span class="rlab">Tray icon<span class="rsub">A ring for how much is left, amber when low, red when it will run out</span></span>
    <div class="choices">${options
      .map(
        ([value, label]) =>
          `<button class="choice${trayIconMode === value ? " on" : ""}" data-tray-icon="${esc(
            value
          )}">${esc(label)}</button>`
      )
      .join("")}</div>
  </div>`;
}

function compactModeRow() {
  const options = [
    ["auto", `Auto (${COMPACT_FROM}+ accounts)`],
    ["on", "Always"],
    ["off", "Never"],
  ];
  return `<div class="grp-row col">
    <span class="rlab">Compact Overview<span class="rsub">One row per account instead of full cards</span></span>
    <div class="choices">${options
      .map(
        ([value, label]) =>
          `<button class="choice${compactMode === value ? " on" : ""}" data-compact="${value}">${esc(
            label
          )}</button>`
      )
      .join("")}</div>
  </div>`;
}

function settingsNav() {
  return `<div class="secnav" role="tablist" aria-label="Settings sections">${SETTINGS_SECTIONS.map(
    ([key, label]) =>
      `<button role="tab" aria-selected="${key === settingsSection}" class="${
        key === settingsSection ? "on" : ""
      }" data-settings-section="${key}">${esc(label)}</button>`
  ).join("")}</div>`;
}

function settingsAlerts() {
  let html = `<div class="grp-label">Limits</div><div class="grp">`;
  html += settingRow(
    "Codex limits",
    "Warnings, run-outs and resets",
    'data-notify-provider="codex"',
    notificationSettings.codex,
    false
  );
  html += settingRow(
    "Claude limits",
    "Warnings, run-outs and resets",
    'data-notify-provider="claude"',
    notificationSettings.claude,
    true
  );
  html += alertOptionsRows();
  html += `</div>`;
  if (notifyOptionsError)
    html += `<div class="banner small">Couldn't save the alert options — try again.</div>`;

  html += `<div class="grp-label">Resets</div><div class="grp">`;
  html += settingRow(
    "Codex reset credits",
    "When a free reset is granted",
    'data-notify-provider="codex_resets"',
    notificationSettings.codex_resets,
    false
  );
  html += settingRow(
    "Claude resets",
    "When a reset is banked or about to expire",
    'data-notify-provider="claude_resets"',
    notificationSettings.claude_resets,
    true
  );
  html += `</div>`;
  if (notifyError)
    html += `<div class="banner small">Couldn't save the notification setting — try again.</div>`;
  return html;
}

function settingsDisplay() {
  const scopes = scopedLimitNames(shownClaudeAccounts());
  let html = `<div class="grp-label">Meters</div><div class="grp">`;
  html += settingRow(
    "Meters fill as you use",
    meterFillsUp
      ? "Bars start empty and grow with usage"
      : "Bars start full and drain as you use",
    "data-meter-fill",
    meterFillsUp,
    true
  );
  html += settingRow(
    "Pace line",
    "Mark how far through each window the clock is",
    "data-meter-pace",
    meterPaceLine,
    true
  );
  html += settingRow(
    "Recent usage",
    "Highlight what each window burned lately",
    "data-meter-recent",
    showRecentBurn,
    true
  );
  if (showRecentBurn || widgetSettings.show_recent) html += recentRangeRow();
  if (scopes.length) {
    html += settingRow(
      "Scoped limits",
      `Show the ${scopes.join(" / ")} gauge`,
      "data-model-weekly",
      showModelWeekly,
      true
    );
  }
  html += `</div>`;
  if (showRecentBurn)
    html += `<div class="sec-sub">Read from the recorded window history, so it only covers time this app was running — the highlight's tooltip says how far back it can actually speak for.</div>`;
  html += `<div class="grp-label">Layout</div><div class="grp">${compactModeRow()}${trayIconRow()}</div>`;
  return html;
}

function settingsWidget() {
  let html = `<div class="grp-label">Taskbar widget</div><div class="grp">`;
  html += settingRow(
    "Show on the taskbar",
    "An always-on-top strip of per-account meters",
    "data-widget-enabled",
    widgetSettings.enabled,
    true
  );
  if (widgetSettings.enabled) {
    html += settingRow(
      "Pace marks",
      "Mark how far through each window the clock is",
      "data-widget-option=\"show_pace\"",
      widgetSettings.show_pace !== false,
      true
    );
    html += settingRow(
      "Weekly bar",
      "A dim second bar under each session bar",
      "data-widget-option=\"show_weekly\"",
      widgetSettings.show_weekly !== false,
      true
    );
    html += settingRow(
      "Recent usage",
      "Highlight what each session bar burned lately",
      "data-widget-option=\"show_recent\"",
      widgetSettings.show_recent === true,
      true
    );
    html += widgetLayoutRows();
    // Only worth a row when there is a choice to make. The drag is confined to
    // one taskbar, so this is the only way across screens.
    if (widgetMonitors.length > 1) {
      html += `<div class="grp-row col">
        <span class="rlab">Display<span class="rsub">Which taskbar it sits on</span></span>
        <div class="choices">${widgetMonitors
          .map(
            (m) =>
              `<button class="choice${m.selected ? " on" : ""}" data-widget-monitor="${esc(
                m.name
              )}"${m.usable ? "" : " disabled title=\"No taskbar on this display\""}>${esc(
                m.label
              )}</button>`
          )
          .join("")}</div>
      </div>`;
    }
    html += `<div class="grp-row">
      <span class="rlab">Width<span class="rsub">Or drag either side edge</span></span>
      <span class="stepper">
        <button class="acct-btn" data-widget-width="-10" title="Narrower" aria-label="Narrower">−</button>
        <b class="stepval">${Math.round(widgetSettings.width || 114)} px</b>
        <button class="acct-btn" data-widget-width="10" title="Wider" aria-label="Wider">+</button>
      </span>
    </div>`;
    html += `<div class="grp-row">
      <span class="rlab">Position and size<span class="rsub">Drag it along the taskbar to move it</span></span>
      <button class="ghost-btn" data-widget-reset>Reset</button>
    </div>`;
  }
  html += `</div>`;
  const placementNote = {
    vertical_taskbar:
      "Your taskbar is docked to a side. The widget lays its accounts out in two rows across a wide strip, so it stays hidden until the taskbar is along the top or bottom.",
    no_taskbar:
      "No taskbar found on this screen — auto-hide is on, or the bar is on another display. The widget stays hidden rather than floating over whatever is underneath.",
  }[widgetPlacement];
  if (widgetSettings.enabled && placementNote) {
    html += `<div class="banner small">${esc(placementNote)}</div>`;
  }
  if (widgetLoadError) {
    html += `<div class="banner small">Couldn't read the widget settings — ${esc(
      widgetLoadError
    )}</div>`;
  }
  html += `<div class="sec-sub">Windows has no way to put a real button inside the taskbar, so this floats on top of it. Hover it for the exact numbers.</div>`;
  return html;
}

function settingsAccounts() {
  const cx = data.codex,
    cl = data.claude;
  const accounts = cl.accounts || [];
  let html = `<div class="grp-label">Accounts</div>`;
  // Built first so a machine with neither provider signed in gets the add-folder
  // button on its own rather than an empty bordered box above it.
  let rows = cx.available ? codexAccountRow() : "";
  if (accounts.length) {
    for (const a of accounts) {
      rows +=
        editingAccount === a.id
          ? `<div class="acct-row">${renameField()}</div>`
          : claudeAccountRow(a);
    }
  } else if (cl.available) {
    // A snapshot with no per-account list still stands for the default folder.
    // Without a row here it would be the one account that couldn't be hidden.
    rows += claudeAccountRow({ id: "", label: "Claude", active: true, removable: false });
  }
  if (rows) {
    html += `<div class="grp">${rows}</div>`;
    if (removeError)
      html += `<div class="banner small">Couldn't remove that folder — ${esc(removeError)}</div>`;
    html += `<div class="sec-sub">Hiding an account only takes it off that surface — the folder stays registered, and its usage keeps being collected and recorded.</div>`;
  }
  html += addFolderControl();

  if (creditAccounts().length || (codexInPanel() && typeof cx.credits === "number")) {
    html += `<div class="grp-label">Usage credits</div><div class="grp">
      <div class="grp-row" data-credits tabindex="0" role="button">
        <span class="rlab">Manage credits<span class="rsub">Monthly caps and the past-limits switch</span></span>
        <span class="go">›</span>
      </div></div>`;
  }
  return html;
}

function settingsAdvanced() {
  const cx = data.codex,
    cl = data.claude;
  const accounts = cl.accounts || [];
  let html = `<div class="grp-label">Data sources</div><div class="grp">`;
  html += diagRow("codex-quota", "Codex quota", cx.health);
  html += diagRow("codex-history", "Codex history", cx.history_health, cx.estimate, true);
  for (const a of accounts) html += diagRow(`claude-account:${a.id}`, a.label, a.health);
  html += diagRow("claude-history", "Claude history", cl.history_health, cl.estimate, true);
  html += windowHistoryRow();
  html += `</div>`;
  html += renderAbout();
  return html;
}

function renderSettings() {
  if (!SETTINGS_SECTIONS.some(([key]) => key === settingsSection)) settingsSection = "alerts";
  let html = header("Settings", "", { hideRefresh: true, rightText: "" });
  html += settingsNav();
  html += {
    alerts: settingsAlerts,
    display: settingsDisplay,
    widget: settingsWidget,
    accounts: settingsAccounts,
    advanced: settingsAdvanced,
  }[settingsSection]();
  return html;
}

const channelLabel = (channel) => (channel === "nightly" ? "Nightly" : "Stable");

// Settings → About: version, updater status, the channel, and the two update
// toggles.
function renderAbout() {
  if (!updateState) return "";
  const status = updateState.status || { kind: "idle" };
  const checkBtn = `<button class="row-btn" data-update-check>Check now</button>`;
  let detail;
  let action = "";

  switch (status.kind) {
    case "checking":
      detail = "Checking…";
      break;
    case "up_to_date":
      detail = "Up to date";
      action = checkBtn;
      break;
    case "available":
      detail = `${channelLabel(status.channel)} ${esc(status.version)} available`;
      action = `<button class="row-btn" data-update-install>Install and restart</button>`;
      break;
    case "downloading":
      detail = `Downloading… ${status.percent}%`;
      break;
    case "installing":
      detail = `Installing ${esc(status.version)}… the app will restart`;
      break;
    case "error":
      detail = `Couldn't check for updates`;
      action = checkBtn;
      break;
    default:
      detail = updateState.last_checked_at
        ? `Last checked ${esc(clockTime(updateState.last_checked_at))}`
        : "Not checked yet";
      action = checkBtn;
  }

  let html = `<div class="grp-label">About</div><div class="grp">`;
  html += `<div class="grp-row static-row">
      <span class="rlab">${channelLabel(updateState.current_channel)} ${esc(
        updateState.current_version
      )}<span class="rsub">${esc(detail)}</span></span>
      ${action}
    </div>`;
  // Nightly reads the stable feed too, so switching to it never strands you
  // behind a stable release; switching back waits for stable to overtake.
  const channel = updateState.channel || "stable";
  html += `<div class="grp-row col">
      <span class="rlab">Update channel<span class="rsub">${
        channel === "nightly"
          ? "Nightly builds as they're made, and stable when it's newer"
          : "Tested releases only"
      }</span></span>
      <div class="choices">${["stable", "nightly"]
        .map(
          (c) =>
            `<button class="choice${c === channel ? " on" : ""}" data-update-channel="${c}">${channelLabel(
              c
            )}</button>`
        )
        .join("")}</div>
    </div>`;
  html += settingRow(
    "Check for updates",
    "In the background, every few hours",
    "data-update-toggle=\"check_automatically\"",
    updateState.settings.check_automatically,
    false
  );
  html += settingRow(
    "Install automatically",
    "Update and restart without asking",
    "data-update-toggle=\"install_automatically\"",
    updateState.settings.install_automatically,
    false
  );
  if (startOnLogin !== null)
    html += settingRow(
      "Start with Windows",
      "Open in the tray when you sign in",
      "data-start-on-login",
      startOnLogin,
      false
    );
  html += `</div>`;

  // The raw error is shown separately: it can be long, and the row above keeps
  // a fixed height.
  if (status.kind === "error")
    html += `<div class="banner small">${esc(status.message)}</div>`;
  if (updateError)
    html += `<div class="banner small">Couldn't save the update setting — try again.</div>`;
  if (startOnLoginError)
    html += `<div class="banner small">Couldn't change the startup setting — try again.</div>`;

  return html;
}

// ---------- render ----------
// A provider with nothing left to show has no tab: an empty Codex screen is
// worse than no Codex screen. Settings and Overview are always reachable, so
// there is never a state with nowhere to turn it back on from.
function applyTabVisibility() {
  const showable = { codex: !data || codexInPanel(), claude: !data || claudeInPanel() };
  for (const [tab, on] of Object.entries(showable)) {
    const button = document.querySelector(`.tab[data-tab="${tab}"]`);
    if (button) button.hidden = !on;
  }
  // Hiding the tab you are standing on would leave the panel on a screen the
  // tab strip no longer offers a way back to.
  if (showable[activeTab] === false) {
    activeTab = "overview";
    markActiveTab();
  }
}

// A selector that finds "the same control" in freshly rendered markup: its id,
// or its first data-* attribute and value. Every interactive element here is
// tagged one of those two ways, because that is how it is wired up below.
function controlSelector(el) {
  if (!el || el === document.body) return null;
  if (el.id) return `#${CSS.escape(el.id)}`;
  const attr = [...el.attributes].find((a) => a.name.startsWith("data-"));
  if (!attr) return null;
  return attr.value
    ? `[${attr.name}="${CSS.escape(attr.value)}"]`
    : `[${attr.name}]`;
}

// Enter and Space on something drawn as a div but acting as a button.
function onActivate(el, handler) {
  el.addEventListener("click", handler);
  el.addEventListener("keydown", (event) => {
    if (event.target !== el || (event.key !== "Enter" && event.key !== " ")) return;
    event.preventDefault();
    handler(event);
  });
}

function render() {
  const content = document.getElementById("content");
  applyTabVisibility();
  if (!data) {
    content.innerHTML = `<div class="loading">Loading usage…</div>`;
    fitWindowHeight();
    return;
  }
  // Every refresh rebuilds the screen. Without this, a keyboard user's place
  // would be thrown back to the top of the page once a minute.
  const focused = content.contains(document.activeElement)
    ? controlSelector(document.activeElement)
    : null;
  const wide = panelExpanded && activeTab === "windows";
  document.getElementById("app").classList.toggle("expanded", wide);
  if (creditsOpen) content.innerHTML = renderCredits();
  else if (activeTab === "codex") content.innerHTML = renderCodex();
  else if (activeTab === "claude") content.innerHTML = renderClaude();
  else if (activeTab === "windows") content.innerHTML = wide ? renderWindowsWide() : renderWindows();
  else if (activeTab === "settings") content.innerHTML = renderSettings();
  else content.innerHTML = renderOverview();
  if (focused) {
    try {
      content.querySelector(focused)?.focus({ preventScroll: true });
    } catch (_) {}
  }

  const refreshBtn = document.getElementById("refreshBtn");
  if (refreshBtn) refreshBtn.addEventListener("click", refresh);

  // Timeline: range switch, and the expand/collapse pair.
  content.querySelectorAll("[data-window-range]").forEach((button) =>
    button.addEventListener("click", () => {
      windowRange = button.dataset.windowRange;
      try {
        localStorage.setItem("windowRange", windowRange);
      } catch (_) {}
      render();
    })
  );
  const expandBtn = document.getElementById("windowExpand");
  if (expandBtn) expandBtn.addEventListener("click", () => setPanelExpanded(true));
  const collapseBtn = document.getElementById("windowCollapse");
  if (collapseBtn) collapseBtn.addEventListener("click", () => setPanelExpanded(false));

  content.querySelectorAll("[data-goto]").forEach((card) =>
    onActivate(card, () => switchTab(card.dataset.goto))
  );
  // Credits navigation. stopPropagation so a meter inside an Overview card
  // opens the Credits screen instead of following the card's tab link.
  content.querySelectorAll("[data-credits]").forEach((el) =>
    onActivate(el, (event) => {
      event.stopPropagation();
      creditsOpen = true;
      render();
    })
  );
  // "Use a reset" on the Overview's top card: the reset is spent where it
  // always is, behind the inline confirm on its provider's tab.
  content.querySelectorAll("[data-use-reset-go]").forEach((el) =>
    el.addEventListener("click", (event) => {
      event.stopPropagation();
      goUseReset(el.dataset.useResetGo);
    })
  );
  content.querySelectorAll("[data-open-url]").forEach((el) =>
    el.addEventListener("click", () =>
      invoke("plugin:opener|open_url", { url: el.dataset.openUrl }).catch(() => {})
    )
  );

  // First-launch card.
  content.querySelectorAll("[data-welcome]").forEach((el) =>
    el.addEventListener("click", () => welcomeAction(el.dataset.welcome))
  );
  content.querySelectorAll("[data-welcome-dismiss]").forEach((el) =>
    el.addEventListener("click", () => {
      welcomeDismissed = true;
      try {
        localStorage.setItem("welcomeDismissed", "1");
      } catch (_) {}
      render();
    })
  );

  // Settings sections, and the controls only some of them draw.
  content.querySelectorAll("[data-settings-section]").forEach((el) =>
    el.addEventListener("click", () => {
      settingsSection = el.dataset.settingsSection;
      try {
        localStorage.setItem("settingsSection", settingsSection);
      } catch (_) {}
      render();
      content.scrollTop = 0;
    })
  );
  content.querySelectorAll("[data-warn-level]").forEach((el) =>
    el.addEventListener("click", () => {
      const level = Number(el.dataset.warnLevel);
      const levels = new Set(notificationSettings.warn_levels || []);
      if (levels.has(level)) levels.delete(level);
      else levels.add(level);
      setAlertOptions({ warn_levels: [...levels].sort((a, b) => a - b) });
    })
  );
  content.querySelectorAll("[data-alert-flag]").forEach((el) =>
    el.addEventListener("click", () => {
      const flag = el.dataset.alertFlag;
      setAlertOptions({ [flag]: !notificationSettings[flag] });
    })
  );
  content.querySelectorAll("[data-alert-flag-switch]").forEach((input) =>
    input.addEventListener("change", () =>
      setAlertOptions({ [input.dataset.alertFlagSwitch]: input.checked })
    )
  );
  content.querySelectorAll("[data-quiet]").forEach((input) =>
    input.addEventListener("change", () => {
      const [h, m] = input.value.split(":").map(Number);
      if (Number.isFinite(h) && Number.isFinite(m))
        setAlertOptions({ [input.dataset.quiet]: h * 60 + m });
    })
  );
  content.querySelectorAll("[data-tray-icon]").forEach((el) =>
    el.addEventListener("click", async () => {
      const previous = trayIconMode;
      trayIconMode = el.dataset.trayIcon;
      render();
      try {
        trayIconMode = (await invoke("set_tray_icon_mode", { mode: trayIconMode })).icon;
      } catch (_) {
        trayIconMode = previous;
      }
      render();
    })
  );
  content.querySelectorAll("[data-compact]").forEach((el) =>
    el.addEventListener("click", () => {
      compactMode = el.dataset.compact;
      try {
        localStorage.setItem("compactOverview", compactMode);
      } catch (_) {}
      render();
    })
  );
  const creditsBack = document.getElementById("creditsBack");
  if (creditsBack)
    creditsBack.addEventListener("click", () => {
      creditsOpen = false;
      render();
    });
  content.querySelectorAll("[data-notify-provider]").forEach((input) =>
    input.addEventListener("change", () =>
      setNotifyEnabled(input.dataset.notifyProvider, input.checked)
    )
  );
  content.querySelectorAll("[data-widget-enabled]").forEach((input) =>
    input.addEventListener("change", async () => {
      const enabled = input.checked;
      try {
        await invoke("set_widget_enabled", { enabled });
        widgetSettings = { ...widgetSettings, enabled };
      } catch (_) {
        // Put the switch back if the backend refused to persist it.
        input.checked = !enabled;
      }
      render();
    })
  );
  content.querySelectorAll("[data-widget-option]").forEach((input) =>
    input.addEventListener("change", async () => {
      const name = input.dataset.widgetOption;
      const value = input.checked;
      try {
        await invoke("set_widget_option", { name, value });
        widgetSettings = { ...widgetSettings, [name]: value };
      } catch (_) {
        input.checked = !value;
      }
      render();
    })
  );
  content.querySelectorAll("[data-widget-width]").forEach((btn) =>
    btn.addEventListener("click", async () => {
      const step = Number(btn.dataset.widgetWidth) || 0;
      try {
        // No clamping here: the backend owns the bounds, and reading the result
        // back is what keeps the stepper honest at the limits rather than
        // duplicating the min and max in a third place.
        await invoke("set_widget_width", {
          width: (widgetSettings.width || 114) + step,
          // The stepper only changes the width; the gap it is anchored by is
          // the widget's own drag to set.
          gap: null,
          commit: true,
        });
        widgetSettings = await invoke("get_widget_settings");
      } catch (_) {}
      render();
    })
  );
  content.querySelectorAll("[data-widget-row]").forEach((btn) =>
    btn.addEventListener("click", async () => {
      const id = btn.dataset.widgetRow;
      const row = btn.dataset.widgetRowValue;
      const previous = widgetSettings;
      // Flip the preview now; the widget redraws off the broadcast that the
      // command sends, so the two land within a frame of each other either way.
      const rows = { ...(previous.account_rows || {}) };
      if (row === "auto") delete rows[id];
      else rows[id] = row;
      widgetSettings = { ...previous, account_rows: rows };
      render();
      try {
        widgetSettings = await invoke("set_widget_account_row", {
          id,
          row: row === "auto" ? null : row,
        });
      } catch (_) {
        widgetSettings = previous;
      }
      render();
    })
  );
  content.querySelectorAll("[data-widget-monitor]").forEach((btn) =>
    btn.addEventListener("click", async () => {
      try {
        await invoke("set_widget_monitor", { name: btn.dataset.widgetMonitor });
        widgetSettings = await invoke("get_widget_settings");
        widgetMonitors = await invoke("list_widget_monitors");
        widgetPlacement = await invoke("get_widget_placement");
      } catch (_) {}
      render();
    })
  );
  content.querySelectorAll("[data-widget-reset]").forEach((btn) =>
    btn.addEventListener("click", async () => {
      try {
        await invoke("reset_widget_position");
        widgetSettings = await invoke("get_widget_settings");
      } catch (_) {}
      render();
    })
  );
  content.querySelectorAll("[data-meter-fill]").forEach((input) =>
    input.addEventListener("change", () => {
      meterFillsUp = input.checked;
      try {
        localStorage.setItem("meterFillsUp", input.checked ? "1" : "0");
      } catch (_) {}
      render();
    })
  );
  content.querySelectorAll("[data-meter-pace]").forEach((input) =>
    input.addEventListener("change", () => {
      meterPaceLine = input.checked;
      try {
        localStorage.setItem("meterPaceLine", input.checked ? "1" : "0");
      } catch (_) {}
      render();
    })
  );
  content.querySelectorAll("[data-meter-recent]").forEach((input) =>
    input.addEventListener("change", () => {
      showRecentBurn = input.checked;
      try {
        localStorage.setItem("showRecentBurn", input.checked ? "1" : "0");
      } catch (_) {}
      // Nothing has been fetched while the band was off, so switching it on has
      // to go and get the figures before the meters can draw them.
      if (showRecentBurn) loadRecentBurn(true);
      render();
    })
  );
  content.querySelectorAll("[data-recent-minutes]").forEach((button) =>
    button.addEventListener("click", async () => {
      const minutes = Number(button.dataset.recentMinutes);
      try {
        await invoke("set_recent_minutes", { minutes });
        widgetSettings = { ...widgetSettings, recent_minutes: minutes };
      } catch (_) {
        // Leave the old reach in place rather than showing a range the
        // recorder wasn't asked for.
        return;
      }
      await loadRecentBurn();
      render();
    })
  );
  content.querySelectorAll("[data-model-weekly]").forEach((input) =>
    input.addEventListener("change", () => {
      showModelWeekly = input.checked;
      try {
        localStorage.setItem("showModelWeekly", input.checked ? "1" : "0");
      } catch (_) {}
      render();
    })
  );
  content.querySelectorAll("[data-update-toggle]").forEach((input) =>
    input.addEventListener("change", () =>
      setUpdateSetting(input.dataset.updateToggle, input.checked)
    )
  );
  content.querySelectorAll("[data-start-on-login]").forEach((input) =>
    input.addEventListener("change", () => setStartOnLogin(input.checked))
  );
  content.querySelectorAll("[data-update-channel]").forEach((button) =>
    button.addEventListener("click", async () => {
      try {
        updateState = await invoke("set_update_channel", { channel: button.dataset.updateChannel });
      } catch (_) {
        updateError = "channel";
      }
      render();
    })
  );
  content.querySelectorAll("[data-update-check]").forEach((button) =>
    button.addEventListener("click", () => {
      invoke("check_for_updates_now").catch(() => {});
    })
  );
  content.querySelectorAll("[data-update-install]").forEach((button) =>
    button.addEventListener("click", () => {
      button.disabled = true;
      button.textContent = "Starting…";
      invoke("install_update").catch(() => {});
    })
  );
  content.querySelectorAll("[data-health-toggle]").forEach((button) =>
    button.addEventListener("click", () => {
      const key = button.dataset.healthToggle;
      if (expandedHealth.has(key)) expandedHealth.delete(key);
      else expandedHealth.add(key);
      render();
    })
  );
  content.querySelectorAll("[data-health-copy]").forEach((button) =>
    button.addEventListener("click", async (event) => {
      event.stopPropagation();
      try {
        await navigator.clipboard.writeText(button.dataset.healthCopy || "");
        button.textContent = "Copied";
      } catch (_) {
        button.textContent = "Copy failed";
      }
    })
  );
  content.querySelectorAll("[data-window-history-clear]").forEach((button) =>
    button.addEventListener("click", async (event) => {
      event.stopPropagation();
      button.disabled = true;
      button.textContent = "Clearing…";
      try {
        await invoke("clear_window_history");
        windowHistory = null;
        render();
      } catch (_) {
        button.disabled = false;
        button.textContent = "Clear failed";
      }
    })
  );
  content.querySelectorAll("[data-history-cache-clear]").forEach((button) =>
    button.addEventListener("click", async (event) => {
      event.stopPropagation();
      button.disabled = true;
      button.textContent = "Clearing…";
      try {
        await invoke("clear_history_cache");
        button.textContent = "Rebuilding…";
        await refresh();
      } catch (_) {
        button.disabled = false;
        button.textContent = "Clear failed";
      }
    })
  );

  // Reset-credit "Use" controls (inline two-step confirm).
  content.querySelectorAll("[data-reset-use]").forEach((el) =>
    el.addEventListener("click", () => startUseReset(el.dataset.resetUse))
  );
  content.querySelectorAll("[data-reset-confirm]").forEach((el) =>
    el.addEventListener("click", () => confirmUseReset(el.dataset.resetConfirm))
  );
  content.querySelectorAll("[data-reset-cancel]").forEach((el) =>
    el.addEventListener("click", cancelUseReset)
  );

  // Per-account show/hide chips (Settings → Accounts).
  content.querySelectorAll("[data-vis-surface]").forEach((btn) =>
    btn.addEventListener("click", () =>
      setAccountShown(btn.dataset.visSurface, btn.dataset.visId, !btn.classList.contains("on"))
    )
  );

  // Per-account rename / forget controls (Settings).
  content.querySelectorAll("[data-rename]").forEach((el) =>
    el.addEventListener("click", () => startRename(el.dataset.rename))
  );
  content.querySelectorAll("[data-remove]").forEach((el) =>
    el.addEventListener("click", () => {
      confirmingRemove = el.dataset.remove;
      removeError = null;
      render();
      content.querySelector("[data-remove-cancel]")?.focus();
    })
  );
  content.querySelectorAll("[data-remove-confirm]").forEach((el) =>
    el.addEventListener("click", () => removeDirectory(el.dataset.removeConfirm))
  );
  content.querySelectorAll("[data-remove-cancel]").forEach((el) =>
    el.addEventListener("click", () => {
      confirmingRemove = null;
      render();
    })
  );
  content.querySelectorAll("[data-rename-cancel]").forEach((el) =>
    el.addEventListener("click", cancelRename)
  );
  content.querySelectorAll("[data-rename-save]").forEach((btn) =>
    btn.addEventListener("click", () => {
      const inp = content.querySelector("[data-rename-input]");
      saveAccountLabel(btn.dataset.renameSave, inp ? inp.value : "");
    })
  );
  const renameInput = content.querySelector("[data-rename-input]");
  if (renameInput) {
    renameInput.addEventListener("input", () => {
      editingDraft = renameInput.value;
    });
    renameInput.addEventListener("keydown", (e) => {
      if (e.key === "Enter") {
        e.preventDefault();
        saveAccountLabel(editingAccount, renameInput.value);
      } else if (e.key === "Escape") {
        e.preventDefault();
        cancelRename();
      }
    });
    if (editFocusPending) {
      renameInput.focus();
      renameInput.select();
      editFocusPending = false;
    }
  }

  // Add-folder control.
  const addOpen = content.querySelector("[data-add-folder-open]");
  if (addOpen) addOpen.addEventListener("click", openAddFolder);
  const addCancel = content.querySelector("[data-add-folder-cancel]");
  if (addCancel) addCancel.addEventListener("click", cancelAddFolder);
  const addSave = content.querySelector("[data-add-folder-save]");
  const addInput = content.querySelector("[data-add-folder-input]");
  if (addSave)
    addSave.addEventListener("click", () => addDirectory(addInput ? addInput.value : ""));
  if (addInput) {
    addInput.addEventListener("input", () => {
      addFolderDraft = addInput.value;
    });
    addInput.addEventListener("keydown", (e) => {
      if (e.key === "Enter") {
        e.preventDefault();
        addDirectory(addInput.value);
      } else if (e.key === "Escape") {
        e.preventDefault();
        cancelAddFolder();
      }
    });
    if (addFolderFocusPending) {
      addInput.focus();
      addFolderFocusPending = false;
    }
  }

  fitWindowHeight();
}

function fitWindowHeight() {
  // The expanded timeline sizes the window itself.
  if (panelExpanded) return;
  requestAnimationFrame(() => {
    const tabs = document.getElementById("tabs");
    const content = document.getElementById("content");
    if (!tabs || !content) return;

    const contentBox = content.getBoundingClientRect();
    const childBottom = [...content.children].reduce((bottom, child) => {
      return Math.max(bottom, child.getBoundingClientRect().bottom - contentBox.top);
    }, 0);
    const contentStyle = getComputedStyle(content);
    const contentHeight =
      childBottom + Number.parseFloat(contentStyle.paddingBottom || "0") + 2;
    const height = tabs.offsetHeight + contentHeight + 2;
    invoke("fit_window_height", { height }).catch(() => {});
  });
}

function switchTab(tab) {
  activeTab = tab;
  // Monitors and placement can both change while the panel is closed.
  if (tab === "settings") loadWidgetSettings();
  creditsOpen = false;
  // The wide layout belongs to the timeline; leaving it puts the popover back.
  if (panelExpanded && tab !== "windows") setPanelExpanded(false);
  if (tab === "windows") loadWindowHistory();
  markActiveTab();
  render();
}

// The active tab's look, and its tab semantics: selected, and the one stop in
// the tab order (the arrow keys move between the rest).
function markActiveTab() {
  document.querySelectorAll(".tab").forEach((t) => {
    const on = t.dataset.tab === activeTab;
    t.classList.toggle("active", on);
    t.setAttribute("aria-selected", String(on));
    t.tabIndex = on ? 0 : -1;
  });
}

// ---------- refresh ----------
// The spinner is bound to the request rather than a fixed timeout, then rounded
// up to the next whole rotation so it never stops mid-turn.
const SPIN_MS = 700;
let refreshing = false;
let spinUntil = 0;
let spinTimer = null;

function spinning() {
  return refreshing || Date.now() < spinUntil;
}

async function refresh() {
  if (refreshing) return;
  refreshing = true;
  const started = Date.now();
  const btn = document.getElementById("refreshBtn");
  if (btn) btn.classList.add("busy");

  // Paint the cached snapshot immediately; the fresh numbers replace it
  // below once the (slow) collection finishes.
  invoke("get_cached_usage")
    .then((cached) => {
      if (refreshing && cached && (!data || cached.generated_at > data.generated_at)) {
        data = cached;
        render();
      }
    })
    .catch(() => {});
  try {
    data = await invoke("get_usage");
    refreshFailedAt = null;
  } catch (e) {
    // Keep stale data on screen if we have any; only blank out on first load.
    if (!data) {
      document.getElementById("content").innerHTML =
        `<div class="empty-state">Failed to load usage.<br/>${esc(e)}</div>`;
      refreshing = false;
      return;
    }
    refreshFailedAt = Date.now();
  }
  refreshing = false;
  // The collection just fed the recorder, so the timeline's copy and the
  // meters' band are a sample behind until they re-read it.
  loadWindowHistory();
  loadRecentBurn(true);
  const elapsed = Date.now() - started;
  spinUntil = Date.now() + (SPIN_MS - (elapsed % SPIN_MS));
  render();
  clearTimeout(spinTimer);
  spinTimer = setTimeout(() => {
    spinUntil = 0;
    render();
  }, SPIN_MS - (elapsed % SPIN_MS));
}

async function loadNotificationSettings() {
  try {
    notificationSettings = await invoke("get_notification_settings");
    render();
  } catch (_) {}
}

// Loaded independently, and loudly. A single try/catch around all three meant
// one failure left the rest unloaded — an empty monitor list reads exactly like
// "you only have one display" — and the silent catch left nothing to diagnose.
async function loadWidgetSettings() {
  widgetLoadError = "";
  for (const [cmd, apply] of [
    ["get_widget_settings", (v) => (widgetSettings = v)],
    ["get_widget_placement", (v) => (widgetPlacement = v)],
    ["list_widget_monitors", (v) => (widgetMonitors = v || [])],
  ]) {
    try {
      apply(await invoke(cmd));
    } catch (e) {
      widgetLoadError = `${cmd}: ${e}`;
      console.error("widget settings:", cmd, e);
    }
  }
  render();
}

async function loadUpdateState() {
  try {
    updateState = await invoke("get_update_state");
    render();
  } catch (_) {}
}

async function setUpdateSetting(key, enabled) {
  const previous = updateState.settings;
  updateState = { ...updateState, settings: { ...previous, [key]: enabled } };
  updateError = null;
  render();
  try {
    const settings = await invoke("set_update_setting", { key, enabled });
    updateState = { ...updateState, settings };
  } catch (_) {
    updateState = { ...updateState, settings: previous };
    updateError = key;
  }
  render();
}

async function loadStartOnLogin() {
  try {
    startOnLogin = await invoke("get_start_on_login");
    render();
  } catch (_) {}
}

async function setStartOnLogin(enabled) {
  const previous = startOnLogin;
  startOnLogin = enabled;
  startOnLoginError = false;
  render();
  try {
    startOnLogin = await invoke("set_start_on_login", { enabled });
  } catch (_) {
    startOnLogin = previous;
    startOnLoginError = true;
  }
  render();
}

async function setNotifyEnabled(provider, enabled) {
  const previous = { ...notificationSettings };
  notificationSettings = { ...notificationSettings, [provider]: enabled };
  notifyError = null;
  render();
  try {
    notificationSettings = await invoke("set_notification_enabled", { provider, enabled });
  } catch (e) {
    notificationSettings = previous;
    notifyError = provider;
  }
  render();
}

// Merge `changes` into the alert options and save the whole set. Optimistic,
// like the other switches: a refusal puts the old values back.
async function setAlertOptions(changes) {
  const previous = notificationSettings;
  const next = { ...previous, ...changes };
  notificationSettings = next;
  notifyOptionsError = false;
  render();
  try {
    notificationSettings = await invoke("set_notification_options", {
      options: {
        warn_levels: next.warn_levels,
        used_up: next.used_up,
        forecast: next.forecast,
        quiet_hours: next.quiet_hours,
        quiet_start: next.quiet_start,
        quiet_end: next.quiet_end,
      },
    });
  } catch (_) {
    notificationSettings = previous;
    notifyOptionsError = true;
  }
  render();
}

async function loadTraySettings() {
  try {
    trayIconMode = (await invoke("get_tray_settings")).icon || "tightest";
  } catch (_) {}
}

// The first-launch card's "Turn on" buttons, each the same action its
// Settings switch performs.
async function welcomeAction(what) {
  if (what === "widget") {
    try {
      await invoke("set_widget_enabled", { enabled: true });
      widgetSettings = { ...widgetSettings, enabled: true };
    } catch (_) {}
    render();
  } else if (what === "alerts") {
    if (data?.codex?.available) await setNotifyEnabled("codex", true);
    if (data?.claude?.available) await setNotifyEnabled("claude", true);
  } else if (what === "start") {
    await setStartOnLogin(true);
  }
}

// Take a reset key to the confirm that spends it: its provider's tab, with
// the inline "Use it?" already open. The tray menu's "Use a reset" lands here
// too, so spending one is always the same two clicks in the same place.
function goUseReset(key) {
  const tab = key.startsWith("claude:") ? "claude" : "codex";
  switchTab(tab);
  startUseReset(key);
  // render() is synchronous, so the confirm is already in the page.
  const confirm = document.querySelector(`[data-reset-confirm="${CSS.escape(key)}"]`);
  confirm?.scrollIntoView({ block: "center" });
  confirm?.focus();
}

// ---------- reset credits (Codex) and banked resets (Claude) ----------
function startUseReset(key) {
  confirmingReset = key;
  resetError = null;
  claudeResetNotice = null;
  render();
}

function cancelUseReset() {
  confirmingReset = null;
  render();
}

async function confirmUseReset(key) {
  confirmingReset = null;
  resetError = null;
  claudeResetNotice = null;
  usingReset = key;
  render();
  if (key.startsWith("claude:")) {
    const accountId = key.slice("claude:".length);
    const acct = (data.claude.accounts || []).find((a) => a.id === accountId);
    try {
      const creditId = acct?.resets?.next_credit_id;
      if (!creditId) throw "No reset credit left.";
      const outcome = await invoke("consume_claude_reset", { accountId, creditId });
      const notice = {
        acct: accountId,
        text: CLAUDE_RESET_OUTCOMES[outcome] || "Done.",
        error: false,
      };
      claudeResetNotice = notice;
      // The outcome is news for a moment, not a permanent caption.
      setTimeout(() => {
        if (claudeResetNotice !== notice) return;
        claudeResetNotice = null;
        render();
      }, 60000);
    } catch (e) {
      claudeResetNotice = { acct: accountId, text: String(e), error: true };
    }
  } else {
    try {
      await invoke("consume_codex_reset", { creditId: key.slice("codex:".length) });
    } catch (e) {
      resetError = String(e);
    }
  }
  usingReset = null;
  // Pull fresh numbers either way: success drops the spent credit, failure
  // restores the list so the user can retry.
  refresh();
}

// ---------- account visibility ----------
// Optimistic: the chip flips now and the backend confirms, so a click doesn't
// wait on a disk write. A refusal puts the old record back rather than leaving
// the panel drawing something the file disagrees with.
async function setAccountShown(surface, id, shown) {
  const previous = accountVisibility;
  const key = `${surface}_hidden`;
  const hidden = new Set(previous[key] || []);
  if (shown) hidden.delete(id);
  else hidden.add(id);
  accountVisibility = { ...previous, [key]: [...hidden] };
  render();
  try {
    accountVisibility = await invoke("set_account_hidden", { surface, id, hidden: !shown });
  } catch (_) {
    accountVisibility = previous;
  }
  render();
}

async function loadAccountVisibility() {
  try {
    accountVisibility = await invoke("get_account_visibility");
  } catch (_) {
    // Nothing hidden is the safe reading: a card too many is a smaller failure
    // than an account that silently isn't there.
  }
  render();
}

// ---------- Claude account management ----------
function startRename(id) {
  const a = (data?.claude?.accounts || []).find((x) => x.id === id);
  editingAccount = id;
  editingDraft = a ? a.label : "";
  editFocusPending = true;
  render();
}

function cancelRename() {
  editingAccount = null;
  editingDraft = "";
  render();
}

async function saveAccountLabel(id, label) {
  editingAccount = null;
  editingDraft = "";
  try {
    await invoke("set_claude_account_label", { id, label });
  } catch (_) {}
  refresh();
}

async function removeDirectory(id) {
  confirmingRemove = null;
  removeError = null;
  try {
    await invoke("remove_claude_directory", { id });
  } catch (e) {
    removeError = String(e);
  }
  refresh();
}

function openAddFolder() {
  addingFolder = true;
  addFolderDraft = "";
  addFolderError = null;
  addFolderFocusPending = true;
  render();
}

function cancelAddFolder() {
  addingFolder = false;
  addFolderDraft = "";
  addFolderError = null;
  render();
}

async function addDirectory(path) {
  try {
    await invoke("add_claude_directory", { path });
    addingFolder = false;
    addFolderDraft = "";
    addFolderError = null;
    refresh();
  } catch (e) {
    // Keep the input open so the user can fix the path.
    addFolderError = String(e);
    render();
  }
}

// ---------- wiring ----------
document
  .querySelectorAll(".tab")
  .forEach((t) => t.addEventListener("click", () => switchTab(t.dataset.tab)));
// Arrow keys move between tabs, as in any tab strip; Home and End jump to
// the ends. Hidden tabs (a provider with nothing to show) are skipped.
document.getElementById("tabs").addEventListener("keydown", (event) => {
  const tabs = [...document.querySelectorAll(".tab")].filter((t) => !t.hidden);
  const at = tabs.indexOf(document.activeElement);
  if (at < 0) return;
  const next = {
    ArrowRight: tabs[(at + 1) % tabs.length],
    ArrowLeft: tabs[(at - 1 + tabs.length) % tabs.length],
    Home: tabs[0],
    End: tabs[tabs.length - 1],
  }[event.key];
  if (!next) return;
  event.preventDefault();
  next.focus();
  switchTab(next.dataset.tab);
});
markActiveTab();

// Rust sends this each time it shows the panel, which is also the moment to
// replay the opening animation — the window is hidden and shown, never
// reloaded, so a plain CSS animation would only ever run once.
listen("refresh", () => {
  const app = document.getElementById("app");
  app.classList.remove("opening");
  void app.offsetWidth;
  app.classList.add("opening");
  refresh();
});
listen("refresh-now", refresh);
// A collection the panel didn't start — the file watcher, the background poll —
// has finished. Paint the snapshot it left rather than collecting again, which
// would ask every provider a second time for the numbers it already has. The
// panel's own refresh emits this too, but by then `data` already holds that
// snapshot, so the timestamp check turns it into a no-op.
listen("usage-updated", async () => {
  if (refreshing) return;
  let cached;
  try {
    cached = await invoke("get_cached_usage");
  } catch (_) {
    return;
  }
  if (!cached || (data && cached.generated_at <= data.generated_at)) return;
  data = cached;
  refreshFailedAt = null;
  // The collection fed the recorder as well, so these are a sample behind.
  loadWindowHistory();
  loadRecentBurn(true);
  render();
});
// Rust collapses the panel whenever it hides, so the next tray click opens the
// familiar popover rather than a full-width window.
listen("collapse", () => {
  if (!panelExpanded) return;
  panelExpanded = false;
  render();
});
// Esc backs out one step at a time: an open confirm, then the Credits screen,
// then the expanded timeline, and finally the panel itself — it has no title
// bar to close. Inputs handle their own Esc (cancelling the edit) first.
document.addEventListener("keydown", (event) => {
  if (event.key !== "Escape" || event.defaultPrevented) return;
  if (confirmingReset || confirmingRemove) {
    confirmingReset = null;
    confirmingRemove = null;
    render();
  } else if (creditsOpen) {
    creditsOpen = false;
    render();
  } else if (panelExpanded) {
    setPanelExpanded(false);
  } else {
    window.__TAURI__.window.getCurrentWindow().hide().catch(() => {});
  }
});
// From the tray menu: "Settings…" and "Use a reset" open the panel on the
// right screen.
listen("goto", (event) => {
  if (event.payload === "settings") switchTab("settings");
});
listen("use-reset", (event) => {
  if (event.payload) goUseReset(event.payload);
});
// The tray menu can switch alerts too, so the Settings switches follow it.
listen("notification-settings", (event) => {
  if (event.payload) notificationSettings = event.payload;
  render();
});
listen("tray-settings", (event) => {
  if (event.payload?.icon) trayIconMode = event.payload.icon;
  if (activeTab === "settings") render();
});
// Rust pushes the updater status on every transition, so the About section
// tracks a background check without polling.
listen("widget-placement", (event) => {
  // Pushed whenever the widget's placement actually changes, so the Settings
  // note follows reality instead of a value sampled once at startup.
  if (event.payload) widgetPlacement = event.payload;
  if (activeTab === "settings") render();
});

listen("widget-settings", (event) => {
  // The widget can change these itself, by being dragged or resized.
  const before = recentMinutes();
  if (event.payload) widgetSettings = event.payload;
  // The band's reach is in here too, and it decides what the recorder was asked
  // for — a stale figure would be labelled with the new range.
  if (recentMinutes() !== before) loadRecentBurn(true);
  if (activeTab === "settings") render();
});

// The record is shared with the widget, so it is pushed rather than polled.
listen("account-visibility", (event) => {
  if (event.payload) accountVisibility = event.payload;
  render();
});

listen("update-state", (event) => {
  updateState = event.payload;
  if (activeTab === "settings") render();
});
// Translucent panel only when the native acrylic backdrop is active,
// otherwise the solid fallback background stays.
invoke("glass_enabled")
  .then((g) => {
    if (g) document.documentElement.classList.add("glass");
  })
  .catch(() => {});
// Refresh as soon as the panel is shown again.
document.addEventListener("visibilitychange", () => {
  if (!document.hidden) refresh();
});
// ...and every minute while it stays open. A hidden panel skips the tick; the
// background poll in Rust keeps the numbers moving for the widget and alerts.
//
// Asks the window, not `document.hidden`: WebView2 keeps reporting the page as
// visible after the panel's window is hidden, so that check would run a full
// collection — and a usage call per Claude account — every minute, forever.
setInterval(async () => {
  try {
    if (await window.__TAURI__.window.getCurrentWindow().isVisible()) refresh();
  } catch (_) {}
}, 60_000);
loadNotificationSettings();
loadAccountVisibility();
// The band's reach lives in the widget settings, so its first load has to
// finish before the meters can ask the recorder for the right stretch.
loadWidgetSettings().then(() => loadRecentBurn(true));
loadUpdateState();
loadStartOnLogin();
loadTraySettings();
loadWindowHistory();
refresh();
