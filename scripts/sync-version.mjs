// Copy the version from package.json into src-tauri/Cargo.toml.
//
// package.json is the single source of truth: tauri.conf.json reads it directly
// via `"version": "../package.json"`, and this script keeps the Rust crate in
// step. Run automatically by the npm `version` lifecycle hook, so
// `npm version minor` bumps everything in one command.

import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { execFileSync } from "node:child_process";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const cargoToml = join(root, "src-tauri", "Cargo.toml");

const { version } = JSON.parse(readFileSync(join(root, "package.json"), "utf8"));
if (!/^\d+\.\d+\.\d+/.test(version)) {
  console.error(`refusing to sync a non-semver version: ${version}`);
  process.exit(1);
}

const manifest = readFileSync(cargoToml, "utf8");

// Only the [package] version, which is the first `version = "..."` in the file.
// Dependency versions further down must not be touched.
let replaced = false;
const updated = manifest.replace(/^version = "[^"]*"$/m, () => {
  replaced = true;
  return `version = "${version}"`;
});

if (!replaced) {
  console.error(`could not find a [package] version line in ${cargoToml}`);
  process.exit(1);
}

if (updated !== manifest) {
  writeFileSync(cargoToml, updated);
  console.log(`synced Cargo.toml to ${version}`);
} else {
  console.log(`Cargo.toml already at ${version}`);
}

// Refresh Cargo.lock so the bumped package version is recorded there too,
// otherwise the next build dirties the tree right after the release commit.
try {
  execFileSync("cargo", ["update", "--workspace", "--offline"], {
    cwd: join(root, "src-tauri"),
    stdio: "ignore",
  });
} catch {
  // Non-fatal: the next build regenerates it. Warn rather than break the bump.
  console.warn("could not refresh Cargo.lock; the next build will update it");
}
