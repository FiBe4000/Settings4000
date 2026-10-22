//! GTK-free domain logic (architecture §2).
//!
//! Everything that can be reasoned about and tested without a display lives
//! here: the typed settings model and validators, the staging/dirty/conflict
//! state machine, installed-app detection, and the transactional apply
//! pipeline. Keeping this layer independent of the UI is what makes the core
//! behavior headlessly unit-testable (R6.2).
//!
//! Hard layering rule: this module and everything under it must never import
//! `gtk` or `relm4`. Side effects (running commands, writing files) are
//! reached only through the abstractions in [`crate::system`]. The rule is
//! enforced by `tests/module_boundaries.rs`; a violation fails the test suite.
//!
//! # Naming conventions
//!
//! Each recurring concept has one name across the whole crate — `core/`, `system/`
//! and, for the verbs the UI shares, [`crate::ui`], whose module docs add the repaint
//! verbs:
//!
//! - **dirty** — `is_dirty()` answers "does this hold a pending edit that differs from
//!   what its source holds?", at every level: one staged value (a store entry, a
//!   monitor, a theme selection), a whole page model, and the
//!   [`SettingsStore`](store::SettingsStore). Staging the original value back clears
//!   it. There is no second word (`is_changed`, `has_changes`) for the per-value case.
//! - **scan** — `scan_*` walks the filesystem or `/proc` and returns what it found:
//!   installed themes, palette schemes in `colors/`, binaries on `$PATH`, process
//!   names, PIDs. Inside [`crate::parsers`] the same prefix keeps its lexer sense —
//!   consuming one token from a byte offset (`scan_assignment`, `scan_short_string`,
//!   `scan_number`, `scan_record`) — which is a different concept from the walk.
//! - **detect** — `detect_*` answers whether one capability or condition holds on
//!   this host: a dotfiles script, the settings portal, the Hyprland config provider,
//!   an unreadable config, a `settings.ini` that is no longer a repo link.
//!   [`Capabilities::detect`](detect::Capabilities::detect) is the composite of them
//!   all. Neither concept is named `discover_*` or `enumerate_*`. `find_*` remains
//!   only for searches inside parsed text, and `check_*` only for validity checks — a
//!   value against a grammar or a file against the app's rules (`check_input_lua`,
//!   `check_string_value`, the kv and monitors `check_value`) — and for the freshness
//!   conflict check; none of them is discovery.
//! - **load** — `load` builds a model or state value from its source and returns it.
//!   The source may be files (`DisplayModel::load`, the [`load`] module's
//!   `load_*` builders) or a live daemon queried on each page entry
//!   ([`sound::load`], [`network::load`]); a live source does not get its own verb,
//!   because the function does the same thing either way. `reload` builds a fresh
//!   model by loading again from the sources an existing model was built from (the
//!   conflict path, R5.6).
//! - **refresh** — `refresh` re-queries the source of state that already exists and
//!   updates it in place ([`SettingsStore::refresh`](store::SettingsStore::refresh),
//!   [`NotificationsModel::refresh`](notifications::NotificationsModel::refresh)); the
//!   store's per-file step is its private `refresh_file`, which re-reads one changed
//!   file and folds it back into the originals.
//! - **read** — `read_*` is the step below `load`: read one source — a file or a
//!   single query — and return the parser's representation or a plain value, not a
//!   model ([`input::read_input_lua`], [`notifd_config::read_notifd_config`],
//!   [`notifd_config::read_values`] over an already-parsed file, the Display model's
//!   private `read_output_enabled` `hyprctl` query).
//!
//! `reload` as a model method is unrelated to the [`reload`] module, which tells a
//! *running component* (Hyprland, hypridle, notifd, …) to re-read its configuration
//! after an Apply (R5.5); a model's `reload` never runs a command of that kind.

// File freshness / external-edit conflict tracking (architecture §3, §6 step 2;
// R5.6). The SettingsStore (`store`) records a file's freshness when it reads
// it, the Apply pipeline's conflict-check step (`apply`) re-reads and compares
// before any write, and the bespoke page models (`display`, `theme`, …) track
// the backing files they own the same way.
pub mod freshness;

// Installed-app / capabilities detection (R4.1–R4.4, R2.2, R3.2, R8.5). It runs
// the binary/daemon/portal/palette-source/config probes once at startup and
// again on manual refresh, producing a plain `Capabilities` struct. The UI
// (`ui::window`, `ui::category`, `ui::row`) hides unsupported rows/pages from
// it, the reload table (`reload`) skips reloads for components it reports
// stopped, and the Apply pipeline takes the palette source's repo root from it.
pub mod detect;

// Home-relative (`~/…`) config values (task 11.10, R8.3): the one definition of
// which `~` form is expanded for validation and the file chooser, and of how a
// chosen image path is written back in the form the file already uses. Used by
// `model`'s image-path validator and `theme`'s wallpaper model; the startup load
// (`load`) takes the home directory from it.
pub mod home_path;

// The typed settings model + validators (R8.3). The SettingsStore (`store`)
// keeps an `original`/`staged` `Value` per `SettingId`, and the Apply pipeline
// (`apply`) validates every staged value against these validators before
// writing anything (architecture §6 step 1).
pub mod model;

// The staging state machine (R5.1, R5.2, R5.6). It holds an
// `original`/`staged` `Value` per `SettingId`, tracks dirty state, and reloads
// originals on external-edit conflict. It builds on `model` and `freshness`;
// the UI pages stage `SetValue` edits into it (via `ui::row`/`ui::page`) and
// the window's Apply chrome drains it into the Apply pipeline.
pub mod store;

// The reload command table (task 4.4; architecture §6; R5.5). It maps each changed
// backing file to the ordered, capability-gated reload actions its change requires —
// "reload only the components that changed and are running" — and runs each action
// through the `CommandRunner`/`ProcessSignaller` seams (no shell, arg vectors only).
// It is consumed by the Apply pipeline (`apply`), which orders the reloads after the
// file writes and decides how to surface reload failures.
pub mod reload;

// The Apply pipeline orchestrator (task 4.5; architecture §6; R5.3–R5.6, R8.3). It
// runs the fixed order — validate all, conflict-check, atomic writes with per-file
// rollback, the palette `generate-colors` step last, then the changed+running
// reloads — over an `ApplyPlan` assembled from staged edits and the parsers,
// returning a structured `ApplyOutcome`. It ties together the writer,
// freshness, model, store, detection, and reload table. The plan itself is built by
// `assemble` from every staging source; the window's Apply chrome (`ui::window`,
// `ui::chrome`) runs the pipeline and commits the store and page models on success.
pub mod apply;

// The write glue shared by the store-backed pages (task 9.17; architecture §6; R5.3, R8.3).
// `input`, `notifd_config` and `power` each stage every one of their settings in the shared
// `store` and write exactly one backing file, so the steps around their per-page renderer —
// no write when nothing is dirty, read the current bytes, render, wrap the result in an
// `apply::FileWrite` — are one implementation here rather than one copy per page. It also
// owns the one error type those failures share, whose contract is that the Apply aborts
// rather than skipping a file whose write could not be prepared.
pub mod store_write;

// The Display-page domain model (task 6.1; R2.3, R4.2, R4.4, R5.2, R5.4, R8.3). It
// merges the `monitors.conf` records with the live `hyprctl monitors -j` state into a
// per-monitor staging model, produces the `monitors.conf` FileWrite the Apply pipeline
// (task 4.5) applies, and drives the runtime-only laptop-display toggle. It is
// consumed by the Display page UI glue (`ui::display`) and the window's Apply/Reset
// chrome (`ui::window`), so its public surface is exercised in a non-test build too.
pub mod display;

// The Input-page domain logic (tasks 6.6, 11.4; R2.3, R4.2, R4.4, R5.6, R8.3). It
// provides the read/write address map of `input.lua`, the gate deciding whether that
// file can back the page, the store-`SettingId` -> `input.lua` write glue (rendering
// the store's dirty Input settings into one surgical `FileWrite` through the
// Lua-literal editor) and the XKB keyboard-layout candidate list. Its `input.lua`
// freshness is the store's, not its own. It is consumed by the Input page UI glue (`ui::input`) and the plan assembler
// (`assemble`), so its public surface is exercised in a non-test build too.
pub mod input;

// The Network-page domain model (task 6.9; R3.1, R4.2). It reads the active
// NetworkManager connections from terse `nmcli` output into a plain status the
// read-only Network page renders, and decides + runs the detached "Open Network
// Settings" launcher (`setsid --fork nm-connection-editor`, else `setsid --fork
// kitty -e nmtui`, per the detected capabilities). It is runtime-backed: nothing is
// staged and the store/Apply pipeline are never involved. It is consumed by the
// Network page UI glue (`ui::network`) and the window, so its public surface is
// exercised in a non-test build too.
pub mod network;

// The D-Bus surface of the notifd notification daemon (task 11.7; R4.1, R5.2, R6.1). It
// builds the exact `busctl`/`systemctl` argument vectors the app issues to notifd (every
// call to a notifd name with `--auto-start=no`), parses their replies strictly, and decides
// the `NotifdStatus` capability from captured probe replies. Detection (`detect`) gathers
// and resolves the capability with it, and the Notifications page model (`notifications`)
// drives the daemon through it.
pub mod notifd;

// The Notifications-page runtime model (task 11.7; R4.2, R5.2). These controls are
// runtime-only: it reads notifd's state (`GetState`, or `bar.json` on a daemon older than
// 1.1) on page entry and applies do-not-disturb, "open the notification center" and the
// confirmation-guarded "clear all" immediately through the `CommandRunner`, bypassing
// staging. It is consumed by the Notifications page UI glue (`ui::notifications`), so its
// public surface is exercised in a non-test build too.
pub mod notifications;

// The Notifications page's file-backed half (task 11.15; R5.1, R8.3): notifd's seven
// settings in `~/.config/notifd/config`, with the daemon's own ranges and defaults, read
// with its "last valid assignment wins" rule and written surgically through the shared
// `parsers::kv` line model. The settings are staged in `store`; this module's
// `NotifdConfigModel` renders the one write, which the pipeline follows with notifd's
// `Reload()` (`reload::BackingFile::NotifdConfig`). `model` calls its range check at
// stage time, the startup load (`load`) its reader, and the plan assembler
// (`assemble`) its write glue.
pub mod notifd_config;

// The Power & Idle-page domain logic (tasks 6.8, 11.13; R4.2, R4.4, R5.6, R8.3). It
// provides the store-`SettingId` -> `hypridle.conf` write glue: it renders the store's dirty
// dim/lock/DPMS timeouts, lock command and lock grace period into one surgical `FileWrite`
// through the hyprlang writer, addressing each timeout by positional listener matching
// (`listener[0]`/`[1]`/`[2]`, §3.2) and the lock command by `general.lock_cmd` — the grace
// period being a `--grace N` token edit on that same value — so editing one listener leaves
// the others byte-identical. Its `hypridle.conf` freshness is the store's, not its own; the Apply
// pipeline (task 4.5) follows the write with a hypridle restart (task 4.4). It is consumed
// by the plan assembler (`assemble`), so its public surface is exercised in a non-test
// build too.
pub mod power;

// The `--grace N` token editor for hypridle's `lock_cmd` value (task 11.13; architecture
// §3). hyprlock ≥ 0.9 takes its grace period only on the command line, so the Power &
// Idle page's grace control edits one token inside that free-text command; this module
// reads and rewrites that token, leaving every other byte of the command alone. It is
// shared by the `hypridle.conf` loader, the store (which keeps the lock command and its
// grace period in step) and the `power` write glue.
pub mod lock_grace;

// The Sound-page domain model (task 6.2; R3.1, R5.2). It loads the PipeWire audio
// devices (from `pw-dump` JSON, falling back to parsing `wpctl status`) and builds the
// `wpctl` command vectors the runtime-only controls run — nothing is staged and nothing
// touches the store (R5.2). It is consumed by the Sound page UI glue (`ui::sound`), so
// its public surface is exercised in a non-test build too.
pub mod sound;

// The Theme-page palette-scheme model (task 6.3; R3.2, R4.2, R4.4, R8.5). It scans
// the switchable schemes from the detected `colors/` directory, detects and preselects
// the active scheme from the generated `colors.conf` header (task 3.7), stages a pending
// switch, and produces the `apply::PaletteSwitch` the Apply pipeline (task 4.5) runs
// last (`generate-colors <scheme>` + the reload chain). Like the Display model it is a
// bespoke staging source folded into the shared Apply/Reset chrome; it is consumed by the
// Theme page UI glue (`ui::theme`) and the window, so its surface is exercised in a
// non-test build too.
pub mod theme;

// The Apply-plan assembler (task 9.16; architecture §6; R5.3–R5.6, R8.3). It sits above
// every other module here: it asks the store and each bespoke page model (`display`,
// `input`, `power`, `theme`) for its contribution and folds them into the
// one `apply::ApplyPlan` the pipeline runs — in a fixed order, refusing to plan anything
// when a model's files changed on disk or a write cannot be prepared. It is the front half
// of the window's Apply button, kept here so those decisions are testable without a
// display (R6.2); the window (`ui::window`) runs the resulting plan, surfaces the outcome
// through `ui::chrome`, and commits the sources the assembler reports as contributing.
pub mod assemble;

// The startup load (tasks 5.4, 9.21; architecture §8; R4.3, R4.4, R6.2). It runs detection
// (`detect`), reads and parses every store-backed config file into the `store`'s originals
// and re-readers, builds the bespoke page models (`display`, `input`, `notifd_config`,
// `power`, `theme`) for the detected capabilities, and holds the refresh rules that merge a
// later pass into live state. It is GTK-free so it runs on a worker thread and is tested
// headlessly; the UI layer (`ui::startup`) only moves it onto that thread, and the window
// (`ui::window`) applies its `StartupLoad` on the main thread.
pub mod load;
