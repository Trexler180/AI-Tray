# AI Usage Tray

A Windows system-tray app that shows your **Claude** and **Codex** usage in a
beautiful pop-over — inspired by Theo's CodexBar.

- **Overview tab** — both providers at a glance, with combined cost today.
- **Codex tab** — live rate-limit gauges (5h session + weekly), plan type,
  credits, and an estimated-cost view.
- **Claude tab** — live 5h/weekly gauges, plus a token/cost estimate built
  from your local Claude Code logs.

## Where the data comes from

- **Live gauges** are fetched from the same usage endpoints the official CLIs
  use, authenticated with the tokens already on disk
  (`~/.claude/.credentials.json`, `~/.codex/auth.json`). Expired access tokens
  are refreshed automatically, and the rotated tokens are written back
  atomically so the CLIs keep working.
- **Cost / token history** is estimated locally from
  `~/.claude/projects/**/*.jsonl` and `~/.codex/sessions/**/rollout-*.jsonl`.
  Files untouched for more than the 30-day window are skipped, so refreshes
  stay fast no matter how much history accumulates.
- When the network (or a token) is unavailable, the app falls back to
  log-based numbers and says so in the panel.

## Behavior

- **Left-click** the tray icon to toggle the panel; it auto-hides on blur.
  The panel opens next to the tray icon and is clamped to the monitor's work
  area, so it rests on the taskbar instead of covering it (top/side taskbars
  work too).
- **Right-click** the tray icon for Refresh / Quit.
- Data refreshes when the panel opens and every 60s while it stays visible;
  nothing polls while it's hidden.
- **Starts on login** — the app registers itself under
  `HKCU\...\CurrentVersion\Run` on every launch, so the entry follows the exe
  if it moves. Remove it via Task Manager → Startup apps if unwanted.
- **Single instance** — launching it again just pops the existing panel.
- **Glassy panel** — the popover uses Windows acrylic (blur-behind) with
  native rounded corners; if the OS can't provide it, the panel falls back to
  a solid background automatically.

## Stack

Tauri v2 (Rust backend, vanilla web frontend + Vite).

## Prerequisites

- Node 18+ and npm
- Rust toolchain (`rustup`) with the MSVC target
- Tauri's Windows deps: **WebView2 runtime** (preinstalled on Win11) and the
  **MSVC C++ Build Tools**

## Setup & run

```powershell
npm install

# dev (hot reload)
npm run tauri dev
```

Icons are checked in under `src-tauri/icons/`. To regenerate them:

```powershell
node scripts/gen-icon.mjs
npx tauri icon src-tauri/icons/source.png
```

## Build

```powershell
# release exe only (fastest)
npm run tauri build -- --no-bundle

# release exe + MSI/NSIS installers
npm run tauri build
```

The exe lands in the cargo target directory (path printed at the end of the
build); installers go under `target/release/bundle/`.

## Tests

```powershell
cd src-tauri
cargo test
```

## Tuning the cost estimate

Costs are **estimates** from local logs at list price (not what you actually
pay on a subscription plan). The per-million-token rates live in
[`src-tauri/src/pricing.rs`](src-tauri/src/pricing.rs) — adjust the `CLAUDE_*`
and `CODEX_*` rate cards to match the models you use and current pricing.
