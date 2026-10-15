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
