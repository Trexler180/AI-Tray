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
export function elapsedPercent(g) {
  if (!g || typeof g.resets_at !== "number") return null;
  const span = (Number(g.window_minutes) || 0) * 60000;
  if (span <= 0) return null;
  const end = g.resets_at * 1000;
  return clamp(((Date.now() - (end - span)) / span) * 100, 0, 100);
}
