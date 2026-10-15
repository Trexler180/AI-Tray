// The app's command ACL has three halves that have to agree, and nothing at
// build time checks the last one:
//
// - `generate_handler!` in lib.rs registers the commands,
// - `COMMANDS` in build.rs gives each one an `allow-<command>` permission,
// - each window's capability grants the commands that window calls.
//
// A command missing from build.rs, or from a window's capability, isn't a
// build error — the call is just refused at runtime with "not allowed". These
// checks are what turn that into a failing test instead.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const read = (path) => readFileSync(new URL(`../${path}`, import.meta.url), "utf8");

function handlerCommands() {
  const block = read("src-tauri/src/lib.rs").match(/generate_handler!\[([\s\S]*?)\]/)?.[1];
  assert.ok(block, "generate_handler! not found in lib.rs");
  return block
    .split(",")
    .map((entry) => entry.trim())
    .filter(Boolean)
    .map((entry) => entry.split("::").pop());
}

function manifestCommands() {
  const block = read("src-tauri/build.rs").match(/const COMMANDS: &\[&str\] = &\[([\s\S]*?)\];/)?.[1];
  assert.ok(block, "COMMANDS not found in build.rs");
  return [...block.matchAll(/"([a-z_]+)"/g)].map((m) => m[1]);
}

const permissionFor = (command) => `allow-${command.replaceAll("_", "-")}`;

// App permissions only: plugin and core ones carry a `name:` prefix.
function grantedAppPermissions(capability) {
  const { permissions } = JSON.parse(read(`src-tauri/capabilities/${capability}.json`));
  return new Set(permissions.filter((p) => typeof p === "string" && !p.includes(":")));
}

// Every registered command a window's code names as a string literal. That
// covers `invoke("x")` and the command tables some loaders loop over, and errs
// towards demanding a permission rather than missing one.
function commandsNamedIn(files, commands) {
  const text = files.map(read).join("\n");
  const literals = new Set([...text.matchAll(/["'`]([a-z_]+)["'`]/g)].map((m) => m[1]));
  return commands.filter((command) => literals.has(command));
}

const WINDOWS = [
  { capability: "default", files: ["src/main.js", "src/shared.js"] },
  { capability: "widget", files: ["src/widget.js", "src/shared.js"] },
];

test("build.rs lists exactly the commands lib.rs registers", () => {
  assert.deepEqual([...manifestCommands()].sort(), [...handlerCommands()].sort());
});

for (const { capability, files } of WINDOWS) {
  test(`the ${capability} capability grants every command its window calls`, () => {
    const granted = grantedAppPermissions(capability);
    const missing = commandsNamedIn(files, handlerCommands()).filter(
      (command) => !granted.has(permissionFor(command))
    );
    assert.deepEqual(missing, [], `add to capabilities/${capability}.json`);
  });

  test(`the ${capability} capability grants nothing its window doesn't call`, () => {
    const used = new Set(commandsNamedIn(files, handlerCommands()).map(permissionFor));
    const extra = [...grantedAppPermissions(capability)].filter((p) => !used.has(p));
    assert.deepEqual(extra, [], `remove from capabilities/${capability}.json`);
  });
}

test("the widget can't reach anything that spends, installs or reconfigures", () => {
  const granted = grantedAppPermissions("widget");
  for (const command of [
    "consume_codex_reset",
    "consume_claude_reset",
    "install_update",
    "add_claude_directory",
    "remove_claude_directory",
    "set_start_on_login",
  ]) {
    assert.ok(!granted.has(permissionFor(command)), command);
  }
});
