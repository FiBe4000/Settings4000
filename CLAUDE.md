# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Current state

**All planned implementation (`docs/tasks.md` §1–§8) is complete and pushed**: the crate builds and the three CI gates run green. The open backlog lives in `docs/tasks.md` as unchecked tasks — §9 (post-review fixes from the 2026-07-21 full-project review), §10 (additional features) and **§11 (dotfiles re-alignment, 2026-10-20)**. `docs/tasks.md` additionally carries the historical record of each landed task, so an older task's text may describe behaviour a later task changed; such places name the superseding task.

**The target dotfiles moved under the app (2026-10-20 re-analysis, `docs/dotfiles_analysis.md` §7).** On branch `wayland-dev` Hyprland now runs its **Lua** config provider (`hyprland.lua` + app-owned `input.lua`; `hyprland.conf`/`input.conf` are frozen fallbacks the compositor ignores), **swaync was replaced by `notifd`** (a config-less Python daemon with eww widgets), the palette schema is 18 keys with a generated `colors.lua`, the cursor is `breeze_cursors`/24, and `hyprctl keyword` is rejected on Lua sessions. The specs (`docs/requirements.md`, `docs/architecture.md`) have been updated to the §7 state and therefore **run ahead of the code**: `src/` still implements the §6 layout, so until §11 lands the Input page, the cursor env write, the Notifications page and the laptop-display disable path write files or run commands the live session ignores. The app is **not currently installed** on the development machine (a different laptop than before). Work §11 in its stated order (fixtures → provider detection and the Lua-literal parser → retargets) before touching §9/§10 items that overlap it (6.6, 6.7, 9.9, 9.18, 9.21, 9.26 are superseded in part or whole — see the §11 preamble).

The authoritative specs, in order of use:
- `docs/requirements.md` — what to build (numbered **R…** requirements, referenced everywhere).
- `docs/architecture.md` — how to build it (module layout, parser strategies, apply pipeline).
- `docs/tasks.md` — atomic, ordered implementation breakdown with acceptance criteria.
- `docs/dotfiles_analysis.md` — the concrete `~/.dotfiles` layout this app reads/writes.

When implementing, treat the R-numbers as the contract and update `docs/tasks.md` checkboxes as tasks land.

## What this is

Settings4000 is a native GTK4 GUI (Rust + Relm4) that edits the config files of a `~/.dotfiles`-managed Hyprland desktop and triggers the right live-reloads — replacing manual config editing for common user-facing settings (display, sound, theme, input, notifications, power/idle, network).

## Commands

```
cargo build
cargo test                       # built-in harness; unit + integration
cargo test <name>                # single test by substring
cargo test --test <file>         # one integration test file in tests/
cargo fmt --check                # CI gate
cargo clippy -- -D warnings      # CI gate (warnings are errors)
```

The pre-commit / CI gate is exactly these three: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`. Logs go to journald: `journalctl --user -t settings4000`.

## Coding conventions

- **Follow the default Rust Style Guide.** All code adheres to the standard conventions enforced by `rustfmt` with its default configuration — no custom `rustfmt.toml` overrides. Naming, layout, and idioms match what an ordinary Rust developer expects. `cargo fmt --check` is a CI gate, so formatting is not optional.
- **Document with rustdoc.** Every public item (module, type, trait, function, field) carries a rustdoc comment (`///` for items, `//!` for module-level). Explain *what* it does and *why* it exists, not a restatement of the signature. Prefer rustdoc over free-floating comments for anything that belongs in the generated API docs.
- **Comment for an outside reader.** Write comments that make sense to a developer who has never seen this project and does not share our context. Spell out domain assumptions, the reasoning behind non-obvious choices, and references to the relevant **R…** requirement or `docs/` section when it clarifies intent. Do not write comments that only make sense to us (no private shorthand, no "as discussed", no unexplained references to conversations). If a comment would not help a newcomer, either rewrite it so it does or delete it.
- **Comment the non-obvious, not the obvious.** Useful comments explain intent, invariants, edge cases, and gotchas (especially the domain gotchas below). Avoid noise that merely narrates what the code plainly says.

## Commit rules

- **One commit per task.** Make a separate commit for each task from `docs/tasks.md`. Small changes may be combined into a single commit only when they genuinely belong together *and* can be clearly described in one line — if you cannot summarize them on one line, they do not belong in the same commit.
- **Subject line: one clear statement of what was done.** Write a single, self-contained line describing the change. Use it as the summary a reader scanning the log relies on.
- **Body: an optional bullet list for clarification.** When the one line is not enough, add a blank line and a bullet list below it giving the extra detail needed to understand the change. Only include a body when it adds real clarity.
- **Write for an outside reader.** The same rule as for documentation applies: someone outside the project reading the message must be able to understand it. It may be technical, but must not rely on insider information, private shorthand, or references only we would know.

## Architecture — the load-bearing rules

These are the constraints that are easy to violate and expensive to get wrong. Read `docs/architecture.md` for the full picture.

**Layering (hard rule): `core/` and `parsers/` never import `gtk`.** All UI staging/dirty/conflict logic lives in `core/` so it is headlessly testable (R6.2). A grep/module-boundary test enforces this. The `ui/` layer is thin: widgets emit `SetValue` messages and render from store state only.

**All side effects go through a `CommandRunner` trait.** No shell anywhere — commands are spawned with arg vectors (`std::process::Command`), 5 s timeout, exit/stderr captured and logged. Tests inject a mock recorder to assert the exact command sequence. This is non-negotiable for both testability (R6.1) and security (no injection surface).

**Parsers are surgical, never regenerating.** Each parser (`colors` kv, hyprlang, Lua literal, `monitors.conf`, INI, `uwsm/env`) produces a lossless line/token representation and edits only the targeted value span — comments, ordering, and commented-out lines stay byte-identical. Every parser has round-trip tests (`parse → edit nothing → emit == input`). The hyprlang parser (§3.2) was the highest-risk component (nested `section { }`, `source=`, and repeatable keys like `env=` each have their own addressing scheme); with the Lua switch it serves only `hypridle`/`hyprlock`/`hyprpaper`, and the risk moves to the planned **Lua-literal editor** (task 11.3): it edits `key = <literal>,` lines by table path in the schema-flat `input.lua` and exactly the two `hl.env(...)` lines in `hyprland.lua`, and treats everything else in that file as opaque code. The swaync JSON parser is retired with swaync (task 11.7).

**Staged edits, then Apply.** File-backed edits accumulate in `SettingsStore` (`original` vs `staged`, dirty = difference) until the user clicks Apply. Runtime-only controls (volume/mute, laptop-display toggle, and — since notifd has no config file — the whole Notifications page) bypass staging and apply immediately (R5.2). The Apply pipeline (`core/apply.rs`) runs a fixed order: validate all → conflict-check (re-read + hash) → atomic writes with per-file rollback → reload only components whose file changed *and* which detection found running.

**Writes target the XDG runtime path, atomically, following symlinks.** The app addresses every backing file by its live path (`$XDG_CONFIG_HOME`/`~/.config`, …), **never** a hardcoded `~/.dotfiles` path — so it works with or without the dotfiles deployment. The writer `fs::canonicalize`s first: a file symlinked into the dotfiles repo has its real target rewritten with the link preserved; a plain file is rewritten in place (temp file beside the resolved target → fsync → atomic rename). Apply is per-file transactional with in-memory pre-apply snapshots for rollback. Repo-only sources (palette `colors/`, `generate-colors`, `theme/fonts`) have no XDG path — they're found via the repo root resolved from a deployed symlink, and when absent the palette control is hidden like a missing app.

**Dynamic visibility from detection.** `core/detect.rs` runs once at startup producing a `Capabilities` struct (binary-on-PATH scan + daemon liveness + config readability). Each row/page declares a required capability; missing ones are cleanly hidden (whole category if emptied), never greyed out or errored. No detection failure aborts startup — errors degrade to "absent".

## Domain gotchas (from the real dotfiles)

- **Hyprland is configured in Lua; the `.conf` twins are dead.** `hyprland.lua` is the live config (`hyprctl status` → `configProvider: lua`); `hyprland.conf` and `input.conf` are a frozen fallback that Hyprland ignores while the `.lua` exists, and both flavours are symlinked into `~/.config/hypr`. Detect the provider (task 11.2) and write only the live flavour. On a Lua session `hyprctl keyword …` fails with `keyword can't work with non-legacy parsers. Use eval.` — live tweaks are `hyprctl eval 'hl.config({ … })'` / `hl.monitor({ … })`; `hyprctl reload`, `setcursor`, `monitors -j`, `getoption` and the hyprpaper IPC all still work. Prefer `hyprctl reload config-only` unless `monitors.conf` changed (a full reload re-applies monitor rules over the hotplug script's runtime state — docked behaviour still to be verified, task 11.11).
- **Palette: edit `colors/<scheme>`, never the generated files.** `colors/<scheme>` (bare-hex `key=value`, **18-key** schema — `shadow` was added; read the key list from the script's `NAMES=(…)` line) is the single source of truth. The generated files (seven color partials — `config/hypr/colors.conf`, **`colors.lua`** (header prefix `-- `), `_colors.scss`, etc. — plus three font partials and `state/active-scheme`) are **read-only inputs** — read only their `# Generated from …` header to detect the active scheme. Palette changes go through running `scripts/generate-colors <scheme>`, which must run **last** among write steps (rollback only restores app-written files, so ordering it last prevents leaving generated files on a new scheme after a rollback). `generate-colors` also depends on `theme/fonts` and aborts if it's missing/incomplete — a broken `theme/fonts` fails a palette apply too. The palette reload set is `hyprctl reload`, `eww reload`, SIGUSR1 to kitty — **no notification-daemon step**.
- **Duplicated values that a writer can desync:** cursor theme/size (now `breeze_cursors`/`24`) is declared in `config/uwsm/env` (canonical), the two `hl.env("XCURSOR_THEME", …)` / `hl.env("XCURSOR_SIZE", …)` lines of `hyprland.lua`, and `gtk-{3,4}.0/settings.ini` — the app writes each copy identically (`launchhyprland.sh` no longer exists; the `hyprland.conf` copy is frozen and not written). Wallpaper vs hyprlock background are separate keys in separate files (same path, now written as `~/Pictures/…` — expand `~` for validation). `scripts/hypr-display-profile.sh` *derives* eDP mode/scale by parsing `monitors.conf`, and so does `hyprland.lua` — the record grammar is **frozen** (`monitor=<output>,<mode>,<position>,<scale>[,<key>,<value>…]`, no commas/`#` in values); the app edits the `monitor=` record within that grammar and never touches either script.
- **Input settings live in `config/hypr/input.lua`** — one flat `hl.config({ input = { … } })` table that `hyprland.lua` loads with `dofile`. Keys are Lua literals (`kb_layout = "us,se",`, `natural_scroll = true,`, `tap_to_click` with an underscore); `kb_variant`/`kb_model`/`kb_rules` are absent and must be inserted when set. Section paths (`input.touchpad.natural_scroll`, …) keep their meaning. Non-cursor session env lives only in `config/uwsm/env` (+ `config/environment.d/` for SSH/kwallet); `hyprland.lua` keeps only the two cursor `hl.env` lines.
- **Notifications: swaync is gone, notifd has no config file.** `scripts/notifd` (Python/Gio, unit `notifd.service`, D-Bus `org.freedesktop.Notifications` + private `se.dotfiles.Notifd`) hard-codes timeouts/slots/history as constants; popups and the center are eww windows. The page is runtime-only: DND/clear/open-center through `se.dotfiles.Notifd` (`SetDnd`, `Clear`, `SetCenter`), state from `$XDG_RUNTIME_DIR/notifd/bar.json` (never the item files — they hold message bodies). Detect via D-Bus `NameHasOwner se.dotfiles.Notifd`; never probe `org.freedesktop.Notifications` with auto-start on, never run `swaync-client` (the `swaync` binary is still installed but its unit is masked), never SIGHUP notifd (that stops it). An icon-theme change needs `systemctl --user restart notifd.service` (it reads the icon theme once at startup).
- **Laptop-display toggle runs the dotfiles script.** `scripts/hypr-toggle-laptop-display` is a pure toggle that issues the `hl.monitor` call, maintains `/tmp/hypr-laptop-display-forced` and relaunches the eww bars; the app reads the state from `hyprctl monitors -j`, runs the script, and never writes a `monitor=…,disable` record, the state file, or an `eww open` itself. The app never restarts the eww daemon either (tray icons only sync at daemon start) — `eww reload` only, and its failure is non-fatal.
- **GTK settings have external writers.** kded6's gtkconfig module (suppressed at Hyprland start by `scripts/kded-gtkconfig off`) and `nwg-look` rewrite `settings.ini`/gsettings; on the development laptop `~/.config/gtk-4.0/settings.ini` is a **plain file** (de-symlinked by nwg-look) while `gtk-3.0/settings.ini` is still the repo link. Write the live file, report the divergence, never replace a symlink with a plain file, never write `gtk-xft-dpi`.
- **No libadwaita, no custom CSS.** The app inherits the system GTK theme (R2.1); libadwaita hard-codes Adwaita and ignores `gtk-theme-name`. Live restyle on theme change only works when `xdg-desktop-portal-gtk` or the dconf backend is present — detection gates whether the UI may claim live restyle vs "takes effect next launch".
- **`GTK_THEME` env override:** if set (in the app's env or uncommented in `uwsm/env`), show a banner and disable the GTK-theme drop-down — never fight the override. (Currently unset everywhere; the session is started by the display manager via `uwsm`, not a wrapper script.)
- **hyprlock gets no reload command** — it reads config at launch, so changes apply at next lock (intentional). Its grace period is CLI-only (hyprlock ≥ 0.9) and lives in the `--grace N` token of `hypridle.conf`'s `lock_cmd`.

## Non-functionals to respect

Cold start < 500 ms (R8.1); no root, user files only under `~/.dotfiles`/`~/.config` (R8.2); never break a working desktop — validate all staged values (hex, `WxH@Hz`, ranges, existing/readable image paths) before any write (R8.3); single instance via fixed GApplication ID (R8.4).
