# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Current state

**This repo is in the planning stage — no code exists yet.** There is no `Cargo.toml`, no `src/`, and no commits; the only content is design docs under `docs/`. The first implementation task is scaffolding the crate (see `docs/tasks.md` §1.1). Until then, "commands" below describe the intended toolchain, not something you can run today.

The authoritative specs, in order of use:
- `docs/requirements.md` — what to build (numbered **R…** requirements, referenced everywhere).
- `docs/architecture.md` — how to build it (module layout, parser strategies, apply pipeline).
- `docs/tasks.md` — atomic, ordered implementation breakdown with acceptance criteria.
- `docs/dotfiles_analysis.md` — the concrete `~/.dotfiles` layout this app reads/writes.

When implementing, treat the R-numbers as the contract and update `docs/tasks.md` checkboxes as tasks land.

## What this is

Settings4000 is a native GTK4 GUI (Rust + Relm4) that edits the config files of a `~/.dotfiles`-managed Hyprland desktop and triggers the right live-reloads — replacing manual config editing for common user-facing settings (display, sound, theme, input, notifications, power/idle, network).

## Commands (once scaffolded)

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

**Parsers are surgical, never regenerating.** Each parser (`colors` kv, hyprlang, `monitors.conf`, swaync JSON, INI, `uwsm/env`) produces a lossless line/token representation and edits only the targeted value span — comments, ordering, and commented-out lines stay byte-identical. Every parser has round-trip tests (`parse → edit nothing → emit == input`). The hyprlang parser (§3.2) is the highest-risk component; nested `section { }`, `source=`, and repeatable keys (`env=`, `exec-once=`) each have their own addressing scheme.

**Staged edits, then Apply.** File-backed edits accumulate in `SettingsStore` (`original` vs `staged`, dirty = difference) until the user clicks Apply. Runtime-only controls (volume/mute, laptop-display toggle) bypass staging and apply immediately (R5.2). The Apply pipeline (`core/apply.rs`) runs a fixed order: validate all → conflict-check (re-read + hash) → atomic writes with per-file rollback → reload only components whose file changed *and* which detection found running.

**Writes target the XDG runtime path, atomically, following symlinks.** The app addresses every backing file by its live path (`$XDG_CONFIG_HOME`/`~/.config`, …), **never** a hardcoded `~/.dotfiles` path — so it works with or without the dotfiles deployment. The writer `fs::canonicalize`s first: a file symlinked into the dotfiles repo has its real target rewritten with the link preserved; a plain file is rewritten in place (temp file beside the resolved target → fsync → atomic rename). Apply is per-file transactional with in-memory pre-apply snapshots for rollback. Repo-only sources (palette `colors/`, `generate-colors`, `theme/fonts`) have no XDG path — they're found via the repo root resolved from a deployed symlink, and when absent the palette control is hidden like a missing app.

**Dynamic visibility from detection.** `core/detect.rs` runs once at startup producing a `Capabilities` struct (binary-on-PATH scan + daemon liveness + config readability). Each row/page declares a required capability; missing ones are cleanly hidden (whole category if emptied), never greyed out or errored. No detection failure aborts startup — errors degrade to "absent".

## Domain gotchas (from the real dotfiles)

- **Palette: edit `colors/<scheme>`, never the generated files.** `colors/<scheme>` (bare-hex `key=value`, fixed 17-key schema) is the single source of truth. The generated files (six color partials — `config/hypr/colors.conf`, `_colors.scss`, etc. — plus three font partials and `state/active-scheme`, see below) are **read-only inputs** — read only their `# Generated from …` header to detect the active scheme. Palette changes go through running `scripts/generate-colors <scheme>`, which must run **last** among write steps (rollback only restores app-written files, so ordering it last prevents leaving generated files on a new scheme after a rollback). Note `generate-colors` now also depends on `theme/fonts` and aborts if it's missing/incomplete — a broken `theme/fonts` fails a palette apply too.
- **Duplicated values that a writer can desync:** cursor theme/size (now unified to `Nordic-cursors`/`16`) is declared in *both* `config/hypr/hyprland.conf` env and `config/uwsm/env` (plus `gtk-{3,4}.0/settings.ini` and `launchhyprland.sh`) — the app writes each copy identically. Wallpaper vs hyprlock background are separate keys in separate files (now the same path). Note `scripts/hypr-display-profile.sh` now *derives* eDP mode/scale by parsing `monitors.conf`, so it is the single source — the app edits the `monitor=` record (keeping it awk-parseable) and never touches the script.
- **Input settings live in `config/hypr/input.conf`**, not `hyprland.conf` — the `input { }` block was extracted into a `source=`d, app-owned file. The hyprlang writer targets `input.conf` for keyboard/touchpad/sensitivity; section paths (`input.touchpad.natural_scroll`, …) are unchanged. Non-cursor session env now lives only in `config/uwsm/env`; `hyprland.conf` keeps only the two cursor `env =` lines.
- **Laptop-display toggle uses the existing hotplug mechanism** (`/tmp/hypr-laptop-display-forced` state file + the `scripts/hypr-monitor-hotplug` path), applied immediately — never a `monitor=…,disable` record, which the hotplug watcher would fight.
- **No libadwaita, no custom CSS.** The app inherits the system GTK theme (R2.1); libadwaita hard-codes Adwaita and ignores `gtk-theme-name`. Live restyle on theme change only works when `xdg-desktop-portal-gtk` or the dconf backend is present — detection gates whether the UI may claim live restyle vs "takes effect next launch".
- **`GTK_THEME` env override:** if set (in the app's env or uncommented in `uwsm/env`), show a banner and disable the GTK-theme drop-down — never fight the override.
- **hyprlock gets no reload command** — it reads config at launch, so changes apply at next lock (intentional).

## Non-functionals to respect

Cold start < 500 ms (R8.1); no root, user files only under `~/.dotfiles`/`~/.config` (R8.2); never break a working desktop — validate all staged values (hex, `WxH@Hz`, ranges, existing/readable image paths) before any write (R8.3); single instance via fixed GApplication ID (R8.4).
