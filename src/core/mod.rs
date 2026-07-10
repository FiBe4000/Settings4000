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
// and decides how to surface reload failures — and which does not exist yet. Until
// it wires the table in, the public surface is exercised only by this module's own
// tests, so a non-test build would flag every item as dead code. Scope the
// allowance to `not(test)` so the lint stays active in test builds (where the
// surface is used); remove it once 4.5 consumes the table.
#[cfg_attr(not(test), allow(dead_code))]
pub mod reload;
