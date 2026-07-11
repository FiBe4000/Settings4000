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

// File freshness / external-edit conflict tracking (architecture §3, §6 step 2;
// R5.6). It is consumed by the SettingsStore (task 4.2), which records a file's
// freshness when it reads it, and by the Apply pipeline's conflict-check step
// (task 4.5) — neither of which exists yet. So in a non-test build its public
// surface is exercised only by its own tests and would otherwise trip the
// `dead_code` lint. Scope the allowance to `not(test)` so the lint stays active
// in test builds (where the surface is used); remove it once 4.2/4.5 wire it in.
#[cfg_attr(not(test), allow(dead_code))]
pub mod freshness;

// Installed-app / capabilities detection (task 4.3; R4.1–R4.4, R2.2, R3.2, R8.5).
// It runs the binary/daemon/portal/palette-source/config probes once at startup
// and is re-run on manual refresh, producing a plain `Capabilities` struct. That
// struct is consumed by the UI (tasks 5.x/6.x) to hide unsupported rows/pages, by
// the reload table (task 4.4) to skip reloads for stopped components, and by the
// Apply pipeline (task 4.5) for the palette source's repo root — none of which
// exist yet. Until they wire it in, its public surface is exercised only by its own
// tests, so a non-test build would flag every item as dead code. Scope the
// allowance to `not(test)` so the `dead_code` lint stays active in test builds
// (where the surface is used); remove it once 4.4/4.5/5.x consume detection.
#[cfg_attr(not(test), allow(dead_code))]
pub mod detect;

// The typed settings model + validators (task 4.1; R8.3). It is consumed by the
// SettingsStore (task 4.2), which stores an `original`/`staged` `Value` per
// `SettingId`, and by the Apply pipeline (task 4.5), which validates every staged
// value before writing (architecture §6 step 1) — neither of which exists yet.
// Until they wire it in, its public surface is exercised only by its own tests, so
// a non-test build would flag every item as dead code. Scope the allowance to
// `not(test)` so the `dead_code` lint stays active in test builds (where the
// surface is used); remove it once 4.2/4.5 consume the model.
#[cfg_attr(not(test), allow(dead_code))]
pub mod model;

// The staging state machine (task 4.2; R5.1, R5.2, R5.6). It holds an
// `original`/`staged` `Value` per `SettingId`, tracks dirty state, and reloads
// originals on external-edit conflict. It builds on `model` and `freshness`, and is
// itself consumed by the UI (task 5.x) and the Apply pipeline (task 4.5) — neither
// of which exists yet — so in a non-test build its surface is exercised only by its
// own tests and would otherwise trip the `dead_code` lint. Scope the allowance to
// `not(test)` so the lint stays active in test builds (where the surface is used);
// remove it once the UI/apply pipeline wire the store in.
#[cfg_attr(not(test), allow(dead_code))]
pub mod store;

// The reload command table (task 4.4; architecture §6). It maps each changed
// backing file to the ordered, capability-gated reload actions its change requires
// and runs each action through the `CommandRunner`/`ProcessSignaller` seams. It is
// consumed by the Apply pipeline (task 4.5), which orders it after the file writes
// and decides how to surface reload failures.
#[cfg_attr(not(test), allow(dead_code))]
pub mod reload;

// The Apply pipeline orchestrator (task 4.5; architecture §6; R5.3–R5.6, R8.3). It
// runs the fixed order — validate all, conflict-check, atomic writes with per-file
// rollback, the palette `generate-colors` step last, then the changed+running
// reloads — over an `ApplyPlan` a page assembles from staged edits and the parsers,
// returning a structured `ApplyOutcome`. It ties together the writer (2.2),
// freshness (2.3), model (4.1), store (4.2), detection (4.3), and reload table
// (4.4). It is consumed by the UI Apply chrome (task 5.3), which is not wired in
// yet, so in a non-test build its public surface is exercised only by its own
// tests. Scope the `dead_code` allowance to `not(test)` so the lint stays active in
// test builds (where the surface is used); remove it once 5.3 consumes the pipeline.
#[cfg_attr(not(test), allow(dead_code))]
pub mod apply;

// The Display-page domain model (task 6.1; R2.3, R4.2, R4.4, R5.2, R5.4, R8.3). It
// merges the `monitors.conf` records with the live `hyprctl monitors -j` state into a
// per-monitor staging model, produces the `monitors.conf` FileWrite the Apply pipeline
// (task 4.5) applies, and drives the runtime-only laptop-display toggle. It is
// consumed by the Display page UI glue (`ui::display`) and the window's Apply/Reset
// chrome (`ui::window`), so its public surface is exercised in a non-test build too.
pub mod display;

// The Input-page domain logic (task 6.6; R2.3, R4.2, R4.4, R5.6, R8.3). It provides the
// store-`SettingId` -> `input.conf` write glue (rendering the store's dirty Input
// settings into one surgical `FileWrite` through the hyprlang writer) and the XKB
// keyboard-layout candidate list. Its `input.conf` freshness is the store's, not its
// own. It is consumed by the Input page UI glue (`ui::input`) and the window's Apply
// wiring (`ui::window`), so its public surface is exercised in a non-test build too.
pub mod input;

// The Notifications-page domain logic (task 6.7; R4.2, R4.4, R5.2, R5.6). It provides the
// store-`SettingId` -> `swaync/config.json` write glue (rendering the store's dirty
// position/timeout settings into one `FileWrite` through the swaync JSON adapter, position
// decomposed back into `positionY`/`positionX`) and the runtime-only do-not-disturb
// commands (`swaync-client --get-dnd`/`--dnd-on`/`--dnd-off`) — DND is live daemon state,
// not a config key, so it bypasses staging (R5.2). Its `config.json` freshness is the
// store's, not its own. It is consumed by the Notifications page UI glue
// (`ui::notifications`) and the window's Apply wiring (`ui::window`), so its public surface
// is exercised in a non-test build too.
pub mod notifications;

// The Power & Idle-page domain logic (task 6.8; R4.2, R4.4, R5.6, R8.3). It provides the
// store-`SettingId` -> `hypridle.conf` write glue: it renders the store's dirty dim/lock/
// DPMS timeouts and lock command into one surgical `FileWrite` through the hyprlang writer,
// addressing each timeout by positional listener matching (`listener[0]`/`[1]`/`[2]`, §3.2)
// and the lock command by `general.lock_cmd`, so editing one listener leaves the others
// byte-identical. Its `hypridle.conf` freshness is the store's, not its own; the Apply
// pipeline (task 4.5) follows the write with a hypridle restart (task 4.4). It is consumed
// by the window's Apply wiring (`ui::window`), so its public surface is exercised in a
// non-test build too.
pub mod power;

// The Sound-page domain model (task 6.2; R3.1, R5.2). It enumerates the PipeWire audio
// devices (from `pw-dump` JSON, falling back to parsing `wpctl status`) and builds the
// `wpctl` command vectors the runtime-only controls run — nothing is staged and nothing
// touches the store (R5.2). It is consumed by the Sound page UI glue (`ui::sound`), so
// its public surface is exercised in a non-test build too.
pub mod sound;

// The Theme-page palette-scheme model (task 6.3; R3.2, R4.2, R4.4, R8.5). It enumerates
// the switchable schemes from the discovered `colors/` directory, detects and preselects
// the active scheme from the generated `colors.conf` header (task 3.7), stages a pending
// switch, and produces the `apply::PaletteSwitch` the Apply pipeline (task 4.5) runs
// last (`generate-colors <scheme>` + the reload chain). Like the Display model it is a
// bespoke staging source folded into the shared Apply/Reset chrome; it is consumed by the
// Theme page UI glue (`ui::theme`) and the window, so its surface is exercised in a
// non-test build too.
pub mod theme;
