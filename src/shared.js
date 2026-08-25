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
