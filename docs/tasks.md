# Settings4000 — Implementation Tasks

Atomic, non-overlapping breakdown of `docs/architecture.md`. Requirement references (R…) point to `docs/requirements.md`.
Complexity: 1 = simple/repetitive … 5 = highly complex.

## 1. Project foundation

- [x] **1.1 Cargo project scaffold** — Complexity: 1
  Init binary crate (2024 edition) with `gtk4`, `relm4`, `tracing`, `tracing-subscriber`, `tracing-journald`, `serde_json` (preserve_order), `tempfile` (dev), `nix` deps; module tree `ui/ core/ parsers/ system/` as in architecture §2. *Accept:* `cargo build` and `cargo test` pass on empty modules; a module-boundary check (grep-based test for `gtk` imports, or workspace crate split) fails if `core/` or `parsers/` import `gtk`.

- [ ] **1.2 CLI & logging init** — Complexity: 2
  `--log-level` flag; `EnvFilter` from `SETTINGS4000_LOG`/`RUST_LOG` (flag wins); journald layer with stderr fmt fallback when journald is unavailable (R7.1–R7.2). *Accept:* messages visible in `journalctl --user -t settings4000`; fallback exercised by unit test with journald socket absent.

- [ ] **1.3 GtkApplication bootstrap + single instance** — Complexity: 2
  Fixed app ID, `Application::register`; second launch activates the existing window and exits (R8.4). Empty `ApplicationWindow` shown. *Accept:* relaunching focuses the running window; process exits 0.

- [ ] **1.4 CI / pre-commit gate** — Complexity: 1
  Wire `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` into CI and/or pre-commit (R6.1). *Accept:* pipeline fails on any of the three.

## 2. System boundary

- [ ] **2.1 `CommandRunner` trait + real impl** — Complexity: 2
  Shell-free `Command` spawning (arg vectors), 5 s timeout, captured exit status/stderr; every invocation logged (cmd + exit, R7.3). Mock recorder impl for tests. *Accept:* unit tests via mock; real impl runs `true`/`false` correctly with timeout kill verified.

- [ ] **2.2 Atomic file writer** — Complexity: 3
  Files are addressed by their XDG runtime path (`~/.config/…`), never a hardcoded `~/.dotfiles` path (R8.5). `fs::canonicalize` the target (a symlink into a dotfiles repo resolves to the repo file; a plain file resolves to itself) → `NamedTempFile` in the resolved target's dir → fsync → rename over the target (R5.4). Symlink preserved when present; plain files rewritten in place. Pre-write snapshot capture for rollback; write logged with resolved path (byte-level — changed-key logging lives in the apply orchestrator, R7.3). *Accept:* tests prove symlink target rewritten + link preserved, a plain (non-symlink) file rewritten in place, no partial file on injected failure, snapshot restore works.

- [ ] **2.3 File freshness tracking** — Complexity: 2
  Per-file content-hash + mtime record on read; `check_conflicts()` re-reads and reports changed files (R5.6). *Accept:* external modification between read and check is detected; unchanged files aren't flagged.

## 3. Parsers (each: lossless representation, targeted value edits, round-trip tests — R5.3 item 1, R6.1)

- [ ] **3.1 Palette kv parser (`colors/<scheme>`)** — Complexity: 2
  Line-oriented `key=barehex`; in-place value edit; validation against the fixed 17-key schema. Write path exists only for R6.1 round-trip coverage / future per-key editing — no v1 UI edits `colors/<scheme>` (R3.2 switching runs `generate-colors` only; per-key palette editing is out of scope for v1). *Accept:* round-trip identity; edit changes exactly one value span; malformed lines surfaced as parse warnings, not panics.

- [ ] **3.2 hyprlang parser/writer** — Complexity: 5
  Tokenize comments, `key = value`, nested `section { }`, `source=`. Edits address keys by section path (`input.touchpad.natural_scroll`); repeatable top-level keys (`env=`, `exec-once=`) addressed by key + first comma-field (e.g. `env:XCURSOR_THEME`) with only the value portion edited; commented-out lines never matched; new keys appended at section end. Covers `config/hypr/input.conf` (the extracted `input { }` block), `hyprland.conf` (cursor `env=`), `hypridle.conf`, `hyprlock.conf`, `hyprpaper.conf`. *Accept:* round-trip identity on live-dotfile fixtures; targeted edits leave all other bytes untouched; nested + duplicate-section fixtures pass; the `input.*` fixture edits `input.conf` (e.g. `input.touchpad.natural_scroll`), not `hyprland.conf`; repeatable-key fixture (multiple `env=` lines, edit `env:XCURSOR_THEME` only) passes.

- [ ] **3.3 monitors.conf record parser** — Complexity: 3
  Parse `monitor=` records (name/desc, mode, position, scale, enable — the latter handled as `monitor=NAME,disable` records) preserving surrounding lines and later-rule-wins order; edit matching record or append after last `monitor=` line. *Accept:* round-trip identity; editing the eDP rule doesn't disturb the catchall or comments.

- [ ] **3.4 swaync JSON adapter** — Complexity: 1
  `serde_json` preserve_order, parse–modify–reserialize (2-space pretty). *Accept:* key order stable across a no-op edit; position/timeout edits round-trip.

- [ ] **3.5 INI editor (GTK settings.ini)** — Complexity: 2
  Line-preserving kv-in-section edits. Both `settings.ini` files are now tracked in the dotfiles and symlinked (analysis §6.5), so editing an existing `[Settings]` section is the common path; creating file + `[Settings]` section when absent remains a fallback. *Accept:* comments/order preserved; both the edit-existing and file-creation paths tested.

- [ ] **3.6 env-file editor (`uwsm/env`)** — Complexity: 2
  Targeted `export KEY=value` edits for `XCURSOR_THEME`/`XCURSOR_SIZE`; detects (but never edits) commented lines — feeds the `GTK_THEME` override warning (R3.3). *Accept:* round-trip identity; commented `GTK_THEME` reported, uncommented lines editable.

- [ ] **3.7 Generated-file readers** — Complexity: 1
  Read-only: active-scheme detection from the `# Generated from colors/<scheme>` header of the deployed `~/.config/hypr/colors.conf` (R3.2); optional per-scheme swatch parse (reads the discovered repo's `colors/<scheme>` files, R8.5) for the drop-down. The repo also carries a `state/active-scheme` marker (analysis §6.4) as an optional future detection shortcut; v1 reads the header. *Accept:* scheme name extracted from fixture; missing/odd header degrades to "unknown" without error.

## 4. Core domain (GTK-free — R6.2)

- [ ] **4.1 Typed settings model + validators** — Complexity: 3
  `SettingId`, typed values (enum/bool/float/string), per-setting validation: hex colors, `WxH@Hz` mode strings, timeout/sensitivity/scale ranges, file paths (exists + readable + image extension for wallpaper/lock background) (R8.3). *Accept:* unit tests per validator, valid + invalid cases.

- [ ] **4.2 SettingsStore (staging state machine)** — Complexity: 3
  `original`/`staged` per setting; dirty computation, per-page dirty rollup, `stage()`, `reset()`, conflict-triggered reload of originals (R5.1, R5.6). Runtime-only settings bypass staging (R5.2). *Accept:* headless tests cover stage→dirty→reset→clean, conflict reload, and bypass paths.

- [ ] **4.3 Capabilities detection** — Complexity: 3
  Manual `$PATH` scan (injected PATH string) — including `hyprctl` (needed by nearly every reload), `gsettings`, `pactl`/`wpctl`, and the daemon binaries (`kitty`/`eww`/`swaync`/`hyprpaper`/`hypridle`/`hyprlock`) — daemon liveness (Hyprland IPC socket path, procfs pidof-equivalent), `nmcli` presence, settings-portal availability (`xdg-desktop-portal-gtk`/dconf — gates the R2.2 live-restyle claim), the dotfiles palette source (repo root canonicalized from a deployed config symlink, then `colors/` + `scripts/generate-colors` present — gates the palette switcher, R3.2/R8.5), config readable/parseable checks at XDG paths (R4.1, R4.4). Produces plain `Capabilities` struct; errors degrade to absent; run once at startup + manual refresh (R4.3). *Accept:* fake-PATH tests (R6.1); non-symlinked config (no repo behind it) hides the palette switcher; hidden items logged at `info`, unreadable configs at `warn`.

- [ ] **4.4 Reload command table** — Complexity: 2
  Map backing-file → reload action: `hyprctl reload`, `eww reload`, `swaync-client -rs`, SIGUSR1 to kitty PIDs via `nix::kill`, `hyprctl hyprpaper …`, hypridle restart (`systemctl --user try-restart` else kill+respawn), `gsettings set …` + `hyprctl setcursor` (architecture §6 table). This set mirrors `scripts/apply-theme` (the canonical CLI wrapper, analysis §6.1) and must stay in sync; kitty uses SIGUSR1 in v1 though remote control is now enabled for a later flicker-free `kitten @ set-colors` (analysis §6.1). *Accept:* unit tests: each changed file maps to exactly the expected actions, gated on capability.

- [ ] **4.5 Apply pipeline orchestrator** — Complexity: 4
  Fixed order: validate all → conflict check → atomic writes with per-file rollback → reloads for changed+running components only, via the 4.4 table (architecture §6). Palette change = run the discovered `scripts/generate-colors <scheme>` as the **last** write step (non-zero exit = write failure — which now also covers a missing/incomplete `theme/fonts`, since generate-colors depends on it, analysis §6.5), so rollback of earlier files never leaves the generated files (six color + three font partials + marker) on the new scheme. Each write logged with path + changed keys (R7.3). Reload failures non-fatal, logged `error`, surfaced (R5.3–R5.5). *Accept:* integration tests with mock runner assert exact command sequence per change class and failure-injection behavior, including that generate-colors runs after all file writes.

## 5. UI shell

- [ ] **5.1 Main window: sidebar + stack** — Complexity: 2
  `GtkStackSidebar` + `GtkStack` with the seven categories; usable at 1.33-scaled 2880×1800 and 1440p (R2.4). Categories with zero visible rows hidden (R4.2). No libadwaita — it hard-codes Adwaita styling and ignores `gtk-theme-name` (R2.1/R2.2). *Accept:* window renders all detected categories; no custom CSS anywhere (R2.1).

- [ ] **5.2 Declarative row framework** — Complexity: 3
  Row descriptor (`label`, widget kind, `SettingId`, required capability) → widget construction: `GtkDropDown`/`GtkSwitch`/`GtkScale`, plus a `GtkListBox`-backed reorderable editable list (add/remove/reorder rows) for ordered multi-value settings (R2.3). Widgets emit `SetValue` messages and render from store state only. *Accept:* a page defined purely as descriptor list renders and round-trips values through the store.

- [ ] **5.3 Apply/Reset chrome + dirty indicators** — Complexity: 2
  Suggested-action Apply button, per-page dot markers, Reset, non-fatal error toasts for reload failures, conflict warning dialog on focus/refresh (R5.1, R5.5, R5.6); manual detection-refresh action that re-runs `core/detect` and repopulates pages (R4.3). *Accept:* dirty markers track store state; Reset clears; toast shown on injected reload failure.

- [ ] **5.4 Startup sequencing** — Complexity: 3
  Detection + all file parsing on worker thread concurrent with window construction; pages populated on completion; detection summary logged at `info` (architecture §8). *Accept:* pages populate on worker-thread completion; missing optional apps never block startup (R4.3). (The < 500 ms budget, R8.1, is verified by task 7.3.)

## 6. Category pages (each: descriptor list + page-specific glue; depends on §3–§5)

- [ ] **6.1 Display page** — Complexity: 4
  Per-monitor resolution/refresh/scale/position drop-downs + per-monitor enable switch (R2.3) from `monitors.conf` records + current `hyprctl monitors -j` state; laptop-display toggle drives the existing hotplug mechanism — writes/removes `/tmp/hypr-laptop-display-forced` and triggers the same path as `scripts/hypr-monitor-hotplug`, applied immediately (R5.2), never a `monitor=…,disable` record. `monitors.conf` is the single source for eDP mode/scale — `scripts/hypr-display-profile.sh` derives its values by parsing it (analysis §6.2), so a `monitor=` record edit must stay awk-parseable (leading `key,` token, mode field 2, scale field 4, extras after) and the app never writes the script. *Accept:* staged edit → Apply rewrites only the target `monitor=` record and runs `hyprctl reload`; the rewritten record is still parseable by `hypr-display-profile.sh`'s awk; toggle creates/removes the state file and never touches `monitors.conf`; page hidden and logged at `info` when `hyprctl` is absent (R4.2/R4.4).

- [ ] **6.2 Sound page (runtime-only)** — Complexity: 3
  Output/input device drop-downs (switching via `wpctl set-default <id>`), volume sliders, mute switches driving `wpctl` immediately (R3.1, R5.2); device/state enumeration on page entry from `pw-dump` JSON, falling back to parsing `wpctl status`. *Accept:* controls bypass staging; device switch issues `wpctl set-default` with the right id; commands verified via mock runner; page hidden when `wpctl`/`pactl` absent.

- [ ] **6.3 Theme page — palette scheme** — Complexity: 2
  Drop-down over the palette files in the discovered repo's `colors/` dir (repo root resolved from a deployed config symlink, not a hardcoded `~/.dotfiles` path — R8.5; enumeration skips dotfiles, subdirectories, and non-palette files so a state marker never appears as a scheme) with active-scheme detection (task 3.7); degrades to read-only display when <2 schemes (R3.2); Apply runs the discovered `generate-colors` + reload chain via the pipeline. *Accept:* fixture with one scheme shows read-only; switch triggers exact expected command sequence; controls hidden and logged at `info` when the palette source (`generate-colors`/the colors dir, or the repo behind the config) is absent (R4.2/R4.4).

- [ ] **6.4 Theme page — GTK/icon/cursor themes** — Complexity: 4
  Discovery scans (`~/.themes`, `~/.local/share/themes`, `/usr/share/themes` for gtk dirs; icon dirs with `index.theme`, cursor = has `cursors/`) (R3.3–R3.4). Apply = `gsettings` + both `settings.ini` + `hyprctl setcursor` + identical `XCURSOR_*` writes to `hyprland.conf` env and `uwsm/env`. `GTK_THEME` override (set in the app's own environment or uncommented in `uwsm/env`) shows a banner and disables the GTK drop-down (R3.3). *Accept:* discovery unit-tested against fixture trees; cursor apply writes both files to the same value; banner path tested; changing the GTK theme from the app restyles the app live when the portal/dconf path is active (R2.2) — verified manually on the target setup, with the "takes effect at next launch" fallback shown otherwise; GTK/icon/cursor controls hidden and logged at `info` when `gsettings` is absent (R4.2).

- [ ] **6.5 Theme page — wallpaper & lock background** — Complexity: 2
  Path choosers writing `hyprpaper.conf` and `hyprlock.conf` via the hyprlang editor, plus a `fit_mode` drop-down (same hyprlang edit); hyprpaper reload via `hyprctl hyprpaper …`. The two paths now default to the **same** image (analysis §6.2), so the UI presents a single wallpaper with an optional lock-screen override, still writing both files. hyprlock intentionally gets no reload — it reads its config at launch, so changes apply at the next lock. *Accept:* staged path/fit-mode edit produces exact expected file diff + preload/wallpaper commands; no reload command issued for hyprlock-only changes; wallpaper/lock controls hidden and logged at `info` when hyprpaper/hyprlock are absent (R4.2/R4.4).

- [ ] **6.6 Input page** — Complexity: 3
  Keyboard layouts as an ordered editable list (add/remove/reorder → `kb_layout=us,se`), entries sourced from the XKB registry (`/usr/share/xkb/rules/evdev.xml`); keyboard options as curated switches for the options in use (`caps:escape`, `grp:win_space_toggle`), preserving unknown `kb_options` entries verbatim; mouse sensitivity slider, touchpad switches (natural scroll, tap-to-click) editing the `input { }` section path in `config/hypr/input.conf` (the `source=`d file, **not** `hyprland.conf` — analysis §6.3) via the hyprlang writer. *Accept:* each control maps to the right section path; layout reorder round-trips; unknown `kb_options` survive an edit untouched; Apply diff limited to the touched lines in `input.conf`; `hyprctl reload` triggered; page hidden and logged at `info` when `hyprctl` is absent (R4.2/R4.4).

- [ ] **6.7 Notifications page** — Complexity: 2
  Position drop-down, timeout sliders, DND switch over `swaync/config.json`; reload via `swaync-client -rs`. *Accept:* JSON edits round-trip with stable key order; page hidden when swaync absent.

- [ ] **6.8 Power & Idle page** — Complexity: 3
  Dim/lock/dpms timeout sliders + lock command entry over `hypridle.conf` listener/general blocks (hyprlang writer, positional listener matching); hypridle restart on Apply. *Accept:* editing one listener's timeout leaves the other listener blocks byte-identical; restart command issued only when the file changed; page hidden and logged at `info` when hypridle is absent (R4.2/R4.4).

- [ ] **6.9 Network page (read-only)** — Complexity: 2
  Connection status from `nmcli -t` + "Open Network Settings" button spawning `kitty -e nmtui` (gated on the kitty capability) or `nm-connection-editor` when installed (R3.1). *Accept:* status renders; button spawns the expected command per available capability (mock runner); page hidden without NetworkManager.

## 7. Test infrastructure & suites (beyond per-task unit tests)

- [ ] **7.1 Fixture dotfiles tree** — Complexity: 2
  Anonymized copy of the real dotfiles layout under `tests/fixtures/` — including the post-prep files (`config/hypr/input.conf`, `config/gtk-{3,4}.0/settings.ini`, `theme/fonts`, the generated font partials, `state/active-scheme`; analysis §6), installable into a `tempfile` dir with symlinks mirroring deployment (R6.1). *Accept:* helper produces a working tree per test; used by ≥ all integration suites.

- [ ] **7.2 End-to-end staged-edit → Apply suites** — Complexity: 3
  Per category: stage edits against the fixture tree, Apply with mock runner, assert exact resulting file bytes + exact reload command list, including rollback-on-failure and reload-failure scenarios (R5.4–R5.5). Consolidates the per-page Apply assertions from §6 into the shared fixture harness — page tasks may reference this suite rather than re-specify byte-exact tests. *Accept:* one suite per file-backed category; failure-injection cases green.

- [ ] **7.3 Startup-time benchmark check** — Complexity: 2
  Reproducible measurement of cold start to first frame (e.g. `GTK_DEBUG=interactive`-free timer + logged mark), tracked against the 500 ms budget (R8.1). *Accept:* number produced in CI or a documented manual procedure; current value recorded.

## 8. Polish & release

- [ ] **8.1 Structured logging audit** — Complexity: 1
  Verify R7.3 coverage: detection results, every write (path + keys, no full contents at `info`), every reload + exit status, all errors; `debug` includes parsed values and staged diffs. Also confirm no operation requires root or writes outside `~/.dotfiles`/`~/.config` (R8.2). *Accept:* checklist walkthrough against `journalctl` output for a full Apply cycle.

- [ ] **8.2 Desktop integration** — Complexity: 1
  `.desktop` file + icon, install target (`~/.local/share/applications` or package). *Accept:* launchable from rofi; single-instance activation works from the launcher.

- [ ] **8.3 README + install/build docs** — Complexity: 1
  Build/run instructions, dependency list, relation to the dotfiles prep tasks (`~/.dotfiles/settings_app_prep_tasks.md`). *Accept:* clean-machine build succeeds following the doc.
