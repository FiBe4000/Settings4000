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
