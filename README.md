# Settings4000

A native GTK 4 settings GUI (Rust + [Relm4](https://relm4.org/)) for a
dotfiles-managed [Hyprland](https://hypr.land/) desktop. This project is also
an experiment: everything in it — the requirements, the architecture, the
documentation, and the code — was written with
[Claude Code](https://claude.com/claude-code). It edits the config
files behind the common user-facing settings — display, sound, theme/palette,
input, notifications, power/idle, network — with **surgical, comment-preserving
edits** (never regenerating a hand-written file), and triggers the right live
reloads (`hyprctl reload`, `swaync-client -rs`, hypridle restart, …) so changes
take effect without a relogin. File-backed edits are staged and written
atomically on **Apply**, with validation before any write and per-file rollback
on failure; controls for tools that aren't installed are simply hidden.

## Dependencies

### Build

- **Rust** ≥ 1.85 (the `rust-version` in `Cargo.toml`; edition 2024). CI builds
  on the current stable toolchain. Distribution-packaged compilers are often
  older than the MSRV; install via [rustup](https://rustup.rs) if in doubt.
- **GTK 4 development libraries**, version ≥ 4.10 (the crate enables the
  `v4_10` API), found via `pkg-config`:
  - Arch Linux: `pacman -S gtk4` (headers are included; you also need
    `base-devel` for a C toolchain and `pkgconf`).
  - Debian/Ubuntu: `apt install libgtk-4-dev` (what CI installs; on a minimal
    system add `build-essential` and `pkg-config`). Needs Debian 13+ /
    Ubuntu 24.04+ — Debian 12 (bookworm) ships GTK 4.8, below the required
    4.10.

### Runtime (all optional)

The app works against whatever is present. At startup it scans `$PATH` and
checks daemon liveness (`core/detect.rs`); controls whose backing tool is
missing are hidden — never greyed out — and their absence is logged. None of
these are build dependencies:

| Tool | Used for |
|---|---|
| `hyprctl` | Hyprland reloads, monitor info, `setcursor`, hyprpaper control |
| `wpctl` / `pactl` | Sound page (volume, mute, devices) |
| `swaync` | Notifications page (reloads run through `swaync-client`) |
| `hypridle`, `hyprlock`, `hyprpaper` | Power & Idle, lock background, wallpaper |
| `nmcli`, `nm-connection-editor` | Network status page + settings launcher |
| `gsettings`, `dconf` | GTK theme/cursor propagation, live-restyle detection |
| `kitty`, `eww` | Palette reload targets (SIGUSR1 / `eww reload`) |

## Build, run, test

```sh
cargo build
cargo run                       # or: cargo run -- --log-level debug
```

The CI gate (also a sensible pre-commit check) is exactly:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Plain `cargo test` runs everything: the unit tests plus the integration suites
in `tests/` (per-category staged-edit → Apply against a fixture dotfiles tree,
module-boundary and no-custom-CSS guards). The `testing` cargo feature — the
fixture installer and mock seams — is enabled for test targets automatically
via a self-referential dev-dependency, so no extra flags are needed.

A pre-commit hook mirroring the gate ships in `.githooks/pre-commit`; opt in
with `git config core.hooksPath .githooks`.

Two documented runtime checks beyond the test suite:

- `cargo run --example logging_walkthrough` drives a full Apply cycle
  headlessly against a temporary fixture tree with the real journald logging;
  `docs/logging_audit.md` walks the resulting journal output.
- `docs/benchmarks.md` describes the startup-time measurement against the
  500 ms cold-start budget (the binary logs a `first frame painted … ms` mark).

## Install

```sh
./install.sh              # build --release and install for the current user
./install.sh --uninstall  # remove exactly what was installed
```

No root. The script installs three files: the binary to
`~/.local/bin/settings4000`, a desktop entry to
`$XDG_DATA_HOME/applications/org.settings4000.Settings4000.desktop` (with its
`Exec=` rewritten to the absolute binary path), and an SVG icon under
`$XDG_DATA_HOME/icons/hicolor/scalable/apps/`. The desktop entry makes the app
launchable from rofi/wofi-style launchers; the app is single-instance (fixed
GApplication ID `org.settings4000.Settings4000`), so relaunching it activates
the existing window instead of starting a second process.

## Logging

Logs go to the systemd journal (tag `settings4000`), with stderr as fallback
when journald is unavailable:

```sh
journalctl --user -t settings4000
```

Verbosity: the `--log-level {debug,info,warn,error}` flag overrides the
`SETTINGS4000_LOG` env var, which overrides `RUST_LOG`; the default is `info`.
`--log-level debug` raises only this crate's level (`info,settings4000=debug`);
the quieter levels apply globally, and the env vars accept full `tracing`
filter directives.

## Relation to the dotfiles

The app addresses every config file by its **live XDG path**
(`$XDG_CONFIG_HOME`/`~/.config/…`), never a hardcoded `~/.dotfiles` path, and
writes atomically **following symlinks** — so a file deployed as a symlink into
a dotfiles repo has its real target rewritten with the link preserved, and a
plain file is rewritten in place. It therefore works with or without a
dotfiles deployment. The few repo-only sources with no XDG location (the
palette `colors/` directory, `scripts/generate-colors`, `theme/fonts`) are
found by resolving a deployed symlink back to the repo root; when that fails,
the palette controls are hidden like any missing tool.

Before this app could safely edit the dotfiles, the dotfiles themselves needed
some restructuring — extracting the `input { }` block into an app-owned
`config/hypr/input.conf`, unifying the duplicated cursor theme/size and
wallpaper/lock-background values, single-sourcing the eDP display profile from
`monitors.conf`, and adding the `theme/fonts` source. Those prep tasks are
tracked in the dotfiles repo itself, in
`~/.dotfiles/settings_app_prep_tasks.md` (all completed); the resulting layout
the app targets is documented in `docs/dotfiles_analysis.md` §6.

## Documentation

- `docs/requirements.md` — what to build: the numbered **R…** requirements
  referenced throughout the code and tests.
- `docs/architecture.md` — how it is built: module layout, parser strategies,
  the Apply pipeline, detection.
- `docs/tasks.md` — the ordered implementation breakdown with acceptance
  criteria and per-task completion notes.
- `docs/dotfiles_analysis.md` — the concrete dotfiles layout the app
  reads/writes (§6 is the current, post-prep state).
- `docs/benchmarks.md` — startup-time benchmark procedure and recorded values.
- `docs/logging_audit.md` — the R7.3 logging-coverage audit with a real
  journal walkthrough.
