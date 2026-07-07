//! Tracing/logging initialization (R7.1, R7.2).
//!
//! Settings4000 logs through the `tracing` ecosystem. In a normal desktop
//! session logs go to the systemd journal so they are retrievable with
//! `journalctl --user -t settings4000`; when the journal is unavailable (no
//! systemd, a sandbox without the journald socket, running the binary directly
//! from a shell without a session bus, …) the app transparently falls back to a
//! human-readable formatter on stderr so no diagnostics are lost (R7.1).
//!
//! Log verbosity is controlled by an `EnvFilter` directive resolved from, in
//! decreasing precedence, the `--log-level` CLI flag, the `SETTINGS4000_LOG`
//! environment variable, the `RUST_LOG` environment variable, and finally a
//! built-in `info` default (R7.2). That `info` default is applied explicitly
//! when the filter is built, so a *set but malformed* directive (from either
//! environment variable) degrades to `info` rather than to `tracing`'s own
//! built-in `error` fallback — keeping the documented default true.
//!
//! The `--log-level` flag deliberately raises only this crate's verbosity:
//! `--log-level debug` maps to `info,settings4000=debug` so the app's own
//! `debug` output (parsed values, staged diffs — R7.3) is not buried under the
//! `debug` chatter of its GUI dependencies (GTK/GLib/Relm4). A power user who
//! genuinely wants a dependency at `debug` can still pass a full directive
//! through `SETTINGS4000_LOG`/`RUST_LOG`.
//!
//! The two side-effecting concerns — *which* backend is reachable and *how* the
//! subscriber is wired — are kept separate from the pure precedence logic so the
//! stderr-fallback path can be exercised in a unit test by simulating an absent
//! journald socket, without depending on the host actually lacking journald.

use std::io;
use std::io::IsTerminal;

use tracing::Subscriber;
use tracing::level_filters::LevelFilter;
use tracing_journald::Layer as JournaldLayer;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry;
use tracing_subscriber::util::SubscriberInitExt;

/// The journald `SYSLOG_IDENTIFIER` field the app tags every entry with.
///
/// Setting it explicitly (rather than relying on journald deriving it from the
/// process name) guarantees `journalctl --user -t settings4000` finds the logs
/// no matter how the binary was launched (via `$PATH`, an absolute path, or a
/// renamed symlink) — the acceptance criterion for R7.1.
const SYSLOG_IDENTIFIER: &str = "settings4000";

/// A log verbosity level selectable on the command line via `--log-level`.
///
/// These four levels mirror the `tracing`/`EnvFilter` level names. A chosen
/// value becomes an `EnvFilter` directive via [`LogLevel::directive`]. `Debug`
/// is scoped to this crate (on top of an `info` floor) so `--log-level debug`
/// surfaces the app's own diagnostics without the flood of `debug` events its
/// GUI dependencies emit (see the module docs); the quieter levels apply to
/// every target.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum LogLevel {
    /// Everything, including parsed values and staged diffs (R7.3).
    Debug,
    /// Normal operational logging: detection results, writes, reloads (R7.3).
    Info,
    /// Recoverable problems, e.g. an unreadable config file (R4.4).
    Warn,
    /// Failures only.
    Error,
}

impl LogLevel {
    /// Returns the `EnvFilter` directive string for this level.
    ///
    /// `Debug` maps to `"info,settings4000=debug"`: an `info` floor for all
    /// targets plus this crate raised to `debug`, so `--log-level debug`
    /// surfaces the app's own `debug` output (parsed values and staged diffs,
    /// R7.3) without the flood of `debug` events GTK/GLib/Relm4 would otherwise
    /// emit. The quieter levels map to the bare level name and apply to every
    /// target, since none is verbose enough for dependency chatter to bury the
    /// app's logs.
    #[must_use]
    pub(crate) const fn directive(self) -> &'static str {
        match self {
            // `settings4000` is this crate's tracing target (its module-path
            // root), so scoping the directive to it raises only the app while
            // leaving dependencies at the `info` floor.
            LogLevel::Debug => "info,settings4000=debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }
}

/// Which logging backend the subscriber was wired up with.
///
/// Returned by [`init`] so callers (and tests) can observe whether the journal
/// was reachable or the stderr fallback was used.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum LogBackend {
    /// Logs are written to the systemd journal (`journalctl --user -t
    /// settings4000`).
    Journald,
    /// The journal was unavailable, so logs are formatted to stderr instead.
    StderrFallback,
}

/// Installs the global `tracing` subscriber and returns the backend it selected.
///
/// The verbosity directive is resolved from `cli_level` and the
/// `SETTINGS4000_LOG`/`RUST_LOG` environment variables (see
/// [`resolve_directive`] for precedence), a journald layer is attempted first,
/// and the stderr formatter is used only if journald cannot be reached (R7.1,
/// R7.2).
///
/// This must be called at most once per process; a second call cannot replace
/// the already-installed global subscriber and is reported to stderr rather than
/// panicking, since it indicates a programming error rather than a runtime
/// condition.
pub(crate) fn init(cli_level: Option<LogLevel>) -> LogBackend {
    let directive = resolve_directive(
        cli_level,
        std::env::var("SETTINGS4000_LOG").ok(),
        std::env::var("RUST_LOG").ok(),
    );
    let filter = build_filter(&directive);

    // `journald_layer` and `io::stderr` are the real backends; `build_subscriber`
    // stays free of them so tests can inject substitutes. ANSI color escapes are
    // only useful on an interactive terminal, so the fallback formatter is told
    // whether the real stderr is a TTY — redirected or piped logs then stay
    // plain text instead of carrying stray escape sequences.
    let (backend, subscriber) = build_subscriber(
        filter,
        journald_layer,
        io::stderr,
        io::stderr().is_terminal(),
    );

    if let Err(error) = subscriber.try_init() {
        // The subscriber could not be installed as the global default. This only
        // happens if one is already set (i.e. `init` was called twice), so we
        // have no working `tracing` sink to report through — write to stderr.
        eprintln!("settings4000: could not install the tracing subscriber: {error}");
        return backend;
    }

    if backend == LogBackend::StderrFallback {
        tracing::warn!("systemd journald is unavailable; logging to stderr instead (R7.1)");
    }
    tracing::debug!(directive = %directive, backend = ?backend, "logging initialized");

    backend
}

/// Resolves the `EnvFilter` directive string from the flag and environment,
/// applying the R7.2 precedence: the `--log-level` flag wins over
/// `SETTINGS4000_LOG`, which wins over `RUST_LOG`; with none set, the default is
/// `info`.
///
/// Environment values are used verbatim (so a full directive such as
/// `settings4000=debug,warn` is honored), except that a blank/whitespace-only
/// value is treated as unset so an accidentally-empty variable does not silence
/// logging. This function is pure — it takes the environment values as arguments
/// rather than reading them — so the precedence rules are unit-testable.
#[must_use]
pub(crate) fn resolve_directive(
    cli_level: Option<LogLevel>,
    settings4000_log: Option<String>,
    rust_log: Option<String>,
) -> String {
    if let Some(level) = cli_level {
        return level.directive().to_owned();
    }
    if let Some(value) = non_empty(settings4000_log) {
        return value;
    }
    if let Some(value) = non_empty(rust_log) {
        return value;
    }
    LogLevel::Info.directive().to_owned()
}

/// Discards an environment value that is absent or only whitespace, so it is
/// treated identically to an unset variable in [`resolve_directive`].
fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

/// Builds the global `EnvFilter` from a resolved directive string, applying the
/// R7.2 `info` default explicitly.
///
/// `EnvFilter::new` (and the `parse_lossy` it uses) tolerates a malformed
/// directive by discarding the unparseable parts, but its built-in fallback
/// default is `error`. That would silently raise the effective floor to `error`
/// for a *set but invalid* directive (e.g. `SETTINGS4000_LOG="!!!"`),
/// contradicting the documented `info` default. Setting the default directive
/// to `info` here makes such a directive degrade to `info` instead, so the
/// promise in the module docs holds.
fn build_filter(directive: &str) -> EnvFilter {
    EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .parse_lossy(directive)
}

/// Builds the journald layer tagged with the app's syslog identifier (R7.1).
///
/// Returns the same `io::Error` as [`tracing_journald::layer`] when the journal
/// socket cannot be opened, which is the signal [`build_subscriber`] uses to
/// switch to the stderr fallback.
fn journald_layer() -> io::Result<JournaldLayer> {
    tracing_journald::layer()
        .map(|layer| layer.with_syslog_identifier(SYSLOG_IDENTIFIER.to_owned()))
}

/// Assembles the subscriber: `filter` gates every event globally, then either
/// the journald layer (when `make_journald` succeeds) or an stderr formatter
/// writing through `make_writer` (when it fails) is attached.
///
/// The journald factory and the fallback writer are injected rather than
/// hard-coded so a test can force the "journald unavailable" branch and capture
/// the fallback output without a live journal. `ansi` likewise carries the
/// caller's `stderr().is_terminal()` decision so the color-escape choice stays
/// injectable: escapes are only useful on an interactive terminal, so passing
/// `false` (as tests and redirected output do) keeps the formatted lines plain.
/// Both candidate layers are held as `Option`s (a `None` layer is a no-op) so
/// the two branches share one concrete subscriber type, keeping this a single
/// return path.
fn build_subscriber<J, MW>(
    filter: EnvFilter,
    make_journald: J,
    make_writer: MW,
    ansi: bool,
) -> (LogBackend, impl Subscriber + Send + Sync + 'static)
where
    J: FnOnce() -> io::Result<JournaldLayer>,
    MW: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
    let (backend, journald_layer) = match make_journald() {
        Ok(layer) => (LogBackend::Journald, Some(layer)),
        Err(_) => (LogBackend::StderrFallback, None),
    };

    let fmt_layer = match backend {
        LogBackend::StderrFallback => Some(fmt::layer().with_ansi(ansi).with_writer(make_writer)),
        LogBackend::Journald => None,
    };

    let subscriber = registry().with(filter).with(journald_layer).with(fmt_layer);
    (backend, subscriber)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn flag_overrides_both_environment_variables() {
        // R7.2: the `--log-level` flag wins over the environment. The flag also
        // scopes `debug` to this crate (see `LogLevel::directive`).
        let directive = resolve_directive(
            Some(LogLevel::Debug),
            Some("error".to_owned()),
            Some("warn".to_owned()),
        );
        assert_eq!(directive, "info,settings4000=debug");
    }

    #[test]
    fn settings4000_log_takes_precedence_over_rust_log() {
        let directive = resolve_directive(None, Some("warn".to_owned()), Some("error".to_owned()));
        assert_eq!(directive, "warn");
    }

    #[test]
    fn rust_log_is_used_when_settings4000_log_is_absent_or_blank() {
        assert_eq!(
            resolve_directive(None, None, Some("error".to_owned())),
            "error"
        );
        // A whitespace-only SETTINGS4000_LOG is treated as unset.
        assert_eq!(
            resolve_directive(None, Some("   ".to_owned()), Some("error".to_owned())),
            "error"
        );
    }

    #[test]
    fn defaults_to_info_when_nothing_is_set() {
        assert_eq!(resolve_directive(None, None, None), "info");
        // Empty strings from set-but-blank variables also fall through to the default.
        assert_eq!(
            resolve_directive(None, Some(String::new()), Some(String::new())),
            "info"
        );
    }

    #[test]
    fn log_level_maps_to_expected_directive() {
        // `Debug` is scoped to this crate on top of an `info` floor (R7.3); the
        // quieter levels are bare, global directives.
        assert_eq!(LogLevel::Debug.directive(), "info,settings4000=debug");
        assert_eq!(LogLevel::Info.directive(), "info");
        assert_eq!(LogLevel::Warn.directive(), "warn");
        assert_eq!(LogLevel::Error.directive(), "error");
    }

    #[test]
    fn malformed_directive_degrades_to_info_default() {
        // R7.2: a set-but-unparseable directive must degrade to the documented
        // `info` default. `EnvFilter`'s own `parse_lossy` fallback is `error`,
        // so the explicit `info` default in `build_filter` is what keeps the
        // promise — this test fails if that default is dropped.
        let subscriber = registry().with(build_filter("!!!"));
        assert_eq!(subscriber.max_level_hint(), Some(LevelFilter::INFO));
    }

    #[test]
    fn debug_flag_raises_only_this_crate() {
        // R7.3: `--log-level debug` scopes `debug` to this crate on top of an
        // `info` floor, so the overall maximum enabled level is `debug` (the
        // app is at `debug`) while dependencies stay at `info`.
        let subscriber = registry().with(build_filter(LogLevel::Debug.directive()));
        assert_eq!(subscriber.max_level_hint(), Some(LevelFilter::DEBUG));
    }

    /// A `MakeWriter` that appends everything written to a shared buffer, so a
    /// test can capture what the stderr fallback formatter would have emitted.
    #[derive(Clone)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl io::Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("log capture buffer mutex was poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> MakeWriter<'writer> for SharedBuffer {
        type Writer = SharedBuffer;

        fn make_writer(&'writer self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn falls_back_to_stderr_layer_when_journald_is_unavailable() {
        // R7.1 acceptance: exercise the fallback with journald absent, without
        // depending on the host actually lacking a journal. The injected factory
        // returns the same `NotFound` error `tracing_journald::layer` produces
        // when the journald socket cannot be opened.
        let journald_absent = || -> io::Result<JournaldLayer> {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "simulated: journald socket unavailable",
            ))
        };

        let captured = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = SharedBuffer(captured.clone());

        let (backend, subscriber) =
            build_subscriber(EnvFilter::new("info"), journald_absent, writer, false);

        // The selection must be the stderr fallback.
        assert_eq!(backend, LogBackend::StderrFallback);

        // Install the subscriber only for this thread (avoiding the process-wide
        // global) and confirm an event actually reaches the fallback formatter,
        // proving the layer wiring — not just the selection — is correct.
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("settings4000 stderr fallback probe");
        });

        let output = String::from_utf8(
            captured
                .lock()
                .expect("log capture buffer mutex was poisoned")
                .clone(),
        )
        .expect("captured log output was not valid UTF-8");
        assert!(
            output.contains("settings4000 stderr fallback probe"),
            "the stderr fallback layer did not receive the log line; captured: {output:?}"
        );

        // With ANSI disabled (the case when stderr is not a TTY) the formatter
        // must emit no color-escape codes, so redirected logs stay plain text.
        assert!(
            !output.contains('\u{1b}'),
            "the fallback formatter emitted ANSI escapes with ansi=false; captured: {output:?}"
        );
    }
}
