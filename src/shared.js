// Helpers used by both windows. The panel and the taskbar widget draw the same
// gauges, so the pace mark in particular has to be computed by one function —
// two copies would drift the moment either is tweaked.

export const esc = (s) =>
  String(s ?? "").replace(
    /[&<>"']/g,
    (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]
  );

export const clamp = (n, lo, hi) => Math.min(hi, Math.max(lo, n));

// How far through its window a gauge is, 0..100. Null when the provider didn't
// report a window length, which is also what a cap with no clock behind it
// (usage credits) gets — there is no "through the window" to show.
// The recorder's key for one quota window — `provider|account|window`, exactly
// as windows_history.rs writes it, so a bar on screen can be matched to the
// history recorded for it. Codex has no account discriminator.
export const historyKey = (provider, account, id) => `${provider}|${account}|${id}`;

// The key for whichever window fills a group ("session", "weekly") on one
// account. The recorder keys by quota id when the provider reports a quota
// list, and by a fixed name when it doesn't (see `usage_windows` in alerts.rs);
// both windows have to resolve that the same way or the widget and the panel
// would look up different records for the same bar.
export function groupKey(provider, account, quotas, group, fallbackId) {
  if (!quotas?.length) return historyKey(provider, account, fallbackId);
  const inGroup = quotas.filter((q) => q.group === group);
  // A group can hold more than one window: the weekly group carries both the
  // all-models limit and any model- or surface-scoped one (the Fable weekly).
  // The bar is showing the unscoped window, so that is the one to match.
  const match = inGroup.find((q) => !q.scope_model && !q.scope_surface) || inGroup[0];
  return match ? historyKey(provider, account, match.id) : null;
}

export function elapsedPercent(g) {
  if (!g || typeof g.resets_at !== "number") return null;
  const span = (Number(g.window_minutes) || 0) * 60000;
  if (span <= 0) return null;
  const end = g.resets_at * 1000;
  return clamp(((Date.now() - (end - span)) / span) * 100, 0, 100);
}

// ---------- account visibility ----------
// The key an account is shown or hidden by, on both surfaces. Claude accounts
// are identified everywhere else by their config directory, so that is what
// this keys on; Codex is a single login with no directory behind it and gets a
// fixed id. Prefixed so the two providers can never collide on a bare path.
export const CODEX_ACCOUNT = "codex";
export const claudeAccount = (id) => `claude:${id}`;

// Whether one account is drawn on one surface ("panel" | "widget"). The stored
// list is of *hidden* ids, so anything the file has never heard of — a folder
// added since it was written — shows by default.
export function accountShown(visibility, surface, id) {
  return !(visibility?.[`${surface}_hidden`] || []).includes(id);
}

// ---------- widget layout ----------
// Split the widget's accounts into its rows. Lives here because the panel
// draws a preview of the same arrangement in Settings — two copies of this rule
// would drift the moment either was tweaked, and the preview would then be
// quietly lying about where things land.
//
// The widget has room for two rows and shares each row's width equally between
// the accounts on it, so an account alone on a row spans the whole widget.
// `pinned` (account id → "top" | "bottom") is therefore the entire layout
// vocabulary: pinning one account to one row and the rest to the other is how
// you give the full width to an account that isn't the one the balanced split
// happens to leave over.
//
// Anything unpinned keeps the flow the widget has always used — ceil(n/2) on
// top — filling in around whatever was pinned. An empty row is dropped rather
// than drawn, so pinning everything to one row is also how you ask for a
// single row of full-height bars.
export function widgetRows(list, pinned = {}) {
  if (list.length <= 1) return [list];
  const rowOf = (a) => pinned?.[a.id];
  const top = new Set(list.filter((a) => rowOf(a) === "top"));
  const bottom = new Set(list.filter((a) => rowOf(a) === "bottom"));
  // Never fewer than what is already pinned there: the pin is the instruction,
  // and the balanced split is only the fallback for everything else.
  const wanted = Math.max(top.size, Math.ceil(list.length / 2));
  for (const a of list) {
    if (rowOf(a)) continue;
    (top.size < wanted ? top : bottom).add(a);
  }
  // Membership is decided above; position inside a row is not. A pin says which
  // row an account belongs on, so Codex-then-Claude reading order still holds
  // within each one rather than pinned accounts jumping to the front.
  return [list.filter((a) => top.has(a)), list.filter((a) => bottom.has(a))].filter(
    (row) => row.length
  );
}
