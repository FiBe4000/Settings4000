//! Settings4000 — a native GTK4 settings GUI for a dotfiles-managed Hyprland
//! desktop.
//!
//! The application edits the underlying configuration files of a Hyprland
//! desktop (display, sound, theme, input, notifications, power/idle, network)
//! and triggers the matching live-reloads, replacing hand-editing for common
//! user-facing settings. See `docs/requirements.md` for the numbered `R…`
//! requirements and `docs/architecture.md` for the module layout.
//!
//! # Module layout (architecture §2)
//!
//! - [`core`] — GTK-free domain logic: staging, detection, apply pipeline,
//!   typed settings model. Fully unit-testable headlessly (R6.2).
//! - [`parsers`] — one module per config file format; surgical, lossless edits.
//! - [`system`] — the side-effect boundary: command execution, file IO, logging.
//! - [`ui`] — the thin Relm4/GTK layer that renders from and stages into `core`.
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
mod core;
mod parsers;
mod system;
mod ui;

/// Program entry point.
///
/// This is a scaffold placeholder: CLI parsing and logging init (task 1.2) and
/// the `gtk::Application` bootstrap with single-instance activation (task 1.3)
/// are implemented in subsequent tasks. It exists so the crate builds and the
/// module tree is wired up.
fn main() {}
