import { clamp, elapsedPercent, esc } from "./shared.js";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

let data = null;
let activeTab = "overview";
let creditsOpen = false; // the Credits detail screen, layered over any tab
let notificationSettings = { codex: false, claude: false, codex_resets: false };
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
let notifyError = null; // provider whose toggle failed to save, if any
let updateState = null; // updater status + version, pushed from Rust on change
let updateError = null; // error from the last update toggle, if any
let confirmingReset = null; // credit id awaiting the inline "Use reset" confirm
let resetError = null; // error from the last consume attempt, if any
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
let panelExpanded = false; // timeline showing its wide layout in a widened panel

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

// ---------- meters ----------
// One bar with its text inside: label + headline on the left, reset time on
// the right.
//
// Every meter is driven by the *used* percent, so the warn colouring is the
// same reading in both fill directions; only the width and the headline flip.
// Draining (the default) reads as "fuel left"; filling reads as "how much of
// the allowance is gone".
function meterBar(cls, usedPct, leftHtml, rightHtml, tip, attrs = "", elapsedPct = null) {
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
  return `
    <div class="bgauge ${cls}" title="${tip}" ${attrs}>
      <div class="bfill${empty ? " empty" : ""}" style="width:${empty ? 0 : fill}%"></div>
      ${mark}
      <div class="btxt"><span class="bl">${leftHtml}</span><span class="rr">${rightHtml}</span></div>
    </div>`;
}

// usedPercent 0..100. The headline follows the fill direction so the number
// and the bar always describe the same thing; the other figure is in the
// tooltip either way.
function gauge(label, g, opts = {}) {
  if (!g) return "";
  const usedPercent = Math.min(100, Math.max(0, Number(g.used_percent) || 0));
  const left = (100 - usedPercent).toFixed(0);
  const warn = usedPercent >= 80;
  const cls = [opts.claude ? "claude" : "", warn ? "warn" : ""].join(" ").trim();
  const elapsed = meterPaceLine ? elapsedPercent(g) : null;
  const tip = `${usedPercent.toFixed(0)}% used · ${left}% left${
    g.resets_in ? ` · resets in ${esc(g.resets_in)}` : ""
  }${elapsed === null ? "" : ` · ${elapsed.toFixed(0)}% of the window gone`}`;
  const headline = meterFillsUp
    ? `<b>${usedPercent.toFixed(0)}%</b><span class="sub"> used</span>`
    : `<b>${left}%</b><span class="sub"> left</span>`;
  return meterBar(cls, usedPercent, `${esc(label)} ${headline}`, resetText(g), tip, "", elapsed);
}

// ---------- usage credits ----------
// Accounts whose live snapshot reported an extra_usage block at all.
function creditAccounts() {
  return (data?.claude?.accounts || []).filter((a) => a.extra_usage);
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
  const cls = ["credit", usedPct >= 80 ? "warn" : "", opts.link ? "link" : ""]
    .join(" ").trim();
  const tip = `${usd(used)} of the ${usd(cap)} monthly cap spent`;
  const right = `${usedPct.toFixed(0)}% used${opts.link ? ` <span class="go">›</span>` : ""}`;
  const amount = meterFillsUp ? used : Math.max(0, cap - used);
  return meterBar(
    cls,
    usedPct,
    `${esc(opts.label || "Credits")} <b>${usd(amount)}</b><span class="sub"> of ${usd(cap)}</span>`,
    right,
    tip,
    opts.link ? "data-credits" : ""
  );
}

// ---------- bar chart ----------
function chart(daily, claude) {
  const days = (daily || []).slice(-14);
  if (!days.length) return "";
  const max = Math.max(...days.map((d) => d.tokens), 1);
  const bars = days
    .map((d) => {
      const h = Math.round((d.tokens / max) * 100);
      const cls = d.tokens === 0 ? "bar empty" : "bar";
      const title = `${esc(d.date)}: ${tokens(d.tokens)} tok · ${usd(d.cost)}`;
      return `<div class="${cls}" style="height:${Math.max(4, h)}%" title="${title}"></div>`;
    })
    .join("");
  return `<div class="chart ${claude ? "claude" : ""}" title="Last ${days.length} days">${bars}</div>`;
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
  return `<span class="hd-right"><span title="Last updated">${esc(stamp)}</span>${extra}
    <button class="hd-refresh${busy}" id="refreshBtn" title="Refresh"
      aria-label="Refresh">${refreshIcon()}</button></span>`;
}

function header(title, extras = "", opts = {}) {
  return `<div class="hd"><span class="hd-title">${esc(title)}</span>${extras}${headerControls(
    opts
  )}</div>`;
}

// ---------- merged value + history card ----------
function valueCard(estimate, health, today, todayTok, m30, tok30, daily, claude) {
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

// One reset-credit card: title, expiry, and a Use action that expands into an
// inline two-step confirm (no native dialog). Tinted amber when it expires
// within a day.
function resetCard(cr) {
  const soon =
    typeof cr.expires_at === "number" && cr.expires_at * 1000 - Date.now() <= 24 * 3600 * 1000;
  const title = cr.title || "Free rate-limit reset";
  const expires = cr.expires_at
    ? `Expires ${fmtDate(cr.expires_at)}${cr.expires_in ? ` · ${esc(cr.expires_in)} left` : ""}`
    : cr.granted_at
      ? `Granted ${fmtDate(cr.granted_at)}`
      : "";
  const action =
    confirmingReset === cr.id
      ? `<div class="reset-confirm">
          <span class="reset-q">Use it?</span>
          <button class="acct-btn save" data-reset-confirm="${esc(cr.id)}" title="Confirm">✓</button>
          <button class="acct-btn" data-reset-cancel title="Cancel">✕</button>
        </div>`
      : `<button class="reset-use" data-reset-use="${esc(cr.id)}">Use reset</button>`;
  return `<div class="reset-card ${soon ? "soon" : ""}">
    <div class="reset-info">
      <div class="reset-title">${esc(title)}</div>
      ${expires ? `<div class="reset-meta ${soon ? "soon" : ""}">${expires}</div>` : ""}
    </div>
    ${action}
  </div>`;
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
  else for (const cr of credits) html += resetCard(cr);
  return html;
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
    for (const quota of c.quotas) html += gauge(quota.label, quota.gauge);
  } else {
    // No quota list means no reported window length, so the session label
    // can't name its own hours here.
    html += gauge("Session", c.primary);
    html += gauge("Weekly", c.secondary);
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
    false
  );
  return html;
}

// One account's gauges. `sessionLabel` differs between the detailed tab
// ("Session (5h)") and the compact overview card ("Session").
function claudeGauges(acct, sessionLabel) {
  let h = "";
  if ((acct.quotas || []).length) {
    for (const quota of acct.quotas) {
      const scoped = quota.scope_model || quota.scope_surface;
      if (scoped && !showModelWeekly) continue;
      const label = quota.group === "session" ? sessionLabel : quota.label;
      h += gauge(label, quota.gauge, { claude: true });
    }
    return h;
  }
  h += gauge(sessionLabel, acct.five_hour, { claude: true });
  h += gauge("Weekly", acct.seven_day, { claude: true });
  const mg = acct.seven_day_model;
  if (mg && showModelWeekly) h += gauge(`Weekly (${mg.model})`, mg.gauge, { claude: true });
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
    ? claudeGauges(a, "Session (5h)") + creditsBar(a, { link: true })
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

  const accounts = c.accounts || [];
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
    true
  );
  if (multi)
    html += `<div class="sec-sub">Combined across all accounts — local logs aren't per-account.</div>`;
  return html;
}

function renderOverview() {
  const cx = data.codex,
    cl = data.claude;
  let html = header(
    "Overview",
    `<span class="pill">${usd(cx.cost_today + cl.cost_today)} today</span>`
  );

  // Codex card
  html += `<div class="ov-card" data-goto="codex">
    <div class="ov-head">
      <div class="ov-name">Codex ${healthBadge(cx.health)}
        ${cx.plan_type ? `<span class="pill">${esc(cx.plan_type)}</span>` : ""}</div>
      <span class="ov-cost">${usd(cx.cost_today)} today</span>
    </div>`;
  if ((cx.quotas || []).length) {
    for (const quota of cx.quotas)
      html += gauge(quota.group === "session" ? "Session" : quota.label, quota.gauge);
  } else {
    html += gauge("Session", cx.primary);
    html += gauge("Weekly", cx.secondary);
  }
  if (!cx.available) html += `<div class="sec-sub">No data</div>`;
  const resetCount = availableResets(cx).length;
  if (resetCount)
    html += `<div class="ov-reset">${resetCount} reset credit${
      resetCount > 1 ? "s" : ""
    } available</div>`;
  html += `</div>`;

  // Claude card
  const accounts = cl.accounts || [];
  const multi = accounts.length > 1;
  html += `<div class="ov-card" data-goto="claude">
    <div class="ov-head">
      <div class="ov-name">Claude
        ${multi ? "" : healthBadge(accounts.length ? accounts[0].health : null)}</div>
      <span class="ov-cost">${usd(cl.cost_today)} today</span>
    </div>`;
  if (accounts.length) {
    for (const a of accounts) {
      if (multi)
        html += `<div class="ov-acct">${esc(a.label)}
          ${healthBadge(a.health)}${a.active ? ` <span class="pill">active</span>` : ""}</div>`;
      // Credits stay inside the account's own block: a combined bar appended
      // after the loop reads as belonging to the last account listed.
      if (a.live) html += claudeGauges(a, "Session") + creditsBar(a, { link: true });
      else html += `<div class="sec-sub">Live unavailable — open Claude Code</div>`;
    }
  } else {
    html += `<div class="sec-sub">Live unavailable — open Claude Code</div>`;
  }
  html += `</div>`;

  return html;
}

// ---------- Credits screen ----------
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
  const multi = (cl.accounts || []).length > 1;
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
    html += `<div class="sec-sub">Caps and the past-limits switch are managed at claude.ai → Settings → Billing.</div>`;
  } else {
    html += `<div class="empty-state">No usage-credit info from Claude yet.<br/>It appears after a live refresh on plans with credits.</div>`;
  }

  if (typeof cx.credits === "number") {
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
      key: `${provider}|${account}|${id}`,
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
  if (cx?.live) {
    if ((cx.quotas || []).length) {
      for (const q of cx.quotas) add("codex", "", "Codex", q.id, q.label, q.group, q.gauge);
    } else {
      add("codex", "", "Codex", "session", "Session", "session", cx.primary);
      add("codex", "", "Codex", "weekly", "Weekly", "weekly", cx.secondary);
    }
  }
  for (const a of (data?.claude?.accounts || []).filter((x) => x.live)) {
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
    html += `<div class="empty-state">${
      timelineCredentials().length
        ? "Only short windows are live — see Today."
        : "No live quota windows.<br/>Sign in to Codex or Claude Code and refresh."
    }</div>`;
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
  const all = timelineCredentials();
  const credentials = all
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
    return (
      html +
      `<div class="empty-state">${
        all.length
          ? "Only short windows are live — see Today."
          : "No live quota windows.<br/>Sign in to Codex or Claude Code and refresh."
      }</div>`
    );
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

function renderSettings() {
  const cx = data.codex,
    cl = data.claude;
  let html = header("Settings", "", { hideRefresh: true, rightText: "" });

  html += `<div class="grp-label">Notifications</div><div class="grp">`;
  html += settingRow(
    "Codex limits",
    "Near, hit, and reset",
    'data-notify-provider="codex"',
    notificationSettings.codex,
    false
  );
  html += settingRow(
    "Codex reset credits",
    "When a free reset is granted",
    'data-notify-provider="codex_resets"',
    notificationSettings.codex_resets,
    false
  );
  html += settingRow(
    "Claude limits",
    "Near, hit, and reset",
    'data-notify-provider="claude"',
    notificationSettings.claude,
    true
  );
  html += `</div>`;
  if (notifyError)
    html += `<div class="banner small">Couldn't save the notification setting — try again.</div>`;

  if (creditAccounts().length || typeof cx.credits === "number") {
    html += `<div class="grp-label">Usage credits</div><div class="grp">
      <div class="grp-row" data-credits>
        <span class="rlab">Manage credits<span class="rsub">Monthly caps and the past-limits switch</span></span>
        <span class="go">›</span>
      </div></div>`;
  }

  const scopes = scopedLimitNames(cl.accounts);
  html += `<div class="grp-label">Display</div><div class="grp">`;
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

  html += `<div class="grp-label">Taskbar widget</div><div class="grp">`;
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
      <span class="rlab">Width<span class="rsub">Or drag the widget's left edge</span></span>
      <span class="stepper">
        <button class="acct-btn" data-widget-width="-10" title="Narrower">−</button>
        <b class="stepval">${Math.round(widgetSettings.width || 114)} px</b>
        <button class="acct-btn" data-widget-width="10" title="Wider">+</button>
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
  html += `<div class="sec-sub">Windows has no way to put a real button inside the taskbar, so this floats on top of it.</div>`;

  const accounts = cl.accounts || [];
  html += `<div class="grp-label">Claude accounts</div>`;
  if (accounts.length) {
    html += `<div class="grp">`;
    for (const a of accounts) {
      if (editingAccount === a.id) {
        html += `<div class="acct-row">${renameField()}</div>`;
        continue;
      }
      html += `<div class="acct-row">
        <span class="rlab"><span class="nm">${esc(a.label)}</span>${
          a.active ? ` <span class="pill">default</span>` : ""
        }<span class="pth">${esc(a.id)}</span></span>
        <span class="acts">
          <button class="acct-btn" data-rename="${esc(a.id)}" title="Rename">✎</button>
          ${
            a.removable
              ? `<button class="acct-btn" data-remove="${esc(
                  a.id
                )}" title="Remove this folder">✕</button>`
              : ""
          }
        </span>
      </div>`;
    }
    html += `</div>`;
  }
  html += addFolderControl();

  html += `<div class="grp-label">Data sources</div><div class="grp">`;
  html += diagRow("codex-quota", "Codex quota", cx.health);
  html += diagRow("codex-history", "Codex history", cx.history_health, cx.estimate, true);
  for (const a of accounts) html += diagRow(`claude-account:${a.id}`, a.label, a.health);
  html += diagRow("claude-history", "Claude history", cl.history_health, cl.estimate, true);
  html += windowHistoryRow();
  html += `</div>`;

  html += renderAbout();

  return html;
}

// Settings → About: version, updater status, and the two update toggles.
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
      detail = `Version ${esc(status.version)} available`;
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
      <span class="rlab">Version ${esc(updateState.current_version)}<span class="rsub">${esc(
        detail
      )}</span></span>
      ${action}
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
  html += `</div>`;

  // The raw error is shown separately: it can be long, and the row above keeps
  // a fixed height.
  if (status.kind === "error")
    html += `<div class="banner small">${esc(status.message)}</div>`;
  if (updateError)
    html += `<div class="banner small">Couldn't save the update setting — try again.</div>`;

  return html;
}

// ---------- render ----------
function render() {
  const content = document.getElementById("content");
  if (!data) {
    content.innerHTML = `<div class="loading">Loading usage…</div>`;
    fitWindowHeight();
    return;
  }
  const wide = panelExpanded && activeTab === "windows";
  document.getElementById("app").classList.toggle("expanded", wide);
  if (creditsOpen) content.innerHTML = renderCredits();
  else if (activeTab === "codex") content.innerHTML = renderCodex();
  else if (activeTab === "claude") content.innerHTML = renderClaude();
  else if (activeTab === "windows") content.innerHTML = wide ? renderWindowsWide() : renderWindows();
  else if (activeTab === "settings") content.innerHTML = renderSettings();
  else content.innerHTML = renderOverview();

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
    card.addEventListener("click", () => switchTab(card.dataset.goto))
  );
  // Credits navigation. stopPropagation so a meter inside an Overview card
  // opens the Credits screen instead of following the card's tab link.
  content.querySelectorAll("[data-credits]").forEach((el) =>
    el.addEventListener("click", (event) => {
      event.stopPropagation();
      creditsOpen = true;
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
          commit: true,
        });
        widgetSettings = await invoke("get_widget_settings");
      } catch (_) {}
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

  // Per-account rename / forget controls (Settings).
  content.querySelectorAll("[data-rename]").forEach((el) =>
    el.addEventListener("click", () => startRename(el.dataset.rename))
  );
  content.querySelectorAll("[data-remove]").forEach((el) =>
    el.addEventListener("click", () => removeDirectory(el.dataset.remove))
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
  document
    .querySelectorAll(".tab")
    .forEach((t) => t.classList.toggle("active", t.dataset.tab === tab));
  render();
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
  } catch (e) {
    // Keep stale data on screen if we have any; only blank out on first load.
    if (!data) {
      document.getElementById("content").innerHTML =
        `<div class="empty-state">Failed to load usage.<br/>${esc(e)}</div>`;
      refreshing = false;
      return;
    }
  }
  refreshing = false;
  // The collection just fed the recorder, so the timeline's copy is a sample
  // behind until it re-reads it.
  loadWindowHistory();
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

// ---------- Codex reset credits ----------
function startUseReset(id) {
  confirmingReset = id;
  resetError = null;
  render();
}

function cancelUseReset() {
  confirmingReset = null;
  render();
}

async function confirmUseReset(id) {
  confirmingReset = null;
  resetError = null;
  render();
  try {
    await invoke("consume_codex_reset", { creditId: id });
  } catch (e) {
    resetError = String(e);
  }
  // Pull fresh numbers either way: success drops the spent credit, failure
  // restores the list so the user can retry.
  refresh();
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
  try {
    await invoke("remove_claude_directory", { id });
  } catch (_) {}
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

listen("refresh", refresh);
// Rust collapses the panel whenever it hides, so the next tray click opens the
// familiar popover rather than a full-width window.
listen("collapse", () => {
  if (!panelExpanded) return;
  panelExpanded = false;
  render();
});
// Esc leaves the expanded timeline — the panel has no title bar to close.
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && panelExpanded) setPanelExpanded(false);
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
  if (event.payload) widgetSettings = event.payload;
  if (activeTab === "settings") render();
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
loadNotificationSettings();
loadWidgetSettings();
loadUpdateState();
loadWindowHistory();
refresh();
