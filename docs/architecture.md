# Settings4000 — Technical Architecture

Companion to `docs/requirements.md` (R-numbers referenced throughout) and `docs/dotfiles_analysis.md`.

## 1. Technology stack

| Concern | Choice | Notes |
|---|---|---|
| Language | **Rust** (stable, 2024 edition) | Single binary, fast cold start (R8.1) |
| GUI | **GTK4 via `gtk4-rs`, structured with Relm4** | Relm4 components give declarative message-driven UI; the core state machine lives outside widgets (R6.2) |
| Testing | **`cargo test`** (built-in harness) + `tempfile` for fixture dirs, `assert_matches`/snapshot comparison for round-trip tests | No extra runner; CI/pre-commit gate is plain `cargo test` (R6.1) |
| Logging | **`tracing`** + **`tracing-journald`** layer, `tracing-subscriber` fmt layer to stderr as fallback | journald per R7.1; `EnvFilter` honors `SETTINGS4000_LOG`/`RUST_LOG` and `--log-level` (R7.2) |
| Single instance | `gtk::Application` with a fixed application ID | GApplication D-Bus activation raises the existing window (R8.4) |

## 2. Crate / module layout

```
settings4000/
├── src/
│   ├── main.rs            # CLI args, logging init, gtk::Application bootstrap
│   ├── ui/                # Relm4 components: window, sidebar, category pages
│   ├── core/              # GTK-free, fully unit-testable (R6.2)
│   │   ├── store.rs       # SettingsStore: staged edits, dirty tracking, conflict detection
│   │   ├── detect.rs      # installed-app detection
│   │   ├── apply.rs       # transactional writes + reload orchestration
│   │   └── model.rs       # typed Setting values + validation (R8.3)
│   ├── parsers/           # one module per file format (see §3)
│   └── system/            # side-effect boundary: CommandRunner trait, file IO, journald
└── tests/                 # integration tests against fixture dotfiles
```

Hard rule: `core/` and `parsers/` never import `gtk`. All process execution goes through a `CommandRunner` trait so integration tests inject a mock recorder (R6.1).

## 3. Dotfile parsing & safe modification

Principle: **surgical, span-preserving edits** — never serialize a whole file from a model (R5.3.1). Each parser produces a lossless line/token representation; the writer replaces only the value span of a targeted key and re-emits everything else byte-identical. Every parser has round-trip tests (`parse → edit nothing → emit == input`).

Per-format parsers (`parsers/`):

| Format | Files | Strategy |
|---|---|---|
| Palette kv | `colors/<scheme>` | Line-oriented `key=hex`; edit value in place. Validated against the fixed 17-key schema before write |
| hyprlang | `config/hypr/input.conf` (the `input { }` block), `hyprland.conf` (cursor `env=`), `hypridle.conf`, `hyprlock.conf`, `hyprpaper.conf` | Tokenize into lines: comments, `key = value`, `section {` / `}`, `source=`. Edits address a key by *section path* (e.g. `input.touchpad.natural_scroll`) and rewrite only that line's value. Note the `input { }` block was extracted into its own `source=`d `input.conf` (analysis §6.3), so `input.*` section paths resolve there, **not** in `hyprland.conf`; the cursor `env=` lines remain in `hyprland.conf`. **Repeatable top-level keys** (`env=`, `exec-once=`) are addressed by key + first comma-field (e.g. `env:XCURSOR_THEME` matches the `env = XCURSOR_THEME,…` line) and only the value portion after that field is edited. Commented-out lines are never matched. New keys are appended at the end of their section |
| monitors.conf | `config/hypr/monitors.conf` | Parse `monitor=` records (name/desc, mode, position, scale) preserving surrounding lines. Later-rule-wins semantics respected: edits modify the matching record for the target monitor, or append after the last `monitor=` line. The app does not touch `hypr-display-profile.sh`, which now **derives** its eDP mode/scale by parsing this file (analysis §6.2) — so editing the `monitor=` record automatically feeds hotplug/toggle and no divergence warning is needed. The edited record must stay parseable by the script's `awk`: leading `key,` token, mode as field 2, scale as field 4, extras (e.g. `bitdepth,10`) after |
| JSON | `swaync/config.json` | `serde_json` with `preserve_order`; JSON carries no comments here, so parse–modify–reserialize (pretty, 2-space) is acceptable |
| INI | `gtk-3.0/settings.ini`, `gtk-4.0/settings.ini` | Minimal line-preserving kv-in-section editor. Both files are now tracked in the dotfiles and symlinked (analysis §6.5), so editing an existing `[Settings]` section is the norm; the create-file-if-absent path remains a fallback for non-dotfiles users |
| env file | `config/uwsm/env` | Targeted `export KEY=value` line edits for `XCURSOR_THEME`/`XCURSOR_SIZE`; commented lines detected but not modified (used for the `GTK_THEME` warning, R3.3) |

Generated files are **read-only inputs** — never written directly. `generate-colors` produces **six color partials** (`config/hypr/colors.conf`, `eww/_colors.scss`, `swaync/colors.css`, `rofi/colors.rasi`, `kitty/colors.conf`, `zsh/colors.zsh`), **three font partials** (`kitty/fonts.conf`, `eww/_fonts.scss`, `rofi/fonts.rasi` — templated from `theme/fonts`, analysis §6.5), and the `state/active-scheme` marker. The app reads only the color partial's `# Generated from colors/<scheme>` header to detect the active scheme (R3.2), and optionally the marker; fonts are out of v1 scope (requirements §9). All palette changes go through editing `colors/<scheme>` + running `scripts/generate-colors`.

### Write safety (R5.4, R5.6)

- On first read, the store records each file's content hash + mtime. Before Apply (and on window focus/manual refresh) files are re-read; a mismatch triggers a conflict warning and reload of staged views instead of a silent clobber.
- The app **addresses every backing file by its XDG runtime path** (`$XDG_CONFIG_HOME`/`~/.config`, and the equivalent per-app paths), never a hardcoded `~/.dotfiles` path (R8.5) — so it behaves identically for this symlink-deployed setup and for a user with plain config files. Apply writes each file as: write full new content to `NamedTempFile` in the same directory as the *resolved* target → fsync → atomic `rename` over that target. The writer `fs::canonicalize`s first: when the XDG path is a symlink into a dotfiles repo it rewrites the repo file and leaves the link intact; when it is a plain file it rewrites it in place. Either way the temp file lives beside the real target so the rename is atomic and cross-device-safe.
- Apply is per-file transactional: files are written in sequence; on any failure, already-written files are restored from their pre-apply snapshots (kept in memory), the error is surfaced, and staged state is retained for retry.
- Validation (R8.3) runs on the typed model before any write: hex format, monitor mode strings (`WxH@Hz`), numeric ranges for timeouts/sensitivity/scale, and file paths (exists + readable + image extension for wallpaper/lock background).

## 4. Installed-app detection & dynamic visibility (R4)

`core/detect.rs` runs once at startup (async, before window population; re-runnable via a refresh action, R4.3):

- **Binary presence**: manual `$PATH` scan (executable file check per dir) rather than shelling out to `which` — testable with an injected `PATH` string (R6.1 detection tests).
- **Daemon liveness** where relevant: Hyprland via existence of `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket.sock` (plus `hyprctl` on `$PATH` — nearly every reload needs it); swaync/hypridle/hyprpaper/eww/kitty via `pidof`-equivalent (procfs scan); NetworkManager via `nmcli` presence; settings-portal availability (`xdg-desktop-portal-gtk` process or dconf backend) for the R2.2 live-restyle claim.
- **Dotfiles palette source** (R3.2, R8.5): canonicalize a deployed config symlink (e.g. `~/.config/hypr/colors.conf`) to find the repo root, then check for `colors/` + `scripts/generate-colors`. A non-symlinked config (i.e. no dotfiles repo behind it) or a missing source gates the palette switcher off — it is hidden like a missing app rather than pointing at a guessed path.
- **Config readability** (R4.4): each backing file is checked readable+parseable at its XDG path; failures hide the affected controls and log `warn`.

The result is a `Capabilities` struct (plain data). Each settings row/page declares its required capability; the UI builder skips rows whose capability is absent and hides a category entirely when all its rows are gone (R4.2), logging each hidden item at `info`. No detection failure aborts startup — detection errors degrade to "absent".

## 5. System color / theme ingestion (R2)

The app performs **no palette ingestion for its own styling**: as a GTK4 app it inherits the active system GTK theme automatically (R2.1). No custom CSS is shipped. **Live** re-styling on theme change (R2.2) is *not* native on Hyprland: GTK4 reads `settings.ini` only at startup and picks up runtime `org.gnome.desktop.interface` changes only via the settings portal (`xdg-desktop-portal` + `xdg-desktop-portal-gtk`) or the dconf GSettings backend. Detection checks for that path; when absent, theme changes take effect at next launch and the UI says so (see §10).

Colors are read only as *data* for the Theme page:

- Active palette scheme: header line of the deployed `~/.config/hypr/colors.conf` (R3.2). Available schemes: directory listing of the dotfiles `colors/` dir, located by canonicalizing a deployed config symlink (`~/.config/hypr/colors.conf` → `<repo>/config/hypr/colors.conf` → repo root) rather than assuming `~/.dotfiles`; if that resolution fails (no dotfiles repo behind the config), the palette switcher is an absent capability and hidden (R4, R8.5).
- Current GTK/icon/cursor theme: `gsettings get org.gnome.desktop.interface {gtk-theme,icon-theme,cursor-theme,cursor-size}`, cross-checked against `settings.ini`; installed themes discovered per R3.3/R3.4 directory scans. (Decision: v1 shells out to `gsettings` through `CommandRunner` for uniform logging/mocking; switching to in-process `gio::Settings` — dropping the binary dependency and its detection entry — is a noted later simplification.)
- A `GTK_THEME` env var, if set in the app's own environment (note `scripts/launchhyprland.sh` exports it uncommented, so it may be present in the session env — analysis §6.3) or found uncommented in `config/uwsm/env`, triggers a persistent info banner and disables the GTK-theme drop-down (R3.3 — never fight the override).

Optional nicety: the palette drop-down renders small swatches per scheme by parsing each `colors/<scheme>` file — display only, no styling impact.

## 6. Apply pipeline & reload mechanism (R5)

Staging: the `SettingsStore` holds `original` and `staged` typed values per setting; dirty = any difference. Runtime-only controls bypass the store (R5.2) and execute immediately: volume/mute via `wpctl set-volume`/`wpctl set-mute`, default output/input device via `wpctl set-default <id>` (devices enumerated from `pw-dump` JSON, falling back to parsing `wpctl status`), and the laptop-display toggle, which writes/removes `/tmp/hypr-laptop-display-forced` and invokes the same code path as `scripts/hypr-monitor-hotplug` — it never writes a `monitor=…,disable` record, which the hotplug watcher would fight. Reset drops `staged`.

On **Apply**, `core/apply.rs` executes a fixed-order plan:

1. **Validate** all staged values (abort before any side effect on failure).
2. **Conflict check** (re-read + hash compare, §3).
3. **Write files** atomically (§3). If the palette scheme changed: edit nothing generated — run the discovered repo's `scripts/generate-colors <scheme>` (located per §5/R8.5) as a child process and treat non-zero exit as a write failure (the script validates and fails loudly without partial output). The generate-colors step runs **last** among the write steps: per-file rollback only restores app-written snapshots, so ordering it last guarantees a failure elsewhere never leaves the generated files (the six color partials, three font partials, and the marker — §3) on the new scheme while everything else is rolled back. Note `generate-colors` now also depends on `theme/fonts` (analysis §6.5) and aborts if it is missing/incomplete, so a broken font source surfaces here as a palette write failure — already covered by the non-zero-exit handling.
4. **Reload** affected components — only those whose backing file actually changed *and* which detection found running (this set mirrors `scripts/apply-theme`, the canonical CLI wrapper — analysis §6.1 — and must be kept in sync):

| Component | Mechanism |
|---|---|
| Hyprland | `hyprctl reload` (child process; hyprctl speaks the IPC socket for us). Simple live tweaks may additionally use `hyprctl keyword ...` for flicker-free effect |
| eww | `eww reload` |
| swaync | `swaync-client -rs` |
| kitty | `SIGUSR1` to all kitty PIDs (sent directly via `nix::kill`, no shell). Remote control is now enabled in the dotfiles (`allow_remote_control socket-only` + per-instance socket, analysis §6.1), so a later switch to flicker-free `kitten @ set-colors` is unblocked; v1 keeps SIGUSR1 |
| hyprpaper | `hyprctl hyprpaper preload/wallpaper ...` |
| hypridle | restart: `systemctl --user try-restart hypridle` if unit exists, else kill + respawn detached |
| hyprlock | **none (intentional)** — hyprlock reads its config at launch, so changes take effect at the next lock |
| GTK theme/icons/cursor | `gsettings set ...` + `hyprctl setcursor <theme> <size>`; the cursor value is written identically to every place the app owns — both `settings.ini` (via the INI writer), the `hyprland.conf` env line, and `uwsm/env` in step 3 — so the (now-unified `Nordic-cursors`/16) duplicates stay equal (R3.4, analysis §6.2) |

All reloads go through the `CommandRunner` trait: spawned without a shell (`Command` with arg vectors — no injection surface), 5 s timeout, exit status + stderr captured. A failed reload is logged at `error` and shown as a non-fatal toast; file writes stand (R5.5). Every write and reload is logged per R7.3.

## 7. UI architecture

- Plain `GtkApplicationWindow` with sidebar `GtkStackSidebar` (categories from §3 of requirements) + content `GtkStack`; scales for the 1.33-scaled laptop panel and 1440p monitors (R2.4). **libadwaita is deliberately not used**: it hard-codes the Adwaita stylesheet and ignores `gtk-theme-name`, which would violate R2.1/R2.2.
- Each category page is a Relm4 component that renders rows from a declarative row list (`label`, `widget kind`, `setting id`, `required capability`). Widget kinds per R2.3: `GtkDropDown` (choices), `GtkSwitch` (booleans), `GtkScale` (continuous).
- Widgets are thin: they emit `SetValue(setting_id, value)` messages to the store and re-render from store state. Dirty state drives a suggested-action **Apply** button + per-page dot markers, with **Reset** alongside (R5.1). All staging/dirty/conflict logic is in `core/` and headlessly tested (R6.2).
- Sound page reads PipeWire state on page entry from `pw-dump` JSON (fallback: parsing `wpctl status`); device switching via `wpctl set-default <id>` (v1; event subscription via `pw-mon` deferred). Network page is read-only status (`nmcli -t`) + an "Open Network Settings" button that spawns `kitty -e nmtui` (gated on the kitty capability) or `nm-connection-editor` when installed (R3.1) — re-launching the autostarted `nm-applet` or a terminal-less `nmtui` is not viable.

## 8. Startup sequence (budget: <500 ms, R8.1)

1. Parse CLI, init tracing (journald or stderr fallback).
2. `gtk::Application::register` — if another instance holds the ID, activate it and exit.
3. Run detection + parse all backing files concurrently on a worker thread while GTK builds the window shell.
4. Populate pages when detection/parsing completes; log detection summary at `info`.

## 9. Testing strategy (R6)

- **Unit** (`src/**` `#[cfg(test)]`): every parser — round-trip identity, targeted-edit exactness (comments/order/commented-out lines untouched), malformed-input tolerance; store staging/dirty/reset/conflict transitions; validators.
- **Integration** (`tests/`): fixture copy of the real dotfiles tree in a `tempfile` dir; scenarios stage edits → Apply → assert exact resulting file bytes; `CommandRunner` mock asserts the exact reload commands (and their order) for each change class, including failure injection for R5.4/R5.5 paths.
- **Detection**: fake `PATH` dirs with dummy executables; assert `Capabilities` and hidden-row outcomes.
- CI/pre-commit: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`.

## 10. Key risks / decisions

- **hyprlang writer fidelity** is the highest-risk parser (nested sections, `source=`, comment preservation); mitigated by exhaustive round-trip fixtures taken from the live dotfiles. The fallback proposed in analysis §5.3 (small `source=`d files the app fully owns) is now **partly realized** — the `input { }` block is already extracted into `input.conf` (analysis §6.3), so the remaining exposure is `hyprland.conf`'s cursor `env=` lines plus `hypridle`/`hyprlock`.
- **Duplicated values**: cursor env (`hyprland.conf` + `uwsm/env`, now both `Nordic-cursors`/16) and wallpaper vs hyprlock background (now the same path) are handled **write-both-identically**. The monitors-vs-profile-script duplication is **gone** — `hypr-display-profile.sh` now derives eDP mode/scale from `monitors.conf` (analysis §6.2), so the app edits one record (kept awk-parseable) and issues no sync warning.
- **Live theme propagation** (R2.2) depends on `xdg-desktop-portal-gtk` (or the dconf GSettings backend) being present — not guaranteed on Hyprland. Detection checks for it; acceptance is "restyles live when the portal/dconf path is active, on next launch otherwise", and the UI communicates which applies.
- No root, user files only (R8.2); no shell interpolation anywhere in command execution.
