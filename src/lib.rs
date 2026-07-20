//! Settings4000 — a native GTK4 settings GUI for a dotfiles-managed Hyprland
//! desktop.
//!
//! The application edits the underlying configuration files of a Hyprland
//! desktop (display, sound, theme, input, notifications, power/idle, network)
//! and triggers the matching live-reloads, replacing hand-editing for common
//! user-facing settings. See `docs/requirements.md` for the numbered `R…`
//! requirements and `docs/architecture.md` for the module layout.
//!
//! # Library + binary split
//!
//! The crate builds as a library plus a thin binary (`src/main.rs`) that only
//! parses the CLI, initializes logging, and runs [`ui::app::run`]. The library
//! target exists for the integration tests in `tests/`: they compile as
//! separate crates, so the parsers, the core domain logic, and the system seams
//! they exercise must be reachable through a public API (R6.1) — the headless
//! layers ([`core`], [`parsers`], [`system`]) are therefore `pub` throughout,
//! while [`ui`] stays internal apart from the [`ui::app`] entry point the
//! binary needs. There is no external consumer: the public surface is a
//! test-access contract, not a semver commitment.
//!
//! # Module layout (architecture §2)
//!
//! - [`core`] — GTK-free domain logic: staging, detection, apply pipeline,
//!   typed settings model. Fully unit-testable headlessly (R6.2).
//! - [`parsers`] — one module per config file format; surgical, lossless edits.
//! - [`system`] — the side-effect boundary: command execution, file IO, logging.
//! - [`ui`] — the thin Relm4/GTK layer that renders from and stages into `core`.
//! - [`testing`] — test-only support (the fixture-dotfiles installer, task 7.1),
//!   compiled only for `cfg(test)` or the `testing` feature.
//!
//! # Layering rule (hard constraint)
//!
//! `core` and `parsers` never import `gtk`/`relm4`; all UI-independent logic
//! lives there so it can be tested without a display. This is enforced by
//! `tests/module_boundaries.rs`.

// The `core` module deliberately shares its name with the `core` standard
// library crate (per architecture §2). Within this crate `core::` resolves to
// this module; the std crate remains reachable as `::core` on the rare occasion
// it is needed. The names do not otherwise conflict.
pub mod core;
pub mod parsers;
pub mod system;

// Test-support code (R6.1): the fixture-dotfiles installer integration suites
// use to materialize an anonymized copy of the real dotfiles tree per test
// (task 7.1). Gated so it exists for in-crate unit tests and for test targets
// (which enable the `testing` feature via the self dev-dependency), but never
// in a release build.
#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub mod ui;
