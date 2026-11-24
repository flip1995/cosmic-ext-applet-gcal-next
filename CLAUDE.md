# CLAUDE.md

## Version control

- Use **`jj`** (Jujutsu) for all version control operations, not `git`. The repo
  is colocated (`.jj/` next to `.git/`), so only fall back to raw `git` for
  things `jj` cannot do.
- Typical flow: `jj st`, `jj diff`, `jj describe -m "…"`, `jj new`,
  `jj file track <path>` for new files (auto-tracking is not enabled).
- Only create or describe commits when asked.

## What this is

A minimal COSMIC panel applet: one pill showing the time until the next
meeting from **one** Google Calendar, colored by urgency and optionally
pulsing. Clicking it opens a popup with details, *Join*, *Refresh now* and a
settings page. The same binary also runs as an Ironbar module (`--ironbar`).

## Layout

| File | Role |
|------|------|
| `src/main.rs` | CLI (`--once`, `--ironbar`, `--settings`, `--join`, `--details`, `--refresh`, `--get`/`--set [--step]`/`--toggle`, `--ics-file`, `--now`, `--set-url`, `--print-default-config`); no args runs the applet |
| `src/app.rs` | `cosmic::Application`: pill view, popup, subscriptions, fetch scheduling |
| `src/settings.rs` | `SettingsState`: the settings page (view + saving via cosmic-config), used by the popup and the window |
| `src/settings_window.rs` | `--settings`: the settings page as a standalone libcosmic window |
| `src/ironbar.rs` | Ironbar mode: headless fetch/tick loop publishing the pill and popup texts as ironvars over IPC, plus the button helpers |
| `resources/ironbar.toml` | Ironbar `custom` module (pill + popup) wired to the CLI |
| `src/config.rs` | `Config` (`CosmicConfigEntry`), defaults, tier validation, secret URL loading (file / keyring) |
| `src/state.rs` | **Pure** `(events, now, config) -> DisplayState` |
| `src/format.rs` | **Pure** labels (`1h 20m`, `2h`, `25m`) |
| `src/style.rs` | Theme color resolution, pulse alpha |
| `src/source/ics.rs` | Own forgiving ICS parser + recurrence expansion (`rrule`) |
| `src/source/fetch.rs` | HTTPS/file fetch, 0600 disk cache, backoff |
| `src/source/links.rs` | Meet/Zoom/Teams link extraction |
| `tests/cli.rs`, `tests/fixtures/google.ics` | `--once` against a synthetic Google-style feed |

Keep `state.rs`, `format.rs` and `source/` free of UI code, and never call
`Utc::now()` in pure logic: `now` is passed in so everything is unit tested.

## Decisions

- **Data source: Google's secret iCal address**, no OAuth/EDS/GOA. The URL is a
  bearer secret: stored in `~/.config/cosmic-ext-applet-gcal-next/ical-url`
  (refused unless mode `0600`, `https://` only) or the Secret Service keyring
  (`oo7`, `--set-url`; the file wins). **Never log it**; `reqwest` errors go
  through `without_url()` and `Location`'s `Debug` is redacted. The URL is
  re-read on every fetch until it works, so setup needs no panel restart.
- **Own ICS parser** instead of the `icalendar` crate, which fails the whole
  feed on one bad line and quotes input in errors. Malformed lines/events are
  skipped and counted. TZIDs: IANA, prefixed IDs, a few Windows names, else
  local. `UNTIL` is normalised to UTC for `rrule`. `RECURRENCE-ID` overrides
  replace or cancel instances; orphan overrides are normal events.
- **Expansion window** `[now − 1 day, now + 8 days]` on each fetch, so any
  lookahead setting works without refetching.
- **Which meeting:** filters for cancelled, all-day, declined (by `my_email`),
  free (`TRANSP`) and zero-duration events. A meeting is *ongoing* (`now`) for
  `ongoing_grace_min` (5) after its start, then the next one shows. Only
  meetings starting within `lookahead_min` (120) count — this replaced the
  original "rest of today" cutoff. Tentative meetings count.
- **Labels round up** (59 s → `1m`, never `0m`). Zero parts are left out
  (`2h`, `1h 30m`); a fixed `0h 05m` form was tried and dropped.
  **Tiers use "at most N"**
  (`within_min`) against that rounded value, so color and text always agree
  and `1` means the last minute. (Originally "below N"; a 1-minute tier could
  never match.)
- **Defaults** (tuned by the user): ≤60 neutral, ≤30 accent, ≤10 success,
  ≤5 warning, ≤1 destructive + pulse; ongoing destructive without pulse;
  refresh every 120 s. Colors are theme tokens (follow light/dark themes) or
  `#RRGGBB[AA]`. Unknown tokens/bad hex/duplicate thresholds drop only that
  entry (`ThemeName` deserializes any identifier for this).
- **Settings** live in cosmic-config (`~/.config/cosmic/<APP_ID>/v1/`, one RON
  file per key) and apply live via the watcher. COSMIC Settings has no page for
  third-party applets, so the popup has a settings page (general options,
  warning levels, far/ongoing styles); labels, `none`/`error` styles, grace and
  pulse timing are file-only.
- **Interaction:** left click toggles the popup (anchored to the pill); no
  hover tooltip, no right click. *Join* opens the video link, or the day in
  Google Calendar's web UI if there is none (the feed has no event URL).
- **Timing:** 1 s UI tick (cheap; iced redraws the tiny pill). Fetch timer is a
  chain of one-shot tasks with a generation counter so *Refresh now* / resume
  can pre-empt it; failures back off 30 s → 5 min. Resume is detected when the
  wall clock jumps > 5 s ahead of the monotonic clock. The error state (`!`)
  only appears without any data or when it is older than `stale_after_min`
  (60); otherwise the last good data stays. The disk cache makes the pill right
  at login before the network is up.
- **Pulse** only subscribes to animation frames while the current state
  pulses; `pulse_enabled` is a global kill switch.
- **Vertical panels** show a colored dot.
- **Ironbar** (`--ironbar`): a tokio loop with the applet's fetch timing
  (refresh interval, backoff, resume detection, disk cache). It publishes
  the pill (Pango markup, `gcal_next_pill`) and the popup texts
  (`popup_vars`, pure) as ironvars by writing JSON to
  `$XDG_RUNTIME_DIR/ironbar-ipc.sock`. That is cheap enough for the ~10/s
  pulse; it publishes only on change, coalescing per key. It publishes
  nothing until there's a cache or a first fetch, so there's no `!` flash.
  Colors come from `style::pill_colors` with
  `cosmic::theme::system_preference()`. The buttons are Pango "chips" in
  theme colors, since Ironbar's default styling makes plain buttons look
  like text.
  - **Process lifetime (Ironbar v0.19 source):**
    - `{{watch:…}}` and polling scripts in dynamic strings are never killed,
      even on reload; Ironbar keeps their pipes open.
    - A `script` module's watch process *is* killed when another script
      module registers the same `cmd`.
    - So `resources/ironbar.toml` hides a script module in the bar button.
      Reloads and the per-monitor copies leave exactly one `--ironbar`.
      Every bar shows `#gcal_next_pill` via a label.
    - The process prints nothing and exits when its stdout gets
      POLLERR/POLLHUP (Ironbar gone). No PDEATHSIG: it fires on the
      parent *thread*'s exit.
  - **Popup:** labels have fixed widths (`truncate.length` =
    `max_length`), since a resize re-centers the popup under the cursor.
    (That's also why the Ironbar slider went: every drag step resized it.)
  - **Settings:** Ironbar's custom widgets have no text inputs or color
    pickers, so *Settings…* runs `--settings`: the same `SettingsState` page
    as the COSMIC popup, in a libcosmic window (own app ID,
    single-instance). There are also `--get`/`--set [--step N]`/`--toggle`
    for scripts. `config::for_each_key!` lists the keys once.
  - **Ironbar quirks:**
    - a label containing `<span` is used verbatim as markup, anything else is
      escaped;
    - `#` in config strings starts a variable;
    - `!cmd` and `{{…}}` run through `/bin/sh -c` after a shorthand parser
      that eats a leading `poll:`/`watch:`/digits prefix.
  - **Helpers:** `--join`/`--details` compute from the disk cache, never the
    network. `--refresh` SIGUSR1s `--ironbar` (found via `/proc/*/cmdline`,
    argv0 basename plus flag). The handler is installed first, since
    SIGUSR1's default is to terminate.
- **IDs:** app ID `io.github.flip1995.CosmicExtAppletGcalNext`, repo
  `flip1995/cosmic-ext-applet-gcal-next`, maintainer
  `Philipp Krones <hello@philkrones.com>`.

## Packaging and releases

- Arch package via **nFPM** (`nfpm.yaml`, `just package`), not a PKGBUILD.
  `cosmic-panel` and `ironbar` are optdepends (nFPM `suggests`).
  `depends` is maintained by hand (from `ldd` + `pacman -Qo`, plus the
  dlopen()ed `wayland`, and `hicolor-icon-theme` for the icon directories);
  re-check when bumping libcosmic. namcap in the release
  workflow flags gaps.
- One workflow, `.github/workflows/ci.yml` (`archlinux:base-devel`). Job
  `check` (main, PRs, tags): fmt, clippy `-D warnings`, tests, package build,
  uploaded as artifact; on tags it first checks tag == `Cargo.toml` version.
  Job `release` (tags `v*` only, `needs: check`): reuses that artifact — no
  second build or test run — runs namcap, installs and smoke-tests the
  package, publishes `.pkg.tar.zst` + `SHA256SUMS`.
- Release: bump `version` in `Cargo.toml` (+ `Cargo.lock`), describe the change,
  `jj bookmark set main -r @-`, `jj git push`, then `jj tag set vX.Y.Z -r main`
  and `jj git push --tag vX.Y.Z`.

## Development

`just check` (fmt, clippy, tests), `just once --ics-file tests/fixtures/google.ics --now …`,
`just run --ics-file f.ics` (applet in a window), `just install restart-panel`
(user-local install into the real panel), `just uninstall`.

## Possible follow-ups

Package signing, a pacman repo, `.deb`/`.rpm` via nFPM, AUR package, a Google
Calendar API backend if the secret address is unavailable (Workspace policy),
logind `PrepareForSleep` / network-up refresh, panel test on more themes.
