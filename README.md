# AI Usage Tray

A Windows system-tray app that shows Claude and Codex quota usage, reset times,
and an API-equivalent value estimate in a compact popover.

- **Overview** — both providers at a glance and today's combined estimated value.
- **Codex** — live quota windows, plan and credit details, reset credits, and
  model-aware local token history.
- **Claude** — live account and scoped quota windows plus model-aware local
  Claude Code history. Multiple Claude config directories are supported.
- **Windows** — a timeline of every quota window: bar length is the window's
  span, the fill is what was spent inside it, and a notch marks now, so a fill
  running ahead of the notch means the allowance is going faster than the
  window refills it. **Today** is an hour axis where five-hour windows are
  full-size bars — finished ones dim, the live one bright, the next dashed.
  **Week** and **2 weeks** start on the day the earliest window in view opened,
  so a weekly bar shows both where it began and where it resets, with the window
  after it dashed alongside. Week sizes itself to the windows you are currently
  in rather than to a literal seven days, which could not hold a seven-day
  window and its start at once. `⤢` widens the panel to a full-width version
  that stays open until you close it.
- **Settings** — notifications, meter display options, the taskbar widget, and
  per-account **Show in App / Widget** switches.
- **Credits** — a dedicated screen (opened from the violet meter on any tab)
  with each account's usage-credit spend, monthly cap, and whether "use
  credits past plan limits" is switched on, plus the Codex credit balance.

Every time-based meter carries a **pace line**: a hairline marking how far
through its window the clock is, drawn in the same direction the bar runs. Fill
short of the line means the allowance is going faster than the window refills
it. Usage credits have a cap but no clock, so they never show one. Turn it off
in Settings → Display.

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

## Recorded quota windows

The usage endpoints only describe the window that is live right now, so the
Windows timeline would have nothing to draw behind "now". Every refresh
therefore samples each live window into a third cache:

```text
%APPDATA%\AI Usage Tray\windows-history-cache.json
```

Each entry is a window occurrence — when it started, when it resets, and the
percentages observed while it ran. Readings are stored when the number moves or
every fifteen minutes, occurrences older than fifteen days are dropped, and
recording does not depend on the notification settings.

Nothing is back-filled. A window that closed before the app was watching stays
absent rather than being drawn as idle: the day view draws those stretches
hollow and says how far back the record goes. Unlike the log indexes this
cannot be rebuilt, which is why
**Settings → Data sources → Quota window history** reports how much exists
before offering to clear it.

## Showing and hiding accounts

**Settings → Accounts** lists every account — Codex, then each Claude config
folder — with a **Show in** pair: **App** and **Widget**. The two are
independent, so an account can sit on the taskbar while staying out of the
panel, or the other way round.

Hiding is not removing. The folder stays registered, its credentials are left
alone, its quota windows keep being recorded, and turning it back on restores
everything at once. Removing a folder (`✕`) is still the way to forget it.

A hidden account disappears from the Overview, the provider tabs, the Windows
timeline and the Credits screen, and its spend drops out of the Overview's
"today" figure. A provider with no visible accounts left loses its tab
entirely — an empty Codex screen is worse than no Codex screen — and standing on
that tab when it goes moves you to Overview. Settings always lists everything,
which is what keeps the switch reachable.

Because the local Claude logs carry no account id, the cost and token history is
machine-wide: hiding one of several Claude accounts leaves those figures whole.

The choice is stored in

```text
%APPDATA%\AI Usage Tray\account-visibility.json
```

as the *hidden* ids rather than the visible ones, so a folder added after
that file was written shows up by default instead of arriving invisible.

## Taskbar widget

An optional always-visible strip of per-account meters, off by default and
switched on in Settings → Taskbar widget.

- One cell per account — Codex first, then Claude — with the account name set
  inside its own bar, the pace mark from the panel's meters, and a dimmer weekly
  bar tucked underneath. Colour is the only thing naming the provider.
- Accounts are flowed across two rows, `ceil(n/2)` on top. With one Codex and
  two Claude accounts the first Claude moves up beside the Codex and the second
  spans the row below, so every cell keeps a roughly equal share of the width.
  Account count never changes the widget's width — more accounts narrow the
  cells instead.
- **Settings → Taskbar widget → Layout** overrides that flow. Each account can
  be pinned to the **Top** or **Bottom** row, or left on **Auto**. Since a row
  shares its width equally, an account alone on a row spans the whole widget —
  so pinning one account to one row and the rest to the other is how you give
  the full width to an account that isn't the one the split happens to leave
  over. Pinning everything to a single row is how you ask for one row of
  full-height bars. A live preview above the pickers shows where they land, and
  a row left empty is dropped rather than drawn.
- An account whose plan reports only one window (Codex meters weekly only on
  some plans) draws that single bar filling the row, rather than a real bar over
  an empty one.
- Names are measured against the cell they have to fit. Too wide and a leading
  `claude-`/`codex-` token is dropped first — the cell's colour already says
  which provider it is — then the first word, then the initial. Widening the
  widget brings longer names back.
- Clicking it toggles the panel, exactly as the tray icon does. It never takes
  focus (`WS_EX_NOACTIVATE`), stays out of Alt-Tab, and hides itself while a
  fullscreen window owns the monitor.
- Drag it along the taskbar to move it, or drag either side edge to resize
  (72–420 px). Dragging the left edge holds the right one against the tray;
  dragging the right edge spends the tray gap so the left one holds still. A
  grip disappears once that side is against the end of the bar. Both position
  and size are remembered. Settings has a width stepper, switches for the pace
  marks and the weekly bar, and a Reset for position and size.
- Which accounts get a cell is its own choice, separate from the panel's — see
  [Showing and hiding accounts](#showing-and-hiding-accounts). With every
  account switched off the widget stays on the bar as a dim placeholder, so it
  is still there to click and still there to put back. Row pins are kept for
  accounts that are switched off or signed out, so the arrangement comes back
  as it was rather than being rebuilt from scratch.
- With more than one display, Settings gains a Display picker. The drag is
  confined to a single taskbar — that is what stops the widget being dropped in
  the middle of the desktop — so choosing the screen is a setting rather than a
  gesture. The choice is remembered, and falls back to the current screen if
  that display is later unplugged.
- It only draws on a taskbar along the top or bottom. A side-docked bar has no
  room for a two-row strip, and an auto-hidden one has nothing to sit on, so in
  both cases the widget stays hidden and Settings says why.

Windows exposes no way for a third-party app to place a real button inside the
taskbar — the notification area is the only sanctioned spot, and that is a single
16 px icon. So this floats on top of the bar instead.

Two Windows details drive the placement code. The widget is sized to the strip
the taskbar *reserves* (monitor minus work area), **not** to `Shell_TrayWnd`'s
own rect: Windows 11 reports that window ~24 px taller than the bar it paints,
so matching it leaves the widget standing proud of the taskbar's top edge. It is
anchored horizontally to the left edge of `TrayNotifyWnd` — the chevron, icons
and clock — because how wide that block is depends on how many icons are
showing. Z-order is reclaimed from a `SetWinEventHook` on foreground changes,
since the taskbar is itself a topmost window and clicking it buries anything
above; a 3-second pass covers the rest.

The layout was designed in `design/taskbar-flowed-cells-mockups.html`, which
stays as the reference for the geometry.

## Behavior

- Left-click the tray icon to toggle the panel; it auto-hides on blur and is
  clamped to the monitor work area. The expanded timeline is the exception —
  it stays open until you collapse it (`⤢`, Esc, or the tray icon), so it can be
  read alongside other windows.
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
