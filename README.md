# AI Usage Tray

A Windows system-tray app that shows Claude and Codex quota usage, reset times,
and an API-equivalent value estimate in a compact popover.

- **Overview** — both providers at a glance and today's combined API value.
  When something is low, a **Runs out first** card at the top names the window
  most likely to stop you, says when its pace runs it out, and offers a banked
  reset when there is one. From four accounts up (or always, or never — Settings
  → Display) the cards give way to one row per account, worst first.
- **Codex** — live quota windows, plan and credit details, reset credits, and
  model-aware local token history. Accounts with **Luna Reserve** — the
  separately metered GPT-5.6 Luna allowance Codex opens once the regular limit
  is used up — get its meter under the main ones, marked *standing by* or
  *in use*. It appears on the Overview only while it's being spent, and the
  "limit used up" notification says when the reserve takes over.
- **Claude** — live account and scoped quota windows plus model-aware local
  Claude Code history. Multiple Claude config directories are supported.
  Accounts with **banked resets** (the ones Claude Code offers at a usage
  limit) show how many are left and when the next one expires, with a
  **Use reset** button that claims one the same way Claude Code and T3 Code
  do. Settings → Alerts can announce new resets and expiring ones.
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
- **Settings** — split into Alerts, Display, Widget, Accounts and Advanced
  (data sources and About), one section at a time.
- **Credits** — a dedicated screen (opened from the violet meter on any tab)
  with each account's usage-credit spend, monthly cap, and whether "use
  credits past plan limits" is switched on, plus the Codex credit balance.

Every time-based meter carries a **pace line**: a hairline marking how far
through its window the clock is, drawn in the same direction the bar runs. Fill
short of the line means the allowance is going faster than the window refills
it. Usage credits have a cap but no clock, so they never show one. Turn it off
in Settings → Display.

## Meter states and the tray icon

Meters read in tiers, each with a word as well as a colour: **low** (amber)
from 80% used, **runs out** (red) once the window's average pace so far would
empty it before it resets, **almost out** (red) past 95%, and **used up** — a
striped track whose headline is when it comes back. A meter that will run out
early also gets an *Out by 4:52 pm, 18 min before reset* line under it. The
forecast waits until a tenth of the window (and at least ten minutes) has gone
by and the window is half spent, so an energetic first hour isn't flagged.

The rule lives in `src-tauri/src/forecast.rs` and `src/shared.js`, tested
against the same cases, because the tray icon and the panel have to agree:

- **Tray icon** — a ring showing how much of the tightest window is left, in
  the provider's colour, amber or red at the same tiers. "Tightest" is the
  worst tier, and among windows heading for empty the one that gets there
  first. Settings → Display can pin it to one account or to the plain logo.
  A dot in its corner means something new needs you — a reset to spend or
  about to expire, or a login that stopped working — and opening the panel
  clears it.
- **Tooltip** — one line per account with its tightest window.
- **Right-click menu** — the tightest window and its forecast, a shortcut to
  the confirm for a banked reset when that window has one, Open panel,
  Settings, switches for the taskbar widget and limit alerts, Refresh, Check
  for updates and Quit.

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

- **High** — the model has an exact current catalog entry (a dated snapshot
  such as `claude-haiku-4-5-20251001` counts as its model's entry).
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
  `CODEX_HOME/auth.json`. A login is only refreshed after the provider has
  rejected it, never ahead of its expiry, because the CLI refreshes the same
  file on its own schedule and refreshing early would race it for the
  single-use refresh token. Rotated tokens are written back atomically,
  parallel fetches that find the same token rejected share one refresh, and a
  refresh whose new tokens can't be saved is reported as such — the CLI is
  signed out at that point — rather than as an ordinary authorization error.
- **Codex history** comes from both
  `CODEX_HOME/sessions/**/rollout-*.jsonl` and
  `CODEX_HOME/archived_sessions/**/rollout-*.jsonl`. If `CODEX_HOME` is unset,
  `~/.codex` is used. Usage is attributed to each event's timestamp, not the
  directory date.
- **Claude history** comes from `~/.claude/projects/**/*.jsonl` (and the
  equivalent directory for each configured Claude account).
- **Value by project** comes from the working directory both CLIs log with
  each session (`cwd`). The value card on each provider tab lists today's most
  expensive folders, or the month's before anything has been used today.
- **Data health** is an expandable row in Settings → Advanced. It identifies
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
JSONL records. Files changed in place or truncated are rescanned. Only the last
month is kept: log files untouched for 31 days are skipped, and events older
than 30 days are dropped from the index, so it doesn't grow with every session
ever recorded. Cache writes
use atomic replacement, and an unreadable or older cache version is ignored and
rebuilt. The cache stores token facts rather than currency totals, so a pricing
catalog update reprices history without forcing a log rescan. Each file's
record also keeps the session's working directory; adding that bumped both
cache versions, so the first launch after updating rescans the last month once.

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
**Settings → Advanced → Data sources → Quota window history** reports how much exists
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
switched on in Settings → Widget, or from the tray menu.

- One cell per account — Codex first, then Claude — with the account name set
  inside its own bar, the pace mark from the panel's meters, and a dimmer weekly
  bar tucked underneath. Colour is the only thing naming the provider.
- Accounts are flowed across two rows, `ceil(n/2)` on top. With one Codex and
  two Claude accounts the first Claude moves up beside the Codex and the second
  spans the row below, so every cell keeps a roughly equal share of the width.
  Account count never changes the widget's width — more accounts narrow the
  cells instead.
- **Settings → Widget → Layout** overrides that flow. Each account can
  be pinned to the **Top** or **Bottom** row, or left on **Auto**. Since a row
  shares its width equally, an account alone on a row spans the whole widget —
  so pinning one account to one row and the rest to the other is how you give
  the full width to an account that isn't the one the split happens to leave
  over. Pinning everything to a single row is how you ask for one row of
  full-height bars. A live preview above the pickers shows where they land, and
  a row left empty is dropped rather than drawn.
- While Codex's regular limit is used up and Luna Reserve still has room, the
  Codex cell shows the reserve instead, since a full bar says nothing new. It
  switches back once the regular limit resets or the reserve runs out too.
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
- Right-click the tray icon for the menu described under
  [Meter states and the tray icon](#meter-states-and-the-tray-icon).
- Esc backs out one step: an open confirm, the Credits screen, the expanded
  timeline, then the panel itself. The tabs take arrow keys, and every card
  and meter that opens something can be reached with Tab.
- The panel follows the Windows light or dark app setting, as the widget does.
- On first launch a card on the Overview offers the three things that are off
  by default: the taskbar widget, limit alerts and starting with Windows.
- The panel refreshes when opened and every 60 seconds while visible.
- A lightweight background refresh runs every five minutes even while the
  panel is hidden — every two while the taskbar widget is on — so the widget,
  the Windows timeline and notifications all see usage from other devices.
  Local usage-file changes also schedule a refresh; a change that arrives
  while one was just made is held back and run shortly after, not dropped.
  Both windows repaint from what a background refresh collected rather than
  asking the providers again.
- Limit notifications (Settings → Alerts) warn as a window passes each chosen
  level (90% and 95% unless you pick others), once per window when its pace
  will run it out before it resets, and when it is used up — with when it comes
  back and any banked resets — and resets. Quiet hours hold them overnight.
  Windows toasts from this app can't carry buttons, so a used-up toast points at
  the panel rather than offering the reset itself.
- Using a Codex reset that gets no clear answer keeps its request id, so
  trying again repeats the same redemption instead of spending another credit.
- Claude's usage endpoint is paced per account, because an active Claude Code
  session writes logs constantly and would otherwise have it asked every 30
  seconds. A result under 45 seconds old is reused, and after a rate-limit
  response (HTTP 429) the account is left alone for five minutes, showing its
  last numbers with their age. Local cost and token history is unaffected.
  Banked resets ride along on that same request (`cedar_ember=1`), so they
  cost nothing extra; claiming one skips the 45-second reuse for that account
  so the cleared limits show straight away.
- The app starts on login by registering itself under
  `HKCU\...\CurrentVersion\Run`. Switch it off in **Settings → Advanced → About →
  Start with Windows**; the choice is kept in
  `%APPDATA%\AI Usage Tray\startup.json`.
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
npm test
```

The Rust suite includes sanitized provider-contract fixtures, pricing and
cached-input accounting, dynamic scoped quotas, provenance fallback behavior,
active and archived Codex history, event-date attribution, unchanged and
append-only scan paths, and atomic cache replacement.

`npm test` covers the helpers both windows share, and checks the command
permissions: each window may only call the app commands its file in
`src-tauri/capabilities/` grants, and a command left out of `src-tauri/build.rs`
or a window's capability would otherwise only fail at runtime. Adding a
command means adding it to `generate_handler!`, to `COMMANDS` in `build.rs`,
and to the capability of each window that calls it.

CI (`.github/workflows/ci.yml`) runs both suites, a frontend build and clippy
on every push and pull request.

## Updates

The app checks GitHub Releases for a newer signed build about a minute after
launch and every six hours after that, and tells you when one is found. It does
not install anything on its own unless you ask it to. Both behaviors are toggles
in **Settings → Advanced → About**, and the tray right-click menu has a **Check for
updates…** item for an immediate check.

Every release is either **Stable** or **Nightly**, and says so in its title,
in Settings → About and in the update notification.

- **Stable** is the feed every install has always read: the `latest.json` of
  the newest published, non-prerelease release. Builds from before channels
  existed read that same URL, so they keep updating, and only ever to stable.
- **Nightly** builds are versioned as prereleases of the next minor
  (`0.11.0-nightly.1`) and published as GitHub prereleases, which never become
  "latest". A fixed release tagged `nightly` holds the nightly feed's
  `latest.json`. An install on the nightly channel reads both feeds and takes
  the higher version, so a stable release that overtakes the nightlies still
  reaches it.

Settings → About has the channel switch. Unset, it follows the build: a
nightly install stays on nightly. Moving from nightly back to stable doesn't
downgrade; it waits until a stable release is newer than the nightly you have.

Updates are verified against a public key compiled into the app, so a release
that wasn't signed with the matching private key is refused. That protects the
integrity of the update itself; it is not a Microsoft code-signing certificate,
so Windows SmartScreen still shows its "unrecognized app" prompt during install.

### Cutting a release

```powershell
# Stable
npm version minor                 # bumps package.json, syncs Cargo.toml, commits, tags
git push --follow-tags            # the tag triggers .github/workflows/release.yml

# Nightly: a prerelease of the next minor, counting up
npm version 0.11.0-nightly.1
git push --follow-tags
```

The workflow reads the channel from the tag: anything containing `-nightly`
is a nightly.

`package.json` is the single source of truth for the version:
`tauri.conf.json` reads it via `"version": "../package.json"`, and
`scripts/sync-version.mjs` keeps `src-tauri/Cargo.toml` in step.

CI builds and signs on `windows-latest`. A stable build becomes a **draft**
release, and installed apps only see it once that draft is published — the
manual publish step is deliberate, because an auto-updating app otherwise
distributes a broken build to every install with no way to recall it. A
nightly build is published at once as a prerelease and the `nightly` feed is
pointed at it; only installs that opted into nightly read that feed.

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
