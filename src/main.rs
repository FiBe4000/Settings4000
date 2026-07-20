//! The Settings4000 binary — a thin wrapper over the `settings4000` library.
//!
//! All application logic lives in the library crate (see `src/lib.rs` for the
//! module layout and layering rules); this entry point only parses the command
//! line, initializes logging, and hands control to the GTK application
//! bootstrap in [`settings4000::ui::app`].

use clap::Parser;
use gtk4::glib;

use settings4000::system::logging::LogLevel;
use settings4000::{system, ui};

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
    // Captured before anything else runs, so the startup benchmark mark (task
    // 7.3) measures the whole process — CLI parsing, logging setup, GTK init,
    // and window construction — against the R8.1 cold-start budget. The UI
    // layer logs the elapsed time when the window's first frame is painted
    // (see `ui::app`); the small pre-`main` runtime setup (dynamic linking,
    // crt0) is the only part of the process this cannot see.
    let process_started = std::time::Instant::now();

    let cli = Cli::parse();

    let backend = system::logging::init(cli.log_level);

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        backend = ?backend,
        "settings4000 starting"
    );

    ui::app::run(process_started)
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
