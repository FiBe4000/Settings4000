# Settings4000

A native GTK 4 settings GUI (Rust + [Relm4](https://relm4.org/)) for a
dotfiles-managed [Hyprland](https://hypr.land/) desktop. This project is also
an experiment: everything in it — the requirements, the architecture, the
documentation, and the code — was written with
[Claude Code](https://claude.com/claude-code). It edits the config
files behind the common user-facing settings — display, sound, theme/palette,
input, notifications, power/idle, network — with **surgical, comment-preserving
edits** (never regenerating a hand-written file), and triggers the right live
reloads (`hyprctl reload`, `eww reload`, hypridle restart, …) so changes
take effect without a relogin. Runtime-only controls — volume and devices, the
laptop-display toggle, and the notification daemon's do-not-disturb / clear /
notification-center actions — apply immediately instead. File-backed edits are
staged and written atomically on **Apply**, with validation before any write and
per-file rollback on failure; controls for tools that aren't installed are
simply hidden.

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

The app works against whatever is present. At startup it scans `$PATH`,
checks daemon liveness and asks the session bus whether the notification
daemon runs (`core/detect.rs`); controls whose backing tool is missing are
hidden — never greyed out — and their absence is logged. None of these are
build dependencies:

| Tool | Used for |
|---|---|
| `hyprctl` | Hyprland reloads, monitor info, `setcursor`, hyprpaper control |
| `wpctl` / `pactl` | Sound page (volume, mute, devices) |
| `notifd` (+ `busctl`) | Notifications page: do-not-disturb, notification count, clear all, open the center — detected by its D-Bus name `se.dotfiles.Notifd` (running) or its `~/.config/systemd/user/notifd.service` unit (installed); with notifd ≥ 1.1 the page also edits the seven settings of `~/.config/notifd/config` (popup timeouts, history size, critical popups per app, hold time, 2FA copy), and its `Reload()` method follows a write of that file as well as an icon-theme change; every call goes through `busctl` with auto-start disabled |
| `hypridle`, `hyprlock`, `hyprpaper` | Power & Idle, lock background, wallpaper |
| `nmcli`, `nm-connection-editor` | Network status page + settings launcher |
| `gsettings`, `dconf` | GTK theme/cursor propagation, live-restyle detection |
| `kitty`, `eww` | Palette reload targets (SIGUSR1 / `eww reload`); with eww running, the bar's `~/.config/eww/scripts/bar-text-tone --apply` after a wallpaper change |

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

GitHub Actions runs that gate on every push to `main` and every pull request,
and a second job builds the Arch package (below) in an `archlinux:latest`
container — checking that `.SRCINFO` is current, running the full test suite in
the package's `check()`, and failing on any `namcap` warning outside its
allow-list.

A pre-commit hook mirroring the gate ships in `.githooks/pre-commit`; opt in
with `git config core.hooksPath .githooks`.

Two documented runtime checks beyond the test suite:

- `cargo run --example logging_walkthrough` drives a full Apply cycle
  headlessly against a temporary fixture tree with the real journald logging;
  `docs/logging_audit.md` walks the resulting journal output.
- `docs/benchmarks.md` describes the startup-time measurement against the
  500 ms cold-start budget (the binary logs a `first frame painted … ms` mark).

## Install

There are two install paths: an Arch Linux package, and a per-user script for
everything else. **Do not mix them** — see the warning at the end of this
section.

### Arch Linux: the `settings4000-git` package

```sh
cd packaging/arch/settings4000-git
makepkg -si          # builds the committed tip of main from GitHub and installs it
pacman -R settings4000-git   # as root: removes the package again
```

`makepkg -s` installs the build dependencies (`cargo`, `git`) and the runtime
dependencies with pacman, and `-i` installs the built package. The package
installs four files:

- `/usr/bin/settings4000` — the binary;
- `/usr/share/applications/org.settings4000.Settings4000.desktop` — the
  desktop entry, verbatim from `data/`;
- `/usr/share/icons/hicolor/scalable/apps/org.settings4000.Settings4000.svg` —
  the icon;
- `/usr/share/licenses/settings4000-git/LICENSE` — the MIT license text.

The external tools the app drives (`hyprland`, `hyprpaper`, `hypridle`,
`hyprlock`, `wireplumber`, `pipewire`, `libpulse`, `eww`, `kitty`,
`networkmanager`, `nm-connection-editor`, `dconf`) are recorded as
`optdepends`, not `depends`: the app detects each one at startup and hides the
settings that need a missing tool instead of failing (R4), so none of them is
required to install or start it. `pacman -Qi settings4000-git` lists them with
what each one enables.

To package your local clone instead of GitHub's `main`, override the source
repository:

```sh
cd packaging/arch/settings4000-git
_repo="file://$(git rev-parse --show-toplevel)" makepkg -si
```

This builds the local clone's *committed* `HEAD`; uncommitted changes in the
working tree are not included. makepkg keeps its clone in
`packaging/arch/settings4000-git/settings4000/` and only fetches into it on
later runs, so delete that directory when switching between the GitHub and the
local source, or between branches.

Two things to know when building:

- **Restore the PKGBUILD afterwards.** The version comes from `Cargo.toml`
  through the PKGBUILD's `pkgver()` function; the committed `pkgver=0` line is
  only the placeholder makepkg requires, and makepkg rewrites it in place with
  the computed version after every build. That edit is build output, not a
  change to commit: discard it with
  `git restore packaging/arch/settings4000-git/PKGBUILD`. Until you do, a guard
  test in `cargo test` fails.
- **A rustup toolchain outside pacman.** If your `cargo` comes from a rustup
  install made outside pacman (for example the rustup.rs script) rather than
  pacman's `rust` package, pacman does not know about it, so makepkg treats the
  `cargo` build dependency as missing. Either install the `rust` package, or
  build with `makepkg -i --nodeps`, which skips makepkg's dependency checks
  (pacman still resolves the runtime dependencies when it installs the
  package).

### Any distribution, no root: `install.sh`

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

### Do not mix the two

Both paths install a desktop entry with the same name, and a per-user entry
under `~/.local/share/applications` shadows the system one in
`/usr/share/applications` — the launcher would start the `~/.local/bin` copy
even after the package is installed. Run `./install.sh --uninstall` before
installing the package, and `pacman -R settings4000-git` before using
`install.sh`.

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

**Status (2026-10-21):** the dotfiles have since moved to a Lua Hyprland
config (`hyprland.lua` + app-owned `input.lua`; the hyprlang `.conf` files are
gone) and replaced their notification daemon with a self-written `notifd`
that has its own small config file. The new layout is analysed in
`docs/dotfiles_analysis.md` §7–§8 and the re-alignment work is the §11 backlog
in `docs/tasks.md`. The Input page, the cursor copies, the palette schema and
the Notifications page's runtime controls already follow it; the remaining
§11 items (among them the laptop-display toggle running the dotfiles script)
are still open, and
until they land those parts of the app keep their earlier behaviour.

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

## License

Settings4000 is released under the MIT License; see [`LICENSE`](LICENSE).
