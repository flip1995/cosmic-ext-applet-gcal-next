name := 'cosmic-ext-applet-gcal-next'
app_id := 'io.github.flip1995.CosmicExtAppletGcalNext'

# User-local install locations (`just install`).
bin_dir := env('HOME') / '.local/bin'
data_dir := env('XDG_DATA_HOME', env('HOME') / '.local/share')

# List the recipes.
default:
    @just --list

# fmt, clippy and tests, like CI.
check:
    cargo fmt --check
    cargo clippy --locked --all-targets -- -D warnings
    cargo test --locked

test:
    cargo test

# Print the current state once, e.g. `just once --ics-file tests/fixtures/google.ics`.
once *ARGS:
    cargo run -- --once {{ARGS}}

# Run the applet outside the panel (in a window), e.g. `just run --ics-file f.ics`.
run *ARGS:
    RUST_LOG=${RUST_LOG:-cosmic_ext_applet_gcal_next=debug,warn} cargo run -- {{ARGS}}

build-release:
    cargo build --release --locked

# Install for the current user. Then add "Next Meeting" in
# Settings → Desktop → Panel → Applets (or restart the panel after updates).
install: build-release
    install -Dm755 target/release/{{name}} {{bin_dir}}/{{name}}
    # Absolute Exec path: the panel's PATH may not contain ~/.local/bin.
    install -d {{data_dir}}/applications
    sed 's|^Exec=.*|Exec={{bin_dir}}/{{name}}|' resources/app.desktop > {{data_dir}}/applications/{{app_id}}.desktop
    install -Dm644 resources/icon.svg {{data_dir}}/icons/hicolor/scalable/apps/{{app_id}}.svg

uninstall:
    rm -f {{bin_dir}}/{{name}} {{data_dir}}/applications/{{app_id}}.desktop {{data_dir}}/icons/hicolor/scalable/apps/{{app_id}}.svg

# Restart the panel so it picks up a new binary.
restart-panel:
    pkill -x cosmic-panel || true

# Build the Arch package into dist/ (needs `nfpm` on PATH).
package: build-release
    #!/usr/bin/env bash
    set -euo pipefail
    export VERSION="${VERSION:-$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)}"
    mkdir -p dist
    nfpm package --packager archlinux --config nfpm.yaml --target dist/
    ls -l dist/
