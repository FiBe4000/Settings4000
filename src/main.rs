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

use clap::Parser;
use gtk4::glib;

use crate::system::logging::LogLevel;

/// Command-line interface for Settings4000 (R7.2).
///
/// Only logging configuration is exposed today. `clap` owns the command line in
/// full: `main` parses it here before starting GTK, and the application then
/// forwards only the program name (`argv[0]`) to GTK's own option handling (see
/// [`ui::app::run`]), so GTK never sees or re-parses these flags.
#[derive(Debug, Parser)]
#[command(name = "settings4000", version, about)]
struct Cli {
    /// Minimum log level (`debug`, `info`, `warn`, `error`).
    ///
    /// When set, this overrides the `SETTINGS4000_LOG` and `RUST_LOG`
    /// environment variables; when unset, those are consulted, defaulting to
    /// `info` (R7.2).
    #[arg(long, value_name = "LEVEL")]
    log_level: Option<LogLevel>,
}

/// Program entry point.
///
/// Parses the command line, initializes logging (task 1.2), then builds and runs
/// the single-instance `gtk4::Application` (task 1.3). The returned
/// [`glib::ExitCode`] becomes the process exit status: on a relaunch that
/// activates an already-running instance it is success (0), per R8.4.
fn main() -> glib::ExitCode {
    let cli = Cli::parse();

    let backend = system::logging::init(cli.log_level);

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        backend = ?backend,
        "settings4000 starting"
    );

    ui::app::run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_level_flag_parses_to_the_matching_level() {
        let cli = Cli::try_parse_from(["settings4000", "--log-level", "debug"])
            .expect("`--log-level debug` should parse");
        assert_eq!(cli.log_level, Some(LogLevel::Debug));
    }

    #[test]
    fn log_level_defaults_to_none_when_flag_is_absent() {
        let cli = Cli::try_parse_from(["settings4000"]).expect("no flags should parse");
        assert_eq!(cli.log_level, None);
    }

    #[test]
    fn an_invalid_log_level_is_rejected() {
        // clap validates against the `LogLevel` value set, so a typo is an error
        // rather than being silently ignored.
        let result = Cli::try_parse_from(["settings4000", "--log-level", "verbose"]);
        assert!(result.is_err());
    }
}
