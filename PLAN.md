# cosmic-ext-applet-gcal-next — Implementation Plan

A deliberately minimal COSMIC panel applet: a single pill ("island") in the top
bar that shows the time until the next meeting from **one** Google Calendar,
changes color as the meeting approaches and pulses when it is imminent.

```
 ╭──────────╮   ╭──────────╮   ╭─────╮   ╭─────╮   ╭────────────╮
 │  1h 20m  │   │   25m    │   │  8m │   │  1m │   │ No meeting │
 ╰──────────╯   ╰──────────╯   ╰─────╯   ╰─────╯   ╰────────────╯
   neutral        yellow        orange   red+pulse     dimmed
```

## 1. Goals and non-goals

**Goals**

- Show the relative time to the next meeting **today** (`1h 20m`, `25m`, `now`).
- Show `No meeting` when nothing is left for the rest of the day.
- Color the pill by urgency; pulse it when the meeting is very close or has just started.
- **All of it configurable:** the time thresholds, the color of each tier and
  whether each tier pulses (see §3.2 and §4).
- One Google Calendar, configured once, no GNOME Online Accounts / EDS stack.
- Low resource usage: no busy redraws except while pulsing.

**Non-goals (for v1)**

- Multiple calendars or multiple accounts.
- An agenda / month view, event editing, notifications.
- Supporting non-Google calendars (comes almost for free with ICS, but untested).

## 2. Key decisions

### 2.1 Data source: Google "secret iCal address"

Every Google calendar has a private ICS URL
(Calendar settings → *Integrate calendar* → *Secret address in iCal format*).
The applet fetches that URL with a plain HTTPS GET.

Why:

- No OAuth flow, no client ID, no token refresh, no EDS/GOA dependency.
- Good fit for "one calendar, read-only".

Trade-offs / risks (verify in Phase 0):

- **Workspace admins can disable the secret address.** On a company Google
  Workspace account it may simply not be available. → Fallback: Google Calendar
  API backend (see 2.2).
- The URL is a bearer secret: anyone with it can read the calendar. It must be
  stored with `0600` permissions (or in the Secret Service keyring) and never logged.
- We must expand recurrence rules ourselves (RRULE, EXDATE, RECURRENCE-ID overrides).
- The full feed can be large (years of history). Fetch it at most every few
  minutes and parse off the UI thread.
- Check how quickly edits in Google Calendar show up in the feed.

### 2.2 Backend abstraction

Put the data source behind a small trait so a Google Calendar API backend can be
added later without touching the UI:

```rust
#[async_trait]
trait EventSource {
    /// Return all event instances overlapping [from, to), already expanded.
    async fn events(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<Event>>;
}
```

- `IcsSource` (v1): HTTP GET + parse + expand recurrences.
- `GoogleApiSource` (optional, later): OAuth installed-app flow +
  `events.list?singleEvents=true&timeMin=…&timeMax=…`, which returns recurrences
  already expanded. Use this only if the ICS route is blocked or too slow to update.

### 2.3 UI toolkit

- **libcosmic** (git dependency, features `applet`, `tokio`, `wayland`), structured like
  the official applet template (`cosmic::applet::run::<App>(())`).
- Rendering: a `button` with a custom container style (rounded corners,
  background color from the urgency state) containing a `text` widget. Size based
  on `core.applet.suggested_size()` so it respects the panel size setting.
- Colors from the active COSMIC theme where possible (`warning`, `destructive`,
  accent, neutral) so it looks right in both light and dark themes. Fixed
  RGB fallbacks can be set in the config.

## 3. Behaviour spec

### 3.1 Which event counts as "next meeting"

From the expanded instances for *[now − 1 day, end of today]*, keep events that:

1. are **not all-day** (DTSTART is a `DATE`, not a `DATE-TIME`) — configurable,
2. are not `STATUS:CANCELLED`,
3. are not declined by me (`ATTENDEE;PARTSTAT=DECLINED` matching the configured
   e-mail; skip this filter if no e-mail is configured),
4. optionally (config) are not zero-duration / "transparent" (`TRANSP:TRANSPARENT`).

Then:

- If an event **started within the last `ongoing_grace` minutes (default 5)** and has
  not ended → state **Ongoing** (show `now`, red, pulsing).
- Otherwise the earliest event with `start > now` and `start < end_of_today` →
  **Upcoming**.
- Otherwise → **None** (`No meeting`).

"End of today" = local midnight, in the system time zone.

### 3.2 States, labels and styles

The urgency levels are **not hard-coded**. They come from a configurable list
of *tiers*. Each tier has:

- `below_min`: the tier applies when the time until the meeting starts is
  below this many minutes,
- `color`: a theme token (`neutral`, `accent`, `success`, `warning`,
  `destructive`) or a fixed color (`#RRGGBB` / `#RRGGBBAA`),
- `pulse`: whether the pill pulses in this tier.

The tier with the **smallest** `below_min` that is still greater than the
remaining time wins. If no tier matches, the `far` style is used. The fixed
states (`far`, `ongoing`, `none`, `error`) have their own configurable
`color` + `pulse` entries.

Default configuration (what you get with no config file):

| State / tier | Condition           | Label         | Color (default)  | Pulse (default) |
|--------------|---------------------|---------------|------------------|-----------------|
| `far`        | no tier matches     | `1h 20m`      | `neutral`        | no              |
| tier         | < 60 min            | `45m`         | `accent`         | no              |
| tier         | < 30 min            | `25m`         | `warning`        | no              |
| tier         | < 10 min            | `8m`          | `#F5841F` orange | no              |
| tier         | < 2 min             | `1m` / `<1m`  | `destructive`    | **yes**         |
| `ongoing`    | started ≤ grace ago | `now`         | `destructive`    | **yes**         |
| `none`       | nothing left today  | `No meeting`  | `neutral` dimmed | no              |
| `error`      | never fetched / cache too old | `!` | `neutral` dimmed | no            |

You can add, remove or change tiers freely: e.g. drop the 60 min tier, or let the
10 min tier pulse too. When the config is loaded, the tiers are sorted by
`below_min`. Invalid entries (unknown token, bad hex, duplicate threshold) are
logged and skipped and never crash the applet.

Label format: `Xh Ym` for ≥ 1 h, `Ym` below, rounding **up** (so 59 s left shows
`1m`, never `0m`). The `none` and `ongoing` label texts can be configured too.

### 3.3 Pulsing

- Only while the active tier/state has `pulse = true`: enable an animation
  subscription (`cosmic::iced::time::every(~33 ms)`) and change the background alpha
  with `min + (1 − min) · (0.5 + 0.5 · sin(2π · t / period))`.
- `pulse_period_ms` (default 1200) and `pulse_min_alpha` (default 0.35) are
  configurable globally.
- In all other states, no animation subscription is active (no busy redraws).
- Global kill switch `pulse_enabled = false` overrides every tier (also the place
  to respect reduced motion if COSMIC exposes it).

### 3.4 Timing

- **UI tick:** recompute the state once per second while within the smallest
  configured tier threshold, otherwise align to the next minute boundary (`every(1 s)` is also fine — the computation is cheap,
  but redraw only when the label or state changes).
- **Fetch:** every 5 min (configurable), plus:
  - immediately on startup,
  - after resume from suspend (detected by a wall-clock jump > 2× tick interval
    between ticks; optionally the logind `PrepareForSleep` D-Bus signal),
  - on network coming back (optional, later).
- **Errors:** keep the last good event list; retry with exponential backoff
  (30 s → 5 min). Only show *Error* if there has never been a successful fetch
  or the cache is older than e.g. 1 h.
- **Cache:** write the last good ICS body to `$XDG_CACHE_HOME/…/calendar.ics`
  (0600) so the pill is correct right after login, even before the network is up.

### 3.5 Interaction (minimal)

- **Left click:** open the meeting's video link (Google Meet URL from
  `X-GOOGLE-CONFERENCE`, else the first `https://meet.google.com/…`/Zoom/Teams
  URL found in `LOCATION`/`DESCRIPTION`). If there is none, open the event's
  Google Calendar web page or do nothing.
- **Tooltip / hover:** event title + start time (`Standup · 10:30`).
- Optional, later: a small popup with title, time and a *Join* button,
  plus "Refresh now".

### 3.6 Vertical panels

If the panel is vertical (`core.applet.is_horizontal() == false`), show a colored
dot only (same colors and pulse), with the time in the tooltip.

## 4. Configuration

Two parts:

1. **Settings** via `cosmic-config` (`~/.config/cosmic/<APP_ID>/v1/<key>`, one
   RON file per key), non-secret. The Rust struct (derives `CosmicConfigEntry`,
   every field has a default):

   ```rust
   struct Config {
       refresh_interval_secs: u64,   // 300
       ongoing_grace_min: u32,       // 5
       skip_all_day: bool,           // true
       skip_declined: bool,          // true
       my_email: Option<String>,     // for the PARTSTAT=DECLINED filter

       // --- appearance: times, colors, pulsing ---
       tiers: Vec<Tier>,             // see §3.2 defaults
       far: Style,
       ongoing: Style,
       none: Style,
       error: Style,
       none_label: String,           // "No meeting"
       ongoing_label: String,        // "now"
       pulse_enabled: bool,          // true (global kill switch)
       pulse_period_ms: u32,         // 1200
       pulse_min_alpha: f32,         // 0.35
   }

   struct Tier  { below_min: u32, style: Style }
   struct Style { color: ColorSpec, pulse: bool }
   enum ColorSpec { Theme(ThemeColor), Rgba(String /* "#RRGGBB[AA]" */) }
   ```

   Example `tiers` file (RON):

   ```ron
   [
       (below_min: 30, style: (color: Theme(Warning),     pulse: false)),
       (below_min: 10, style: (color: Rgba("#F5841F"),    pulse: true)),
       (below_min: 2,  style: (color: Theme(Destructive), pulse: true)),
   ]
   ```

2. **Secret ICS URL**, either:
   - `$XDG_CONFIG_HOME/cosmic-ext-applet-gcal-next/ical-url` with mode `0600`
     (v1, simplest; refuse to start / warn if permissions are broader), or
   - Secret Service keyring via the `oo7` crate (later), set with a tiny CLI:
     `cosmic-ext-applet-gcal-next --set-url`.

No settings UI in v1. Settings changes (including tiers, colors and pulse) are
applied live through the `cosmic-config` watch subscription, so you can tune
them by editing the files with the panel running. A `--print-default-config`
CLI flag writes out the defaults as a starting point.

## 5. Crate / module layout

Use `~/flip1995/rust-template` as the starting point (licenses, `.rustfmt.toml`,
`.gitignore`), then layer the applet template structure on top.

```
cosmic-ext-applet-gcal-next/
├── Cargo.toml
├── Cargo.lock                    # committed; packaging builds with --locked
├── justfile                      # build, run, install (user-local), package, check
├── .github/workflows/
│   ├── ci.yml                    # fmt, clippy, test on push/PR
│   └── release.yml               # on tag v*: build Arch package, publish release
├── nfpm.yaml                     # package definition (nFPM, archlinux packager)
├── resources/
│   ├── app.desktop               # X-CosmicApplet=true, NoDisplay=true
│   └── icon.svg
└── src/
    ├── main.rs                   # arg parsing (--once, --set-url), applet::run
    ├── app.rs                    # cosmic::Application impl: view, update, subscription
    ├── config.rs                 # cosmic-config struct + secret URL loading
    ├── state.rs                  # PURE: (events, now, cfg) -> DisplayState
    ├── format.rs                 # PURE: Duration -> "1h 20m"
    ├── style.rs                  # DisplayState + theme -> colors, pulse alpha
    └── source/
        ├── mod.rs                # Event struct, EventSource trait
        ├── ics.rs                # parse + recurrence expansion
        ├── fetch.rs              # reqwest GET, cache, backoff
        └── links.rs              # extract Meet/Zoom/Teams URL
```

Keeping `state.rs`, `format.rs` and `source/ics.rs` free of UI code means almost
all logic can be unit tested without a running panel.

### 5.1 Dependencies (planned)

| Crate | Purpose |
|-------|---------|
| `libcosmic` (git) | applet runtime, widgets, theme, `cosmic-config` |
| `tokio` | async runtime (shared with libcosmic) |
| `reqwest` (`rustls-tls`, no default features) | HTTPS fetch |
| `icalendar` (`parser`) | ICS parsing |
| `rrule` | RRULE / EXDATE / RDATE expansion |
| `chrono`, `chrono-tz` | time zones (Google emits IANA `TZID`s) |
| `serde` | config |
| `tracing`, `tracing-subscriber` | logging (never log the URL) |
| `open` | open the meeting link in the browser |
| `anyhow` / `thiserror` | errors |

## 6. ICS handling details (the tricky part)

- **Time zones:** `DTSTART;TZID=Europe/Berlin:…` → resolve with `chrono-tz`;
  `…Z` → UTC; floating times → local zone. Ignore embedded `VTIMEZONE`
  blocks unless a TZID isn't an IANA name (then map known Windows names or fall back to local).
- **Recurrences:** feed `DTSTART` + `RRULE` + `EXDATE` + `RDATE` into `rrule`
  and expand only within the query window (never unbounded).
- **Overrides:** a `VEVENT` with the same `UID` and a `RECURRENCE-ID` replaces
  (or, if `STATUS:CANCELLED`, removes) the matching generated instance.
- **Duration:** `DTEND` or `DURATION`; default 0 if neither.
- **Robustness:** a single malformed event must not drop the whole feed — log
  it and skip it.

## 7. Implementation phases

### Phase 0 — Spike (½ day)

- [ ] Check that the secret iCal address is available for the target account
      (Workspace policy!). If not → switch to the `GoogleApiSource` plan.
- [ ] `curl` the feed, check its size, and see how quickly edits show up.
- [ ] Collect anonymised fixture snippets: recurring meeting with an exception,
      moved instance, cancelled instance, declined event, all-day event,
      cross-midnight event, event in another time zone.

### Phase 1 — Core logic + CLI (1–2 days)

- [ ] Scaffold the repo from `rust-template`, add the dependencies.
- [ ] `source/ics.rs`: parse + expand, with fixture-based unit tests.
- [ ] `state.rs` + `format.rs`: pure state machine, table-driven tests
      covering every threshold boundary, the end of the day, ongoing grace, and DST changes.
- [ ] `config.rs`: config struct with defaults, tier sorting/validation,
      `ColorSpec` parsing (theme tokens + hex), with tests for tier selection
      on custom tier lists (empty list, single tier, unsorted, duplicates).
- [ ] `fetch.rs`: fetch + cache + backoff.
- [ ] `main.rs --once`: print `state label | title | start` to stdout,
      which is useful for debugging and lets you check the logic without the panel.

### Phase 2 — Applet (1–2 days)

- [ ] Applet skeleton from the COSMIC applet template; desktop entry with
      `X-CosmicApplet=true`.
- [ ] Pill view with urgency colors from the theme.
- [ ] Subscriptions: 1 s tick, fetch timer, config watcher, animation only while pulsing.
- [ ] Tooltip with title/time; left click opens the meeting link.
- [ ] `just install` (user-local: `~/.local/bin` + `~/.local/share/applications`)
      for development; system packaging is handled by nFPM (Phase 4).
      Add the applet via *Settings → Desktop → Panel → Applets*, restart the panel to reload.

### Phase 3 — Polish

- [ ] Vertical panel dot mode.
- [ ] Resume-from-suspend refresh.
- [ ] Error state + tooltip with last successful sync time.
- [ ] Optional popup (title, time, Join, Refresh).
- [ ] Keyring storage for the URL (`oo7`) + `--set-url`.

### Phase 4 — CI/CD and Arch Linux package

Goal: every tagged release publishes a ready-made `.pkg.tar.zst` on GitHub
Releases, installable with

```sh
sudo pacman -U https://github.com/<owner>/cosmic-ext-applet-gcal-next/releases/download/v0.1.0/cosmic-ext-applet-gcal-next-0.1.0-1-x86_64.pkg.tar.zst
```

(`pacman -U` accepts URLs as well as local files.)

Packaging uses **[nFPM](https://github.com/goreleaser/nfpm)** with its
`archlinux` packager instead of a PKGBUILD/makepkg. Cargo builds the binary,
then nFPM only packs the prebuilt files into a `.pkg.tar.zst`, so there's no
build-as-non-root dance and no makepkg in CI.

**4.1 `nfpm.yaml`**

- [ ] Package metadata:

  ```yaml
  name: cosmic-ext-applet-gcal-next
  arch: amd64                  # nFPM translates to x86_64 for archlinux
  platform: linux
  version: ${VERSION}          # from the git tag, without the leading "v"
  release: 1                   # bump only for packaging-only fixes
  maintainer: <name> <email>
  description: Minimal COSMIC panel applet showing the time to your next Google Calendar meeting
  homepage: https://github.com/<owner>/cosmic-ext-applet-gcal-next
  license: MIT OR Apache-2.0
  depends:                     # runtime libs, see below
    - cosmic-panel
    - libxkbcommon
    - wayland
  archlinux:
    packager: <name> <email>
  contents:
    - src: target/release/cosmic-ext-applet-gcal-next
      dst: /usr/bin/cosmic-ext-applet-gcal-next
      file_info: { mode: 0755 }
    - src: resources/app.desktop
      dst: /usr/share/applications/<APP_ID>.desktop
    - src: resources/icon.svg
      dst: /usr/share/icons/hicolor/scalable/apps/<APP_ID>.svg
    - src: LICENSE-MIT
      dst: /usr/share/licenses/cosmic-ext-applet-gcal-next/LICENSE-MIT
    - src: LICENSE-APACHE
      dst: /usr/share/licenses/cosmic-ext-applet-gcal-next/LICENSE-APACHE
  ```

- [ ] **`depends` is maintained by hand.** nFPM doesn't detect shared library
      dependencies the way makepkg does, so derive the list once from
      `ldd target/release/…` + `pacman -Qo` (e.g. `libxkbcommon`, `wayland`,
      `fontconfig`, `freetype2`, `gcc-libs`, `glibc`). Re-check it whenever
      libcosmic is bumped; the CI smoke test (4.3 step 5) catches gaps. No
      `openssl` thanks to `rustls`.
- [ ] Release profile in `Cargo.toml`: `strip = true`, `lto = "thin"`, since
      nFPM doesn't strip binaries.
- [ ] `just package` recipe: `cargo build --release --locked` +
      `nfpm package --packager archlinux --config nfpm.yaml --target dist/`, so the
      package can also be built locally (nFPM isn't in the Arch repos: install it
      via `go install github.com/goreleaser/nfpm/v2/cmd/nfpm@<version>` or a
      release binary).

**4.2 `.github/workflows/ci.yml`** — on push to `main` and on PRs

- [ ] Runs in the `archlinux:base-devel` container, so it uses the same
      toolchain and libraries as the package.
- [ ] `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`,
      `cargo test --locked`.
- [ ] Cache `~/.cargo/registry`, `~/.cargo/git` and `target/` (`actions/cache`,
      keyed on `Cargo.lock`), since libcosmic is a large git dependency.
- [ ] Also run `just package` (build only, no publish) and upload the package as
      a workflow artifact, so `nfpm.yaml` breakage shows up before a release and
      every PR has a package to test.

**4.3 `.github/workflows/release.yml`** — on tag `v*`

1. [ ] Container `archlinux:base-devel` (so the binary links against current Arch
       libraries); `pacman -Syu --noconfirm rust just`. Install a **pinned** nFPM
       release binary from GitHub Releases and verify it against the release's
       checksums file.
2. [ ] Check that the tag `vX.Y.Z` equals the `Cargo.toml` version, otherwise fail;
       export `VERSION=X.Y.Z`.
3. [ ] `cargo test --locked`, then `just package` → `dist/*.pkg.tar.zst`.
4. [ ] `namcap` the package (warnings shown, not fatal at first). It flags
       missing or unneeded `depends`, which matters because they're kept by hand.
5. [ ] **Smoke test:** `pacman -U --noconfirm` the built package in the same
       container, check that the binary, desktop entry and icon exist, and run
       `cosmic-ext-applet-gcal-next --version` and `--once` against a fixture ICS file.
6. [ ] Generate `SHA256SUMS`; publish the `.pkg.tar.zst` + `SHA256SUMS` to a
       GitHub Release for the tag (`gh release create` with `GITHUB_TOKEN`,
       `permissions: contents: write`); release notes from the jj/git log
       between tags.

**4.4 Release procedure** (documented in the README)

- [ ] Bump `version` in `Cargo.toml`, describe the change,
      then `jj bookmark set main -r @-` and `jj git push`, then
      `jj tag set vX.Y.Z -r main` and `jj git push --tag vX.Y.Z`, which triggers
      `release.yml`.

**4.5 Optional follow-ups**

- [ ] Package signing: a detached `gpg --detach-sign` `.sig` next to the package
      (GPG key as an Actions secret), so it can be verified with `pacman-key`.
- [ ] A tiny pacman repository (`repo-add` → `gcal-next.db`) published as release
      assets or on GitHub Pages, so updates arrive via `pacman -Syu` instead of a
      manual `pacman -U`.
- [ ] `.deb` / `.rpm` packages almost for free with the same `nfpm.yaml`
      (`--packager deb|rpm` plus `overrides.<packager>.depends` for distro
      package names). Pop!_OS users are the obvious audience.
- [ ] AUR package (needs a real PKGBUILD; nFPM doesn't produce one).

## 8. Testing strategy

- **Unit:** `format.rs`, `state.rs` (inject `now` — never call `Utc::now()` inside
  pure logic), ICS expansion against fixtures.
- **Integration:** `--once` against a local fixture file (`file://` URL or
  `--ics-file` flag) so the full pipeline runs without network.
- **Manual:** a test calendar with events at +1 min, +5 min, +15 min, +45 min,
  +2 h to see every color/pulse transition in the real panel.
- **Resource check:** confirm the applet sits at ~0 % CPU outside the pulse state.

## 9. Open questions

1. Is the secret iCal address enabled on the target Google Workspace account?
2. During a meeting after the grace period: show the *next* meeting (current
   plan), or keep showing time remaining in the current one?
3. Should "No meeting" also show tomorrow's first meeting in the tooltip?
4. Should declined/tentative events count? (Current plan: skip declined, count tentative.)
