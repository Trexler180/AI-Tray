const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

let data = null;
let activeTab = "overview";
let creditsOpen = false; // the Credits detail screen, layered over any tab
let notificationSettings = { codex: false, claude: false, codex_resets: false };
// Whether the model-scoped weekly gauge (e.g. the Fable-only limit) is shown.
let showModelWeekly = localStorage.getItem("showModelWeekly") !== "0";
// Meter direction: false (default) drains a full bar as the allowance is
// spent; true fills an empty bar instead.
let meterFillsUp = localStorage.getItem("meterFillsUp") === "1";
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

// ---------- formatting helpers ----------
const usd = (n) => "$" + (Number.isFinite(n) ? n : 0).toFixed(2);
function tokens(n) {
  n = n || 0;
  if (n >= 1e9) return (n / 1e9).toFixed(2) + "B";
  if (n >= 1e6) return (n / 1e6).toFixed(1) + "M";
  if (n >= 1e3) return (n / 1e3).toFixed(1) + "K";
  return String(n);
}
// Escape anything that came from logs or APIs before it touches innerHTML.
const esc = (s) =>
  String(s ?? "").replace(
    /[&<>"']/g,
    (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]
  );

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
function meterBar(cls, usedPct, leftHtml, rightHtml, tip, attrs = "") {
  const used = Math.min(100, Math.max(0, usedPct));
  const fill = meterFillsUp ? used : 100 - used;
  // Anything that rounds to 0% in the headline renders as a bare track: the
  // fill's edge line would otherwise leave a 2px sliver reading as "nearly
  // empty" when the meter is empty.
  const empty = fill < 0.5;
  return `
    <div class="bgauge ${cls}" title="${tip}" ${attrs}>
      <div class="bfill${empty ? " empty" : ""}" style="width:${empty ? 0 : fill}%"></div>
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
  const tip = `${usedPercent.toFixed(0)}% used · ${left}% left${
    g.resets_in ? ` · resets in ${esc(g.resets_in)}` : ""
  }`;
  const headline = meterFillsUp
    ? `<b>${usedPercent.toFixed(0)}%</b><span class="sub"> used</span>`
    : `<b>${left}%</b><span class="sub"> left</span>`;
  return meterBar(cls, usedPercent, `${esc(label)} ${headline}`, resetText(g), tip);
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
function header(title, extras = "", opts = {}) {
  const stamp = data?.generated_at ? clockTime(data.generated_at) : "—";
  const busy = spinning() ? " busy" : "";
  const right = opts.hideRefresh
    ? `<span class="hd-right"><span>${esc(opts.rightText || "")}</span></span>`
    : `<span class="hd-right"><span title="Last updated">${esc(stamp)}</span>
        <button class="hd-refresh${busy}" id="refreshBtn" title="Refresh"
          aria-label="Refresh">${refreshIcon()}</button></span>`;
  return `<div class="hd"><span class="hd-title">${esc(title)}</span>${extras}${right}</div>`;
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
    html += gauge("Session (5h)", c.primary);
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
      if (a.live) html += claudeGauges(a, "Session");
      else html += `<div class="sec-sub">Live unavailable — open Claude Code</div>`;
    }
    html += overviewCreditsBar(accounts);
  } else {
    html += `<div class="sec-sub">Live unavailable — open Claude Code</div>`;
  }
  html += `</div>`;

  return html;
}

// One combined credits meter across every account with credits switched on.
function overviewCreditsBar(accounts) {
  const on = accounts.filter(creditsActive);
  if (!on.length) return "";
  const cap = on.reduce((sum, a) => sum + a.extra_usage.monthly_limit, 0);
  const used = on.reduce(
    (sum, a) => sum + Math.min(a.extra_usage.monthly_limit, a.extra_usage.used_credits || 0),
    0
  );
  const usedPct = cap > 0 ? Math.min(100, (used / cap) * 100) : 0;
  const cls = ["credit", usedPct >= 80 ? "warn" : "", "link"].join(" ").trim();
  const headline = meterFillsUp
    ? `<b>${usd(used)}</b><span class="sub"> spent this month</span>`
    : `<b>${usd(Math.max(0, cap - used))}</b><span class="sub"> left this month</span>`;
  return meterBar(
    cls,
    usedPct,
    `Credits ${headline}`,
    `${usedPct.toFixed(0)}% used <span class="go">›</span>`,
    `${usd(used)} of ${usd(cap)} across ${on.length} account${on.length > 1 ? "s" : ""}`,
    "data-credits"
  );
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
  if (creditsOpen) content.innerHTML = renderCredits();
  else if (activeTab === "codex") content.innerHTML = renderCodex();
  else if (activeTab === "claude") content.innerHTML = renderClaude();
  else if (activeTab === "settings") content.innerHTML = renderSettings();
  else content.innerHTML = renderOverview();

  const refreshBtn = document.getElementById("refreshBtn");
  if (refreshBtn) refreshBtn.addEventListener("click", refresh);

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
  content.querySelectorAll("[data-meter-fill]").forEach((input) =>
    input.addEventListener("change", () => {
      meterFillsUp = input.checked;
      try {
        localStorage.setItem("meterFillsUp", input.checked ? "1" : "0");
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
  creditsOpen = false;
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
// Rust pushes the updater status on every transition, so the About section
// tracks a background check without polling.
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
loadUpdateState();
refresh();
