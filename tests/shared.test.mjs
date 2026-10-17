// The helpers both windows draw from. These are the rules the panel's Settings
// preview and the widget itself must agree on, so they are worth pinning down
// in one place.

import { test } from "node:test";
import assert from "node:assert/strict";
import {
  accountShown,
  codexReserve,
  elapsedPercent,
  esc,
  groupKey,
  historyKey,
  meterTier,
  runsOutAt,
  tightestLimit,
  widgetRows,
} from "../src/shared.js";

test("esc neutralises every character that matters in HTML and attributes", () => {
  assert.equal(esc(`<img src=x onerror="a('b')">&`), "&lt;img src=x onerror=&quot;a(&#39;b&#39;)&quot;&gt;&amp;");
  assert.equal(esc(null), "");
  assert.equal(esc(undefined), "");
  assert.equal(esc(42), "42");
});

const accounts = (...ids) => ids.map((id) => ({ id }));
const ids = (rows) => rows.map((row) => row.map((a) => a.id));

test("widgetRows flows ceil(n/2) accounts onto the top row", () => {
  assert.deepEqual(ids(widgetRows(accounts("codex"))), [["codex"]]);
  assert.deepEqual(ids(widgetRows(accounts("codex", "a", "b"))), [["codex", "a"], ["b"]]);
  assert.deepEqual(ids(widgetRows(accounts("codex", "a", "b", "c"))), [["codex", "a"], ["b", "c"]]);
});

test("widgetRows keeps pinned accounts on their row, in reading order", () => {
  assert.deepEqual(ids(widgetRows(accounts("codex", "a", "b"), { b: "top" })), [["codex", "b"], ["a"]]);
  // Alone on a row is how an account gets the full width.
  assert.deepEqual(
    ids(widgetRows(accounts("codex", "a", "b"), { codex: "top", a: "bottom", b: "bottom" })),
    [["codex"], ["a", "b"]]
  );
});

test("widgetRows drops a row left empty", () => {
  assert.deepEqual(
    ids(widgetRows(accounts("codex", "a"), { codex: "bottom", a: "bottom" })),
    [["codex", "a"]]
  );
});

const gauge = (used) => ({ used_percent: used });

test("codexReserve is null without a reserve allowance", () => {
  assert.equal(codexReserve({ quotas: [{ group: "session", gauge: gauge(10) }] }), null);
  assert.equal(codexReserve(null), null);
});

test("codexReserve says when the main limit is drained and picks the fullest reserve window", () => {
  const cx = {
    quotas: [
      { id: "session", group: "session", gauge: gauge(100) },
      { id: "r:session", group: "reserve", gauge: gauge(20) },
      { id: "r:weekly", group: "reserve", gauge: gauge(60) },
    ],
  };
  const reserve = codexReserve(cx);
  assert.equal(reserve.drained, true);
  assert.equal(reserve.spent, false);
  assert.equal(reserve.primary.id, "r:weekly");

  cx.quotas[0].gauge = gauge(40);
  assert.equal(codexReserve(cx).drained, false);
});

test("elapsedPercent needs both a reset time and a window length", () => {
  assert.equal(elapsedPercent(null), null);
  assert.equal(elapsedPercent({ window_minutes: 300 }), null);
  assert.equal(elapsedPercent({ resets_at: 1, window_minutes: 0 }), null);
  const halfway = { resets_at: Date.now() / 1000 + 150 * 60, window_minutes: 300 };
  assert.ok(Math.abs(elapsedPercent(halfway) - 50) < 1);
  // Clamped once the window is over.
  assert.equal(elapsedPercent({ resets_at: Date.now() / 1000 - 60, window_minutes: 300 }), 100);
});

test("accountShown reads the hidden list, so unknown accounts show", () => {
  const visibility = { panel_hidden: ["codex"], widget_hidden: [] };
  assert.equal(accountShown(visibility, "panel", "codex"), false);
  assert.equal(accountShown(visibility, "widget", "codex"), true);
  assert.equal(accountShown(visibility, "panel", "claude:new"), true);
  assert.equal(accountShown(undefined, "panel", "codex"), true);
});

test("groupKey matches the unscoped window in a group, as the recorder keys it", () => {
  const quotas = [
    { id: "weekly_scoped:fable", group: "weekly", scope_model: "Fable" },
    { id: "weekly_all", group: "weekly" },
  ];
  assert.equal(groupKey("claude", "dir", quotas, "weekly", "weekly"), historyKey("claude", "dir", "weekly_all"));
  // No quota list: the fixed name `usage_windows` falls back to.
  assert.equal(groupKey("codex", "", [], "session", "session"), "codex||session");
  assert.equal(groupKey("codex", "", quotas, "session", "session"), null);
});

// The same cases forecast.rs is tested against: the tray icon and the panel
// must agree on which window is about to run out.
const NOW = 1_800_000_000_000;
// A five-hour window `elapsed` of the way through, `used` percent spent.
const session = (used, elapsed) => ({
  used_percent: used,
  window_minutes: 300,
  resets_at: NOW / 1000 + Math.trunc((1 - elapsed) * 300 * 60),
});

test("runsOutAt flags a window spending faster than it refills", () => {
  const g = session(86, 0.81);
  const at = runsOutAt(g, NOW);
  assert.ok(at !== null && at < g.resets_at);
  assert.ok(at - NOW / 1000 > 2300 && at - NOW / 1000 < 2500);
  assert.equal(meterTier(g, NOW), "crit");
});

test("a high window that will last is only low", () => {
  assert.equal(runsOutAt(session(86, 0.92), NOW), null);
  assert.equal(meterTier(session(86, 0.92), NOW), "low");
});

test("early or light windows are not forecast", () => {
  assert.equal(runsOutAt(session(60, 0.016), NOW), null);
  assert.equal(runsOutAt(session(40, 0.2), NOW), null);
  assert.equal(meterTier(session(40, 0.2), NOW), "ok");
});

test("meterTier follows the thresholds", () => {
  assert.equal(meterTier(session(96, 0.99), NOW), "crit");
  assert.equal(meterTier(session(100, 0.5), NOW), "out");
  assert.equal(meterTier({ used_percent: 85 }, NOW), "low");
});

test("tightestLimit picks the worst tier, then the least left", () => {
  const limits = [
    { id: "calm", gauge: session(30, 0.5) },
    { id: "low", gauge: session(86, 0.92) },
    { id: "runs-out", gauge: session(86, 0.81) },
    { id: "low-higher", gauge: session(90, 0.95) },
  ];
  assert.equal(tightestLimit(limits, NOW).id, "runs-out");
  assert.equal(tightestLimit(limits.filter((l) => l.id !== "runs-out"), NOW).id, "low-higher");
  assert.equal(tightestLimit([], NOW), null);
});

test("tightestLimit puts the soonest run-out first, whatever its percentage", () => {
  // A weekly window 91% used three days in runs out days from now; a session
  // 86% used near its end runs out in about 40 minutes.
  const weekly = {
    used_percent: 91,
    window_minutes: 10080,
    resets_at: NOW / 1000 + 4 * 86400,
  };
  const limits = [
    { id: "weekly", gauge: weekly },
    { id: "session", gauge: session(86, 0.81) },
  ];
  assert.equal(meterTier(weekly, NOW), "crit");
  assert.equal(tightestLimit(limits, NOW).id, "session");
});
