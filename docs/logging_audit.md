# Structured logging audit (task 8.1)

An audit of the app's `tracing` coverage against **R7.3** (what must be logged,
at which level) and a confirmation of **R8.2** (no root, writes only under the
user's config locations), concluded with the required checklist walkthrough
against real `journalctl --user -t settings4000` output for a full Apply cycle.

## How the walkthrough was produced

The Apply cycle must never run against the real `~/.config` / `~/.dotfiles`
(nothing in a logging audit justifies mutating a working desktop), so the
cycle is driven headlessly against the anonymized fixture dotfiles tree from
task 7.1, with the app's **real** journald subscriber installed:

```
cargo run --example logging_walkthrough
journalctl --user -t settings4000 --since -2min
```

`examples/logging_walkthrough.rs` initializes logging exactly as
`settings4000 --log-level debug` would (journald layer, `info,settings4000=debug`
directive), installs a fresh `FixtureDotfiles` tree in a `TempDir`, then runs:

1. **Real detection** (`Capabilities::detect`) — a read-only `$PATH` scan of
   the host, palette-source discovery through the fixture's deployed symlink,
   and readability checks on the three loaded configs.
2. **Real store loads** through the app's own startup loaders
   (`input.conf`, `swaync/config.json`, `hypridle.conf`).
3. **A happy-path Apply**: two staged Input edits → the real pipeline
   (`apply::run`) → a real atomic write through the deployed symlink → the
   planned `hyprctl reload` → commit.
4. **A rollback Apply**: a staged edit plus a palette switch whose
   `generate-colors` is the fixture's stub — a **real subprocess** run through
   the **real** `SystemCommandRunner` that exits 2 — so the write-failure,
   per-file rollback, and command-exit logging all fire genuinely.

What is mocked, and why that is honest:

- The **reload subprocesses** in the happy path run through a
  `MockCommandRunner`, because a real `hyprctl reload` would poke the live
  compositor. The reload-*level* log (`reload command succeeded` /
  `reload command reported failure`, `core/reload.rs`) sits **above** the
  runner seam and therefore still runs for real. The runner-*level*
  invocation + exit-status log (`ran command`, `system/command.rs`) is instead
  demonstrated by the rollback cycle's real `generate-colors` run.
- The GUI is not driven (option (i) from the task): Apply cannot be clicked
  programmatically, and the plan assembly used here (`base_apply_plan` +
  the Input model's write glue) is the same code the window's Apply handler
  runs, shared via the task-7.2 harness. What this walkthrough consequently
  does **not** prove is the `ui/window.rs` chrome-level lines (the startup
  summary, the "apply committed" line) — those were verified by inspection
  and are exercised read-only by every GUI launch.

Note on levels: journald stores tracing levels as syslog priorities
(`info` → NOTICE, `debug` → INFO, …). The excerpts below are re-labelled with
the original tracing level; the raw priority mapping is `tracing-journald`'s
documented behavior, and `journalctl -o json` shows the structured fields.

## R7.3 checklist against the captured journal

Captured 2026-07-21 from a `cargo run --example logging_walkthrough` run
(fixture temp root `/tmp/.tmpNRbnB0`, trimmed only by eliding repeated
`found on PATH` lines).

### (a) Startup detection results — covered

Per-item results at `debug` (present binaries, socket liveness, palette
discovery, readable configs), user-visible absences at `info`, unreadable
configs at `warn` (`core/detect.rs`), and a one-line summary at `info`:

```
DEBUG core::detect: found on PATH                                    [binary=hyprctl]
INFO  core::detect: not found on PATH; dependent settings will be hidden (R4.2)  [binary=swaync]
DEBUG core::detect: Hyprland IPC socket liveness                     [hyprland_ipc=false]
DEBUG core::detect: palette source discovered                        [repo_root=/tmp/.tmpNRbnB0/home/.dotfiles]
DEBUG core::detect: config readable                                  [path=…/.config/hypr/input.conf]
INFO  core::detect: capabilities detection complete                  [binaries=12 daemons=0 hyprland_ipc=false palette_source=true settings_portal=true unreadable_configs=0]
```

### (b) Every file write: path + keys at `info`, never full contents — covered

Two layered `info` records per write: the writer logs the **resolved** path +
byte count (`system/writer.rs`), the Apply orchestrator logs the **live** path
+ changed-key labels (`core/apply.rs`). Rollback restores are logged the same
way. No call site logs file contents at `info` anywhere in `src/` (verified by
reading every `tracing::info!`/`warn!`/`error!` call site; the only
output-adjacent log is the command stderr excerpt, which is truncated to
512 bytes and emitted at `debug` only — `system/command.rs`).

```
INFO  system::writer: wrote configuration file atomically   [bytes=419 path=/tmp/.tmpNRbnB0/home/.dotfiles/config/hypr/input.conf]
INFO  core::apply:    wrote backing file                    [keys=["input.kb_layout", "input.touchpad.natural_scroll"] path=…/.config/hypr/input.conf]
INFO  system::writer: restored file to its pre-write contents  [bytes=419 path=…/.dotfiles/config/hypr/input.conf]
```

The full set of production write sites, each logged at `info`:
`write_atomic` (all Apply writes, above), `FileSnapshot::restore` (rollback,
above), and the laptop-display hotplug override file (`core/display.rs`,
"enabled/disabled the laptop display and set/cleared the hotplug override").
The palette generator — the one write step that is a subprocess — logs
`regenerated palette via generate-colors` with the scheme at `info` on
success and a write-failure at `error` otherwise (see (d)).

### (c) Every reload command + exit status — covered

Reload-level record in `core/reload.rs` (`run_and_check`, plus the dedicated
kitty-SIGUSR1 and hypridle-restart paths), and the runner-level invocation +
exit status for **every** spawned command in `system/command.rs`:

```
INFO  core::reload:    reload command succeeded   [command=hyprctl reload]
INFO  system::command: ran command                [args=["nord"] exit_code=Some(2) program=…/.dotfiles/scripts/generate-colors success=false]
DEBUG system::command: command stderr             [program=…/generate-colors stderr=settings4000 fixture: generate-colors stub invoked for real truncated=false]
```

(The `ran command` line above comes from the rollback cycle's real
`SystemCommandRunner`; the happy path's `hyprctl reload` went through the
mock runner, which by design records instead of spawning — on the real app
every reload also produces its own `ran command` line with `exit_code`.)
Failure sides are covered by `reload command reported failure (R5.5)` /
`reload command could not be run (R5.5)` at `error`, `sent SIGUSR1 to kitty`
/ `no running kitty found` for the signal reload, and the systemctl-vs-
respawn branches of the hypridle restart — all verified at their call sites
and pinned by `core::reload`'s unit tests.

### (d) All errors — covered

Every abort/failure path logs at `warn`/`error` before returning:

```
ERROR core::apply: apply write phase failed; rolling back (R5.4)  [cause=generate-colors exited with status 2]
```

Verified by inspection across the tree: validation aborts and conflicts
(`core/apply.rs`, `warn`), write failures + rollback failures (`error`),
non-fatal reload failures (`error`, R5.5), runtime-control command failures
(sound, DND, laptop toggle, network launcher — `error`), parse warnings from
every parser (`warn`), unreadable/unloadable configs (`warn`, R4.4), spawn
failures and timeouts (`system/command.rs`, `warn`), conflict-reload failures
(`core/store.rs`, `error`), and the GApplication registration failure
(`ui/app.rs`, `error`).

### (e) `debug` includes parsed values and staged diffs — covered after fixes

```
DEBUG core::store: parsed values offered to the store  [path=…/.config/hypr/input.conf values=[(KeyboardLayouts, String("us,se")), (KeyboardOptions, String("grp:win_space_toggle,caps:escape")), (MouseSensitivity, Float(0.3)), (TouchpadNaturalScroll, Bool(true)), (TouchpadTapToClick, Bool(true))]]
DEBUG core::store: staged edit                             [dirty=true id=KeyboardLayouts original=String("us,se") staged=Some(String("se,us"))]
DEBUG parsers::hyprlang: rewrote hyprlang value            [path=input.kb_layout]
DEBUG core::store: committed applied edits: promoted staged values and re-baselined written files  [files=1]
```

The bespoke (non-store) staging sources are covered too: the palette model
logs its staged scheme switch at `debug` (added in this task — a palette
switch writes no file, so no parser edit log would otherwise ever name the
pending scheme), and the Display/Theme models' staged edits surface at
`debug` through the parsers' per-edit rewrite logs when the Apply renders
their writes, plus the changed-key labels at `info`.

## Gaps found and fixed in this task

1. **Staged diffs were not actually logged.** `SettingsStore::stage` logged
   only the setting id and dirty flag. It now logs the original and staged
   values at `debug` (`src/core/store.rs`).
2. **Parsed values were only counted, never logged.** The startup loader
   logged `settings = <n>`. The parsed `(SettingId, Value)` pairs are now
   logged at `debug` in `SettingsStore::ingest` (`src/core/store.rs`) — the
   one choke point shared by the initial load and the conflict reload, so
   both paths log identically.
3. **A staged palette switch was invisible below `info`.**
   `PaletteModel::stage` now logs the active → staged scheme at `debug`
   (`src/core/theme.rs`).

No violations of the negative rules were found: no call site logs full file
contents at `info` (the closest thing, the command stderr excerpt, is
truncated and `debug`-only), and no reload path omits its exit status.

## R8.2 confirmation: no root, writes only under the user's config

Verified by inspection, with the evidence:

- **No privilege escalation anywhere.** `grep -rn "sudo\|pkexec\|setuid\|geteuid\|doas" src/`
  matches nothing. Every spawned program is a user-session tool (`hyprctl`,
  `wpctl`, `swaync-client`, `gsettings`, `eww`, `nmcli`, `setsid`,
  `generate-colors`), and every `systemctl` invocation passes `--user`
  (`core/reload.rs`). Commands are arg-vectors with no shell (enforced by
  `system::command` and its shell-metacharacter test).
- **One production write primitive.** All Apply writes go through
  `system::writer::write_atomic`, which canonicalizes the **live**
  `$XDG_CONFIG_HOME`/`~/.config` path the caller addressed (R8.5) and stages
  the temp file **in the resolved target's own directory** — so both the
  target and the temp file are confined to the user's config location or the
  dotfiles repo file it symlinks to. The write-through-symlink behavior is
  pinned by `system::writer`'s unit tests and every task-7.2 suite
  (`assert_repo_untouched_except` proves nothing outside the planned files
  changes).
- **Write targets are only ever XDG paths or the discovered repo.** Callers
  build paths from the XDG config home (`ui/startup.rs`) or from the palette
  source discovered by canonicalizing a deployed `~/.config` symlink
  (`core/detect.rs`, R8.5) — never a hardcoded absolute path elsewhere.
  The palette generator writes the repo's generated partials, running as the
  user from the repo's own `scripts/`.
- **One deliberate, documented exception:** the laptop-display hotplug
  override `/tmp/hypr-laptop-display-forced` (`core/display.rs`). This is not
  configuration but the runtime flag file of the dotfiles' existing hotplug
  mechanism (CLAUDE.md domain gotcha: the app must use that mechanism rather
  than writing `monitors.conf` disable records). It is created/removed with
  ordinary user privileges and logged at `info`.
