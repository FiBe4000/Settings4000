# Settings4000 — Requirements

A native settings GUI for the `~/.dotfiles`-managed Hyprland desktop. It reads/writes the underlying config files (per `docs/dotfiles_analysis.md`) and triggers the required reloads, replacing manual config editing for typical user-facing settings.

## 1. Technology stack

- **Language**: Rust (fast startup, single static-ish binary, memory safety).
- **UI toolkit**: GTK4 via `gtk4-rs` (optionally Relm4 for declarative components). Native Wayland support; first-class `GtkDropDown` and `GtkSwitch` widgets.
- **Rationale**: GTK apps pick up the system GTK theme automatically, which satisfies the styling requirement with zero custom theming code; the ecosystem is already GTK-leaning (swaync).

## 2. UI & styling

- **R2.1** The app uses the **system GTK theme** for its own styling — no custom CSS, no palette injection. It renders with whatever theme is active (via `GtkSettings`/`gsettings`), so it automatically matches the rest of the system.
- **R2.2** When the system GTK theme changes (including changes applied by the app itself), the app re-styles live **when a live propagation path is active** — on Hyprland this requires the settings portal (`xdg-desktop-portal` + `xdg-desktop-portal-gtk`) or the dconf GSettings backend, since GTK4 reads `settings.ini` only at startup; without either, restyle happens on next launch. Detection (R4.1) checks for the portal and the app must not claim live restyle when the path is absent.
- **R2.3** Widget conventions: **drop-down menus** for single-choice options (e.g. color scheme, monitor resolution), **toggle switches** for booleans (e.g. natural scroll, tap-to-click). Sliders permitted for continuous values (volume, sensitivity, timeouts). Multi-value settings (e.g. the ordered keyboard-layout list) may use editable lists (add/remove/reorder rows) instead of a drop-down.
- **R2.4** Layout: sidebar (or stack switcher) of categories, content pane per category. Must be usable on a 2880×1800@1.33-scaled laptop panel and external 1440p monitors.
- **R2.5** The header bar carries an app menu with an **About** entry showing the application name and icon, version, author, and a clickable link to the project's GitHub page (<https://github.com/FiBe4000/Settings4000>). Every displayed value derives from the crate package metadata (single source with the CLI `--version`, R8.6) — no duplicated literals.

## 3. Categories & settings scope

Scope is limited to typical user-facing settings; window rules, keybinds, and scripting stay in the dotfiles. Grouping:

| Category | Settings | Backing file(s) |
|---|---|---|
| **Display** | per-monitor resolution/refresh/scale/position/enable; laptop-display toggle — the toggle drives the **existing hotplug mechanism** (writes/removes `/tmp/hypr-laptop-display-forced` and triggers the same path as `scripts/hypr-monitor-hotplug`), never a `monitor=…,disable` record the watcher would fight; like volume it applies immediately (exempt from staging, R5.2) | `config/hypr/monitors.conf` — now the **single source** for eDP mode/scale (`scripts/hypr-display-profile.sh` derives its values by parsing this file, so the app edits the `monitor=` record, keeps it parseable, and never writes the script); `/tmp/hypr-laptop-display-forced` state file |
| **Sound** | output device, volume, mute; input device, mic volume/mute | runtime only via `wpctl`/`pactl` — no file writes |
| **Theme** | central color palette (drop-down over existing schemes in `colors/*`), system GTK theme, icon theme, cursor theme (drop-downs over installed themes), wallpaper path + fit mode (`fit_mode`, drop-down), lock-screen background | `colors/<scheme>` + `scripts/generate-colors`; GTK settings (see R3.2–R3.4); `hyprpaper.conf`; `hyprlock.conf`; cursor env (see R3.4) |
| **Input** | keyboard layouts (ordered editable list, entries sourced from the XKB registry `/usr/share/xkb/rules/evdev.xml`); keyboard options as curated switches for the options in use (`caps:escape`, `grp:win_space_toggle`) with unknown options preserved verbatim; mouse sensitivity, touchpad (natural scroll, tap-to-click, scroll factor) — cursor theme/size lives under **Theme** (R3.4) | `config/hypr/input.conf` `input{}` block (a `source=`d file split out of `hyprland.conf`) |
| **Notifications** | position, timeouts, do-not-disturb | `swaync/config.json` |
| **Power & Idle** | idle timeouts (dim / lock / dpms), lock command | `hypridle.conf` |
| **Network** | show connection status; deep management delegated to NetworkManager tools via an "Open Network Settings" button that spawns `kitty -e nmtui` (gated on the kitty capability) or `nm-connection-editor` when installed — `nm-applet` is already autostarted and `nmtui` needs a terminal, so a bare re-launch of either is not viable. Read-only in v1 | NetworkManager (runtime) |

- **R3.1** Sound and Network are runtime-backed (no dotfile), so their controls apply via system commands; volume/mute may apply immediately (exempt from staging, see R5.2).
- **R3.2** **Palette switching**: the drop-down lists the scheme files found in the dotfiles **color source** — the repo's `colors/` directory, located by resolving a deployed config symlink (R8.5), never a hardcoded path (currently `nord`, `everforest`). If fewer than two schemes exist, the control degrades to a read-only display of the active scheme; if the color source or `scripts/generate-colors` is absent altogether (e.g. a user without this dotfiles setup), the palette control is hidden entirely, like a missing app (R4). Applying runs the discovered repo's `scripts/generate-colors <scheme>` followed by the component reloads (R5.3). The active scheme is detected from the header line of the deployed generated file at its XDG path (`~/.config/hypr/colors.conf`: `# Generated from colors/<scheme> …`).
- **R3.3** **GTK theme switching**: the drop-down lists installed themes discovered in `~/.themes`, `~/.local/share/themes`, and `/usr/share/themes` (directories containing a `gtk-3.0/` or `gtk-4.0/` subdir, plus the built-in Adwaita variants). Applying sets `org.gnome.desktop.interface gtk-theme` via `gsettings` and writes `gtk-theme-name` in `~/.config/gtk-3.0/settings.ini` / `gtk-4.0/settings.ini` so both mechanisms agree. Note: `config/uwsm/env` has a commented-out `GTK_THEME` line — a set `GTK_THEME` env var overrides everything, so the app must warn if it detects one and not fight it.
- **R3.4** **Icon & cursor theme switching**: drop-downs alongside the GTK theme control. Discovery scans `~/.icons`, `~/.local/share/icons`, and `/usr/share/icons` for directories with an `index.theme`; entries containing a `cursors/` subdir populate the cursor drop-down, the rest the icon drop-down. Applying sets `org.gnome.desktop.interface icon-theme` / `cursor-theme` (+ `cursor-size`) via `gsettings` and mirrors `gtk-icon-theme-name` / `gtk-cursor-theme-name` / `gtk-cursor-theme-size` in the GTK 3/4 `settings.ini`. For the cursor to change everywhere on Hyprland, also run `hyprctl setcursor <theme> <size>` and update the `XCURSOR_THEME` / `XCURSOR_SIZE` definitions — the dotfiles now define these **consistently** as `Nordic-cursors`/16 across `config/uwsm/env` (canonical) and `config/hypr/hyprland.conf` (with the GTK `settings.ini` and `scripts/launchhyprland.sh` also aligned); the app must keep every copy identical whenever it changes them.

## 4. Dynamic visibility (installed-app detection)

- **R4.1** At startup, detect presence of each target application before showing its settings: `hyprctl`, `kitty`, `eww`, `swaync`, `hyprpaper`, `hypridle`, `hyprlock`, `pactl`/`wpctl`, `gsettings`, NetworkManager, the settings portal (`xdg-desktop-portal-gtk` or a dconf backend — gates the live-restyle claim, R2.2), and the **dotfiles palette source** (the repo `colors/` dir + `scripts/generate-colors`, located by resolving a deployed config symlink — gates the palette switcher, R3.2/R8.5). Detection = binary on `$PATH` (`which`-equivalent), plus daemon liveness where relevant (e.g. Hyprland IPC socket).
- **R4.2** If an application is missing, its settings (rows, or the whole category if emptied) are cleanly hidden — no greyed-out stubs, no crashes, no error dialogs. Hidden state must be logged at `info` level.
- **R4.3** Detection results are computed once per launch (with a manual refresh path acceptable); absence of any optional app must never block startup.
- **R4.4** The app must tolerate missing/unreadable config files for a detected app: hide the affected controls and log a `warn`.

## 5. Functionality: staged edits & Apply

- **R5.1** All file-backed edits are **staged in memory** until the user clicks **Apply**. The UI must indicate dirty state (e.g. highlighted Apply button, per-page modified markers) and offer **Reset** to discard staged changes.
- **R5.2** Exception: ephemeral runtime controls (volume/mute, the laptop-display toggle) may apply immediately, since they touch no dotfiles.
- **R5.3** On Apply, the app must:
  1. Rewrite **specific values** in the underlying files — targeted key/line edits that preserve comments, ordering, and unrelated content (never regenerate whole hand-written files). Generated color files are never written directly; the app edits `colors/<scheme>` and runs `scripts/generate-colors`.
  2. Execute the required reload commands, only for running/installed components: `hyprctl reload`, `eww reload`, `swaync-client -R` (config reload; `-rs` reloads only its CSS and is used after a palette switch regenerates `swaync/colors.css`), `kill -SIGUSR1 $(pidof kitty)`, `hyprctl hyprpaper ...`, restart `hypridle` when its config changed.
- **R5.4** Apply must be transactional per file: write to a temp file in the same directory and atomically rename over the resolved target — **following symlinks** so a file deployed as a symlink (e.g. into a dotfiles repo) has its real target rewritten with the link preserved, never replaced; on any write failure, roll back and report the error without leaving partial configs.
- **R5.5** Reload failures (command missing, non-zero exit) must be surfaced non-fatally in the UI and logged at `error`; the file write still stands.
- **R5.6** Before staging, the app re-reads files on focus/refresh so external edits aren't silently clobbered; on conflict (file changed since read), warn and re-load.

## 6. Testing

- **R6.1** Automated tests are a build requirement (`cargo test` in CI/pre-commit):
  - **Unit tests** for every parser/writer (colors kv, hyprlang key edits, `monitors.conf` records, swaync JSON, env files) — round-trip tests asserting comments/ordering preservation.
  - **Integration tests** running against fixture copies of the real dotfiles in a temp dir, asserting staged-edit → Apply produces exact expected file contents; reload commands mocked/injected.
  - **Detection tests** for the dynamic-visibility logic with a fake `$PATH`.
- **R6.2** UI logic (staging state machine, dirty tracking) must be separated from GTK widgets enough to be testable headlessly.

## 7. Logging

- **R7.1** Use standard Linux system logging: log to the systemd journal (journald) via the `tracing` crate with a journald layer (stderr fallback when journald is unavailable).
- **R7.2** Configurable log levels — `debug`, `info`, `warn`, `error` — settable via CLI flag (`--log-level`) and env var (`RUST_LOG`/`SETTINGS4000_LOG`); the flag takes precedence over the env vars.
- **R7.3** Must log: startup detection results, every file write (path + keys changed, never full secrets/contents at `info`), every reload command + exit status, and all errors. `debug` includes parsed values and staged diffs.

## 8. Non-functional

- **R8.1** Cold startup to interactive window < 500 ms on the target hardware.
- **R8.2** No root privileges; operates only on user-owned configuration at its standard runtime locations (`$XDG_CONFIG_HOME`/`~/.config` and the equivalent per-app paths) — plus the dotfiles palette source when present (R8.5) — and user-session commands.
- **R8.3** The app must never break a working desktop: invalid staged values are validated before Apply (e.g. hex color format, resolution strings, timeout ranges, and file paths — wallpaper/lock-background paths must exist, be readable, and have an image extension).
- **R8.4** Single instance (activate existing window on relaunch).
- **R8.6** **Versioning**: the application carries a semantic version whose single source is `Cargo.toml`'s `version` field; the CLI `--version`, the startup log, and the About window (R2.5) all derive from it at compile time (`CARGO_PKG_VERSION`), together with the author and repository URL (`CARGO_PKG_AUTHORS`/`CARGO_PKG_REPOSITORY`). Version **1.0.0** marks the feature-complete release that includes the About menu; later releases bump per semver.
- **R8.5** **File addressing & portability**: the app locates each backing file by its standard runtime path (`$XDG_CONFIG_HOME`/`~/.config` and the equivalent swaync/GTK/uwsm paths), **never** by assuming a `~/.dotfiles` repo. Writes follow symlinks (R5.4), so a file symlinked into a dotfiles repo and a plain real file are handled identically — the app targets the live location and the OS resolves where the bytes land. Repo-only sources with no XDG location (the palette `colors/` dir, `scripts/generate-colors`, `theme/fonts`) are found relative to the repo root discovered by canonicalizing a deployed config symlink; when that resolution fails or the source is missing, the dependent controls are hidden exactly like a missing app (R4).

## 9. Out of scope (v1)

- Editing keybinds, window rules, animations, eww widget layout (`eww.yuck`/`eww.scss` bodies), zsh config.
- Per-key palette color editing / creating new color schemes (v1 only switches between existing scheme files).
- Qt theme management and GTK theming beyond selecting an installed theme (no theme generation from the palette).
- Full network configuration UI (delegated to NetworkManager tools).
- The dotfile restructurings suggested in `docs/dotfiles_analysis.md` §5 (apply-wrapper, sourced `input.conf`, font templating) are prerequisites/companions — tracked separately in `~/.dotfiles/settings_app_prep_tasks.md` and **now implemented** (see `docs/dotfiles_analysis.md` §6) — not app features.
