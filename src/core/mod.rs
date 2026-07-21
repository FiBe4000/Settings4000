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
// freshness, model, store, detection, and reload table. The window's Apply
// chrome (`ui::window`, `ui::chrome`) assembles the plan, runs the pipeline,
// and commits the store and page models on success.
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

// The Network-page domain model (task 6.9; R3.1, R4.2). It reads the active
// NetworkManager connections from terse `nmcli` output into a plain status the
// read-only Network page renders, and decides + runs the detached "Open Network
// Settings" launcher (`setsid --fork nm-connection-editor`, else `setsid --fork
// kitty -e nmtui`, per the detected capabilities). It is runtime-backed: nothing is
// staged and the store/Apply pipeline are never involved. It is consumed by the
// Network page UI glue (`ui::network`) and the window, so its public surface is
// exercised in a non-test build too.
pub mod network;

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
