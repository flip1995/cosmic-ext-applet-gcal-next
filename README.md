> [!WARNING]
> This is fully vibe-coded. Not a single LoC has been reviewed. It works for me. Good luck.

# cosmic-ext-applet-gcal-next

A deliberately minimal COSMIC panel applet: a single pill in the top bar that
shows the time until your next meeting from **one** Google Calendar, changes
color as the meeting approaches and pulses when it is imminent.

- Only meetings starting within the next 2 hours count (configurable).
- **Click** the pill for a popup with the meeting's title and time, *Join*
  (opens the Meet/Zoom/Teams link, or the day in Google Calendar), *Refresh
  now*, the sync status and a *Settings…* page: e-mail, refresh interval,
  lookahead, filters, and the warning levels with their thresholds, colors and
  pulsing.
- On vertical panels the pill becomes a colored dot.

See [`CLAUDE.md`](CLAUDE.md) for an overview of the implementation and the
design decisions.

## Install

Arch Linux: download the package from the
[releases](https://github.com/flip1995/cosmic-ext-applet-gcal-next/releases), e.g.

```sh
sudo pacman -U https://github.com/flip1995/cosmic-ext-applet-gcal-next/releases/download/v1.0.0/cosmic-ext-applet-gcal-next-1.0.0-1-x86_64.pkg.tar.zst
```

From source (user-local, `~/.local`): `just install`.

Then add **Next Meeting** in *Settings → Desktop → Panel → Applets*.

## Setup

1. In Google Calendar, open *Settings → (your calendar) → Integrate calendar*
   and copy the **Secret address in iCal format**.
2. Store it, readable only by you (the applet refuses files with broader
   permissions):

   ```sh
   mkdir -p ~/.config/cosmic-ext-applet-gcal-next
   install -m 600 /dev/null ~/.config/cosmic-ext-applet-gcal-next/ical-url
   $EDITOR ~/.config/cosmic-ext-applet-gcal-next/ical-url
   ```

   The URL gives read access to your calendar to anyone who has it. It is never
   logged.

   Alternatively, store it in the Secret Service keyring (GNOME Keyring,
   KeePassXC, …) — the file takes precedence if both exist:

   ```sh
   cosmic-ext-applet-gcal-next --set-url   # paste the URL, press Enter
   ```

3. Optionally set your e-mail address so events you declined are skipped
   (or enter it in the popup's *Settings…* page):

   ```sh
   mkdir -p ~/.config/cosmic/io.github.flip1995.CosmicExtAppletGcalNext/v1
   echo 'Some("you@example.com")' > ~/.config/cosmic/io.github.flip1995.CosmicExtAppletGcalNext/v1/my_email
   ```

## Usage

```sh
cosmic-ext-applet-gcal-next --once                  # print `state label | title | start`
cosmic-ext-applet-gcal-next --once --ics-file f.ics --now 2026-11-10T10:00:00+01:00
cosmic-ext-applet-gcal-next --print-default-config  # the default settings
```

Set `RUST_LOG=debug` for more log output (on stderr).

## Configuration

Settings live in `~/.config/cosmic/io.github.flip1995.CosmicExtAppletGcalNext/v1/`,
one RON file per key. `--print-default-config` prints all keys with their
defaults. Changes apply live while the panel is running; most of them can also
be made in the popup's *Settings…* page. The time thresholds, the color of each tier and whether it pulses are
configurable, e.g. the `tiers` key (a level applies while the pill shows at
most `within_min` minutes, so `1` is the last minute):

```ron
[
    (within_min: 30, style: (color: Theme(Warning),     pulse: false)),
    (within_min: 10, style: (color: Rgba("#F5841F"),    pulse: true)),
    (within_min: 1,  style: (color: Theme(Destructive), pulse: true)),
]
```

Theme colors: `Neutral`, `Accent`, `Success`, `Warning`, `Destructive`.

## Development

```sh
just check                 # fmt, clippy, tests
just once --ics-file tests/fixtures/google.ics --now 2026-11-10T09:55:00+01:00
just run --ics-file f.ics  # the applet in a window, with debug logging
just install restart-panel # try it in the real panel
just package               # Arch package into dist/ (needs nfpm)
```

### Releasing

1. Bump `version` in `Cargo.toml` (and run `cargo check` to update
   `Cargo.lock`), describe the change.
2. `jj bookmark set main -r @-` and `jj git push`.
3. `jj tag set vX.Y.Z -r main` and `jj git push --tag vX.Y.Z`.

The tag triggers `.github/workflows/release.yml`, which tests, packages,
smoke-tests and publishes the `.pkg.tar.zst` with a `SHA256SUMS` file.

## Code of Conduct

This repository adopts the [Contributor Covenant Code of
Conduct](https://www.contributor-covenant.org/version/1/4/code-of-conduct/)

## License

Copyright 2026 Philipp Krones

Licensed under the Apache License, Version 2.0 or the MIT license, at your
option. Files in the project may not be copied, modified, or distributed except
according to those terms.
