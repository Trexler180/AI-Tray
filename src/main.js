const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

let data = null;
let activeTab = "overview";
let notificationSettings = { codex: false, claude: false, codex_resets: false };
// Whether the model-scoped weekly gauge (e.g. the Fable-only limit) is shown.
let showModelWeekly = localStorage.getItem("showModelWeekly") !== "0";
let notifyError = null; // provider whose toggle failed to save, if any
let confirmingReset = null; // credit id awaiting the inline "Use reset" confirm
let resetError = null; // error from the last consume attempt, if any
let editingAccount = null; // account id whose Claude label is being renamed inline
let editingDraft = ""; // in-progress label text, kept across background re-renders
let editFocusPending = false; // focus the rename input once when editing starts
let addingFolder = false; // whether the "add Claude folder" input is open
let addFolderDraft = ""; // in-progress folder path, kept across background re-renders
let addFolderError = null; // error from the last add attempt, if any
let addFolderFocusPending = false; // focus the add-folder input once when opened

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

// ---------- gauge ----------
// usedPercent 0..100; we display the remaining "left".
function gauge(label, usedPercent, resetsIn, opts = {}) {
  usedPercent = Math.min(100, Math.max(0, Number(usedPercent) || 0));
  const left = (100 - usedPercent).toFixed(0);
  const warn = usedPercent >= 80;
  const cls = ["gauge"];
  if (opts.claude) cls.push("claude");
  if (warn) cls.push("warn");
  const fill = Math.min(100, Math.max(2, usedPercent));
  const resetTxt = resetsIn ? `Resets in ${esc(resetsIn)}` : "";
  return `
    <div class="block-label">${esc(label)}</div>
    <div class="${cls.join(" ")}">
      <div class="gauge-bar"><div class="gauge-fill" style="width:${100 - fill}%"></div></div>
      <div class="gauge-meta">
        <span class="l">${left}% left <span class="sub">· ${usedPercent.toFixed(0)}% used</span></span>
        <span class="r">${resetTxt}</span>
      </div>
    </div>`;
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
  return `<div class="chart ${claude ? "claude" : ""}">${bars}</div>
          <div class="chart-cap">Last ${days.length} days</div>`;
}

function costCard(today, todayTok, m30, tok30) {
  return `
    <div class="cost">
      <div class="row"><span class="k">Today</span>
        <span class="v big">${usd(today)} <span class="sub" style="color:var(--faint);font-weight:400">· ${tokens(todayTok)}</span></span></div>
      <div class="row"><span class="k">Last 30 days</span>
        <span class="v">${usd(m30)} <span class="sub" style="color:var(--faint);font-weight:400">· ${tokens(tok30)}</span></span></div>
    </div>`;
}

function equivalentValueLabel(detail, estimate) {
  const confidence = estimate?.confidence || "low";
  const reviewed = estimate?.pricing_reviewed_at
    ? ` Pricing reviewed ${estimate.pricing_reviewed_at}.`
    : "";
  const unknown = (estimate?.unknown_models || []).length
    ? ` Unknown models using fallback pricing: ${estimate.unknown_models.join(", ")}.`
    : "";
  const stale = estimate?.pricing_stale ? " Pricing may be stale." : "";
  const explanation =
    `Not your subscription bill. Estimated from local token logs using API list prices.${reviewed}${unknown}${stale}`;
  const badgeClass = confidence === "high" && !estimate?.pricing_stale ? "high" : confidence;
  return `<div class="block-label value-label">API-equivalent usage value
    <span class="info-tip" tabindex="0" role="img"
      aria-label="${esc(explanation)}" title="${esc(explanation)}">i</span>
    ${detail ? `<span class="value-detail">${esc(detail)}</span>` : ""}
    <span class="confidence ${esc(badgeClass)}" title="${esc(explanation)}">${esc(confidence)} confidence</span>
  </div>`;
}

function notifyToggle(provider, label = "Notify when limits near, hit, or reset") {
  const enabled = !!notificationSettings?.[provider];
  const claude = provider === "claude";
  const err =
    notifyError === provider
      ? `<div class="banner">Couldn't save the notification setting — try again.</div>`
      : "";
  return `
    <label class="notify-row ${claude ? "claude" : ""}">
      <span>${esc(label)}</span>
      <input class="notify-check" type="checkbox" data-notify-provider="${provider}" ${enabled ? "checked" : ""} />
    </label>${err}`;
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

// One reset-credit card: title, granted/expiry dates, and a Use action that
// expands into an inline two-step confirm (no native dialog). Tinted amber when
// it expires within a day.
function resetCard(cr) {
  const soon =
    typeof cr.expires_at === "number" && cr.expires_at * 1000 - Date.now() <= 24 * 3600 * 1000;
  const title = cr.title || "Free rate-limit reset";
  const granted = cr.granted_at ? `Granted ${fmtDate(cr.granted_at)}` : "";
  const expires = cr.expires_at
    ? `Expires ${fmtDate(cr.expires_at)}${cr.expires_in ? ` · ${esc(cr.expires_in)} left` : ""}`
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
      ${granted ? `<div class="reset-meta">${esc(granted)}</div>` : ""}
      ${expires ? `<div class="reset-meta ${soon ? "soon" : ""}">${expires}</div>` : ""}
    </div>
    ${action}
  </div>`;
}

// The "Reset credits" section for the Codex tab: a notify toggle plus a card
// per available credit (or a quiet empty line).
function resetSection(c) {
  const r = c.resets;
  const credits = availableResets(c);
  let html = `<div class="block-label reset-block">Reset credits${
    credits.length ? ` <span class="pill">${credits.length}</span>` : ""
  }</div>`;
  html += notifyToggle("codex_resets", "Notify about reset credits");
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
    return `<div class="sec-head"><div><div class="sec-title">Codex</div></div></div>
      ${notifyToggle("codex")}
      <div class="empty-state">No Codex sessions found.<br/>Looked in <code>~/.codex/sessions</code>.</div>`;

  let html = `<div class="sec-head">
      <div><div class="sec-title">Codex</div></div>
      ${c.plan_type ? `<span class="pill">${esc(c.plan_type)}</span>` : ""}
    </div>`;
  html += notifyToggle("codex");

  if (c.primary)
    html += gauge("Session (5h)", c.primary.used_percent, c.primary.resets_in);
  if (c.secondary)
    html += gauge("Weekly", c.secondary.used_percent, c.secondary.resets_in);
  if (!c.live)
    html += `<div class="banner">Live usage unavailable — showing the last numbers from local session logs.</div>`;

  if (typeof c.credits === "number") {
    html += `<div class="divider"></div>
      <div class="row"><span class="k">Credits</span><span class="v">${esc(c.credits.toFixed(2))}</span></div>`;
  }

  html += resetSection(c);

  html += equivalentValueLabel("estimated", c.estimate);
  html += costCard(c.cost_today, c.tokens_today, c.cost_30d, c.tokens_30d);
  html += chart(c.daily, false);
  return html;
}

// One account's 5h + weekly gauges. `sessionLabel` differs between the
// detailed tab ("Session (5h)") and the compact overview card ("Session").
function claudeGauges(acct, sessionLabel) {
  let h = "";
  if (acct.five_hour)
    h += gauge(sessionLabel, acct.five_hour.used_percent, acct.five_hour.resets_in, { claude: true });
  if (acct.seven_day)
    h += gauge("Weekly", acct.seven_day.used_percent, acct.seven_day.resets_in, { claude: true });
  const mg = acct.seven_day_model;
  if (mg && showModelWeekly)
    h += gauge(`Weekly (${mg.model})`, mg.gauge.used_percent, mg.gauge.resets_in, { claude: true });
  return h;
}

// Toggle for the model-scoped weekly gauge. Only rendered when at least one
// account actually reports one, so it never shows as dead UI.
function modelWeeklyToggle(accounts) {
  const models = [...new Set(
    (accounts || []).filter((a) => a.seven_day_model).map((a) => a.seven_day_model.model)
  )];
  if (!models.length) return "";
  return `
    <label class="notify-row claude">
      <span>Show ${esc(models.join(" / "))} weekly limit</span>
      <input class="notify-check" type="checkbox" data-model-weekly ${showModelWeekly ? "checked" : ""} />
    </label>`;
}

// Rename/remove controls. Remove is shown only for user-added folders; the
// built-in ~/.claude account can't be removed.
function acctActions(a) {
  const rename = `<button class="acct-btn" data-rename="${esc(a.id)}" title="Rename">✎</button>`;
  const remove = a.removable
    ? `<button class="acct-btn" data-remove="${esc(a.id)}" title="Remove this folder">✕</button>`
    : "";
  return `<div class="acct-actions">${rename}${remove}</div>`;
}

function renameField() {
  return `<input class="acct-input" data-rename-input type="text" maxlength="40"
      value="${esc(editingDraft)}" placeholder="Account name" />
    <div class="acct-actions">
      <button class="acct-btn save" data-rename-save="${esc(editingAccount)}" title="Save">✓</button>
      <button class="acct-btn" data-rename-cancel title="Cancel">✕</button>
    </div>`;
}

// One account block: a header (name + default pill + controls, or the inline
// rename field), the folder path, then its live gauges.
function renderClaudeAccount(a, multi) {
  const editing = editingAccount === a.id;
  const name = editing
    ? renameField()
    : `<div class="acct-name">
        <span class="ov-dot claude"></span>
        <span class="acct-label" data-rename="${esc(a.id)}" title="Rename">${esc(a.label)}</span>
        ${a.active ? `<span class="pill">default</span>` : ""}
      </div>${acctActions(a)}`;
  // Show the folder each account maps to once more than one is configured.
  const path = multi && !editing ? `<div class="acct-path">${esc(a.id)}</div>` : "";
  const body = a.live
    ? claudeGauges(a, "Session (5h)")
    : `<div class="banner small">Live usage unavailable — open Claude Code signed in as this account to refresh it.</div>`;
  return `<div class="acct" data-acct="${esc(a.id)}">
    <div class="acct-head">${name}</div>${path}${body}</div>`;
}

// "Add Claude folder" control: a button that expands into a path input. Lets a
// user track a second login kept in a separate CLAUDE_CONFIG_DIR folder.
function addFolderControl() {
  if (!addingFolder) {
    return `<button class="add-folder-btn" data-add-folder-open>+ Add Claude folder</button>`;
  }
  const err = addFolderError
    ? `<div class="banner small">${esc(addFolderError)}</div>`
    : "";
  return `<div class="add-folder">
      <input class="acct-input" data-add-folder-input type="text"
        value="${esc(addFolderDraft)}" placeholder="Path to a Claude config folder" />
      <div class="acct-actions">
        <button class="acct-btn save" data-add-folder-save title="Add">✓</button>
        <button class="acct-btn" data-add-folder-cancel title="Cancel">✕</button>
      </div>
    </div>
    <div class="sec-sub" style="margin:4px 0 0">A folder containing its own <code>.credentials.json</code> (e.g. a second <code>CLAUDE_CONFIG_DIR</code>).</div>
    ${err}`;
}

function renderClaude() {
  const c = data.claude;
  if (!c.available)
    return `<div class="sec-head"><div class="sec-title">Claude</div></div>
      ${notifyToggle("claude")}
      <div class="empty-state">No Claude data.<br/>Sign in with Claude Code, or check <code>~/.claude</code>.</div>`;

  const accounts = c.accounts || [];
  const multi = accounts.length > 1;

  let html = `<div class="sec-head"><div class="sec-title">Claude</div>
    <span class="pill">${c.live ? "live" : "logs only"}</span></div>`;
  html += notifyToggle("claude");
  html += modelWeeklyToggle(c.accounts);

  if (accounts.length) {
    for (const a of accounts) html += renderClaudeAccount(a, multi);
  } else {
    html += `<div class="banner">Live usage unavailable — token expired or offline. Open Claude Code to refresh it. Showing estimated cost from logs.</div>`;
  }
  html += addFolderControl();

  html += equivalentValueLabel("estimated from logs", c.estimate);
  if (multi)
    html += `<div class="sec-sub" style="margin:-4px 0 8px">Combined across all accounts — local logs aren't per-account.</div>`;
  html += costCard(c.cost_today, c.tokens_today, c.cost_30d, c.tokens_30d);
  html += chart(c.daily, true);
  return html;
}

function renderOverview() {
  const cx = data.codex,
    cl = data.claude;
  let html = `<div class="sec-head"><div class="sec-title">Overview</div>
    <span class="sec-sub">${usd(cx.cost_today + cl.cost_today)} today</span></div>`;

  // Codex card
  html += `<div class="ov-card" data-goto="codex">
    <div class="ov-head">
      <div class="ov-name"><span class="ov-dot"></span>Codex ${cx.plan_type ? `<span class="pill">${esc(cx.plan_type)}</span>` : ""}</div>
      <span class="ov-cost">${usd(cx.cost_today)} today</span>
    </div>`;
  if (cx.primary)
    html += gauge("Session", cx.primary.used_percent, cx.primary.resets_in);
  if (cx.secondary)
    html += gauge("Weekly", cx.secondary.used_percent, cx.secondary.resets_in);
  if (!cx.available) html += `<div class="sec-sub">No data</div>`;
  const resetCount = availableResets(cx).length;
  if (resetCount)
    html += `<div class="ov-reset">↻ ${resetCount} reset credit${
      resetCount > 1 ? "s" : ""
    } available</div>`;
  html += `</div>`;

  // Claude card
  html += `<div class="ov-card" data-goto="claude">
    <div class="ov-head">
      <div class="ov-name"><span class="ov-dot claude"></span>Claude</div>
      <span class="ov-cost">${usd(cl.cost_today)} today</span>
    </div>`;
  const accounts = cl.accounts || [];
  const multi = accounts.length > 1;
  if (accounts.length) {
    for (const a of accounts) {
      if (multi)
        html += `<div class="ov-acct"><span class="ov-dot claude"></span>${esc(a.label)}${
          a.active ? ` <span class="pill">active</span>` : ""
        }</div>`;
      if (a.live) html += claudeGauges(a, "Session");
      else html += `<div class="sec-sub">Live unavailable — open Claude Code</div>`;
    }
  } else {
    html += `<div class="sec-sub">Live unavailable — open Claude Code</div>`;
  }
  html += `</div>`;

  return html;
}

function render() {
  const content = document.getElementById("content");
  if (!data) {
    content.innerHTML = `<div class="loading">Loading usage…</div>`;
    fitWindowHeight();
    return;
  }
  if (activeTab === "codex") content.innerHTML = renderCodex();
  else if (activeTab === "claude") content.innerHTML = renderClaude();
  else content.innerHTML = renderOverview();

  content.querySelectorAll("[data-goto]").forEach((card) =>
    card.addEventListener("click", () => switchTab(card.dataset.goto))
  );
  content.querySelectorAll("[data-notify-provider]").forEach((input) =>
    input.addEventListener("change", () => setNotifyEnabled(input.dataset.notifyProvider, input.checked))
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

  // Per-account rename / forget controls.
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

  const updated = document.getElementById("updated");
  if (data.generated_at) {
    const d = new Date(data.generated_at * 1000);
    updated.textContent = "Updated " + d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  }
  fitWindowHeight();
}

function fitWindowHeight() {
  requestAnimationFrame(() => {
    const tabs = document.getElementById("tabs");
    const content = document.getElementById("content");
    const foot = document.querySelector(".foot");
    if (!tabs || !content || !foot) return;

    const contentBox = content.getBoundingClientRect();
    const childBottom = [...content.children].reduce((bottom, child) => {
      return Math.max(bottom, child.getBoundingClientRect().bottom - contentBox.top);
    }, 0);
    const contentStyle = getComputedStyle(content);
    const contentHeight =
      childBottom + Number.parseFloat(contentStyle.paddingBottom || "0") + 2;
    const height = tabs.offsetHeight + contentHeight + foot.offsetHeight + 2;
    invoke("fit_window_height", { height }).catch(() => {});
  });
}

function switchTab(tab) {
  activeTab = tab;
  document
    .querySelectorAll(".tab")
    .forEach((t) => t.classList.toggle("active", t.dataset.tab === tab));
  render();
}

let refreshing = false;
async function refresh() {
  if (refreshing) return;
  refreshing = true;
  const btn = document.getElementById("refreshBtn");
  btn.classList.add("spin");
  setTimeout(() => btn.classList.remove("spin"), 600);
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
    render();
  } catch (e) {
    // Keep stale data on screen if we have any; only blank out on first load.
    if (!data)
      document.getElementById("content").innerHTML =
        `<div class="empty-state">Failed to load usage.<br/>${esc(e)}</div>`;
  } finally {
    refreshing = false;
  }
}

async function loadNotificationSettings() {
  try {
    notificationSettings = await invoke("get_notification_settings");
    render();
  } catch (_) {}
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
document.getElementById("refreshBtn").addEventListener("click", refresh);

listen("refresh", refresh);
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
refresh();
