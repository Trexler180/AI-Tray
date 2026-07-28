# AI Usage Tray

A Windows system-tray app that shows Claude and Codex quota usage, reset times,
and an API-equivalent value estimate in a compact popover.

- **Overview** — both providers at a glance and today's combined estimated value.
- **Codex** — live quota windows, plan and credit details, reset credits, and
  model-aware local token history.
- **Claude** — live account and scoped quota windows plus model-aware local
  Claude Code history. Multiple Claude config directories are supported.

## What the numbers mean

Quota gauges come from the providers' authenticated usage endpoints. The app
discovers every returned quota window instead of assuming that only five-hour
and weekly limits exist. Model- or surface-specific windows are labeled and can
be shown or hidden in the popover.

The currency figure is labeled **API-equivalent usage value** deliberately. It
applies public API list prices to tokens found in local logs; it is not a bill,
subscription charge, or claim about the provider's internal cost. Cached input
is treated as a subset of input rather than counted twice.

Each estimate includes a confidence level:

- **High** — the model has an exact current catalog entry.
- **Medium** — a documented model-family price was used.
- **Low** — the model was unknown and the conservative fallback was used, or
  the catalog is past its review date.

Pricing is intentionally a small, manually reviewed catalog at
[`src-tauri/src/pricing_catalog.json`](src-tauri/src/pricing_catalog.json).
Release 1 does not fetch pricing automatically. If the catalog becomes stale,
the app keeps using its last-known-good values and visibly lowers confidence.
Update the catalog version, review dates, sources, and rate cards together when
provider pricing changes.

## Where the data comes from

- **Live quota data** uses the same authenticated endpoints as the official
  CLIs, with credentials already stored in `~/.claude/.credentials.json` and
  `~/.codex/auth.json`. Rotated access tokens are written back atomically.
- **Codex history** comes from both
  `CODEX_HOME/sessions/**/rollout-*.jsonl` and
  `CODEX_HOME/archived_sessions/**/rollout-*.jsonl`. If `CODEX_HOME` is unset,
  `~/.codex` is used. Usage is attributed to each event's timestamp, not the
  directory date.
- **Claude history** comes from `~/.claude/projects/**/*.jsonl` (and the
  equivalent directory for each configured Claude account).
- **Data health** is an expandable row in the existing popover. It identifies
  live API, in-memory cache, local-log, and unavailable results; shows
  freshness, scan counts, errors, and pricing review metadata; and can copy a
  sanitized diagnostic summary.

Live endpoint failures do not silently masquerade as current data. A recent
in-memory result may be shown with its age and the failed attempt, while local
history remains separately identified.

## Incremental history cache

History scans maintain versioned, provider-separated indexes in the Windows
configuration directory:

```text
%APPDATA%\AI Usage Tray\codex-history-cache.json
%APPDATA%\AI Usage Tray\claude-history-cache.json
```

The scanner reuses unchanged files and reads only newly appended, complete
JSONL records. Files changed in place or truncated are rescanned. Cache writes
use atomic replacement, and an unreadable or older cache version is ignored and
rebuilt. The cache stores token facts rather than currency totals, so a pricing
catalog update reprices history without forcing a log rescan.

To rebuild the indexes, expand either local-history Data health row and choose
**Clear scan cache**. This removes only the two derived cache files; source
logs, credentials, settings, and provider data are untouched. The subsequent
refresh rebuilds them.

## Behavior

- Left-click the tray icon to toggle the panel; it auto-hides on blur and is
  clamped to the monitor work area.
- Right-click the tray icon for Refresh or Quit.
- The panel refreshes when opened and every 60 seconds while visible.
- When notifications are enabled, a lightweight background refresh checks
  quota thresholds approximately every five minutes even while the panel is
  hidden. Local usage-file changes also schedule a refresh.
- The app starts on login by registering itself under
  `HKCU\...\CurrentVersion\Run`. Disable it in Task Manager's Startup apps.
- A second launch opens the existing single instance.
- Windows acrylic and native rounded corners gracefully fall back to a solid
  panel when unavailable.

## Stack and prerequisites

Tauri v2 with a Rust backend and vanilla JavaScript/Vite frontend.

- Node.js 20.19+ or 22.12+ (Vite 8's requirement) and npm
- Rust (`rustup`) with the MSVC target
- WebView2 runtime and MSVC C++ Build Tools

## Setup and run

```powershell
npm install
npm run tauri dev
```

Icons are checked in under `src-tauri/icons/`. To regenerate them:

```powershell
node scripts/gen-icon.mjs
npx tauri icon src-tauri/icons/source.png
```

## Build

```powershell
# Release executable only
npm run tauri build -- --no-bundle

# Release executable plus MSI/NSIS installers
npm run tauri build
```

## Tests

```powershell
cargo test --manifest-path src-tauri/Cargo.toml
```

The test suite includes sanitized provider-contract fixtures, pricing and
cached-input accounting, dynamic scoped quotas, provenance fallback behavior,
active and archived Codex history, event-date attribution, unchanged and
append-only scan paths, and atomic cache replacement.

## Updates

The app checks GitHub Releases for a newer signed build about a minute after
launch and every six hours after that, and tells you when one is found. It does
not install anything on its own unless you ask it to. Both behaviors are toggles
in **Settings → About**, and the tray right-click menu has a **Check for
updates…** item for an immediate check.

Updates are verified against a public key compiled into the app, so a release
that wasn't signed with the matching private key is refused. That protects the
integrity of the update itself; it is not a Microsoft code-signing certificate,
so Windows SmartScreen still shows its "unrecognized app" prompt during install.

### Cutting a release

```powershell
npm version minor        # bumps package.json, syncs Cargo.toml, commits, tags
git push --follow-tags   # the tag triggers .github/workflows/release.yml
```

`package.json` is the single source of truth for the version:
`tauri.conf.json` reads it via `"version": "../package.json"`, and
`scripts/sync-version.mjs` keeps `src-tauri/Cargo.toml` in step.

CI builds and signs on `windows-latest` and creates a **draft** release.
Installed apps only see the update once that draft is published — the manual
publish step is deliberate, because an auto-updating app otherwise distributes a
broken build to every install with no way to recall it.

Building signed artifacts locally needs the private key:

```powershell
$env:TAURI_SIGNING_PRIVATE_KEY_PATH = "$env:USERPROFILE\.tauri\ai-usage-tray.key"
$env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = ""
npm run tauri build
```

## License

MIT — see [`LICENSE`](LICENSE).

The app reads provider credentials that already exist on your machine and sends
them only to the providers' own endpoints. It has no telemetry and no backend of
its own.
