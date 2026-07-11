//! GApplication bootstrap and single-instance handling (task 1.3, R8.4).
//!
//! This module owns the process-level GTK entry point: it builds the
//! [`gtk4::Application`], wires up window creation, and enforces single-instance
//! behavior so that launching Settings4000 while a copy is already running
//! raises the existing window instead of opening a second one.
//!
//! # Single instance (R8.4)
//!
//! GTK's `Application` implements single-instance behavior through the
//! freedesktop D-Bus "application" protocol: the first process to register a
//! given application ID becomes the *primary* instance and owns the ID on the
//! session bus; any later process that registers the same ID becomes a *remote*
//! instance. Registering (via [`gtk4::gio::prelude::ApplicationExt::register`])
//! is therefore the step that decides our role, which is why the requirement
//! (R8.4) and the startup sequence (architecture §8) call it out explicitly.
//!
//! We then hand control to `run_with_args`, which drives both roles correctly:
//! on the primary it emits the `activate` signal (building and showing the
//! window) and runs the GTK main loop; on a remote launch it forwards
//! `activate` to the primary — which presents its already-open window — flushes
//! the D-Bus message, and returns exit code 0, so the second process exits
//! cleanly with no second window. (Sending the activation ourselves and exiting
//! immediately would risk dropping the still-buffered D-Bus message;
//! `run_with_args` flushes it for us.)
//!
//! # CLI / GApplication argument handling
//!
//! Settings4000 parses its own command line with `clap` in `main` before this
//! runs (currently just `--log-level`, R7.2). GTK would otherwise try to parse
//! `std::env::args()` itself and reject that unknown flag, so we hand
//! `run_with_args` only the program name (`argv[0]`): GTK sees no options to
//! choke on, and there is no duplicate CLI parsing. Passing GTK's own options
//! (e.g. `--display`) on our command line is therefore deliberately
//! unsupported — the command line belongs to clap.

use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, gio, glib};

use super::window;

/// The fixed GApplication ID that identifies Settings4000 on the session bus.
///
/// It must be a valid reverse-DNS application ID (see
/// [`gio::Application::id_is_valid`]) and, above all, *stable*: single-instance
/// activation (R8.4) works by every launch registering this exact string, and
/// task 8.2's `.desktop` file must carry the same value for the window and icon
/// to associate correctly under Wayland. Settings4000 has no owned internet
/// domain, so this is an arbitrary-but-fixed identifier rooted at the project
/// name; it can be re-based on a real domain later with no behavioral change.
const APP_ID: &str = "org.settings4000.Settings4000";

/// Builds the application, registers it for single-instance behavior, and runs
/// it, returning the process exit code.
///
/// See the module documentation for how registration selects the primary vs.
/// remote role (R8.4) and why only the program name is forwarded to GTK. A
/// remote launch returns [`glib::ExitCode::SUCCESS`] after asking the primary to
/// present its window.
pub(crate) fn run() -> glib::ExitCode {
    let app = Application::builder()
        .application_id(APP_ID)
        // The default flags are exactly what we want: a single primary instance
        // with `activate` semantics. We deliberately do not set `NON_UNIQUE`
        // (which would allow multiple instances, defeating R8.4) or
        // `HANDLES_COMMAND_LINE` (the command line belongs to clap, and we
        // forward no arguments to GTK — see the module docs).
        .flags(gio::ApplicationFlags::empty())
        .build();

    app.connect_activate(on_activate);

    // Register explicitly so the single-instance decision (R8.4, architecture
    // §8) is legible here and a failure to reach the session bus is reported
    // clearly, rather than surfacing later as an opaque `run` failure.
    if let Err(error) = app.register(gio::Cancellable::NONE) {
        tracing::error!(error = %error, "failed to register the GApplication instance");
        return glib::ExitCode::FAILURE;
    }

    if app.is_remote() {
        tracing::info!(
            app_id = APP_ID,
            "another Settings4000 instance is already running; activating it and exiting (R8.4)"
        );
    } else {
        tracing::debug!(app_id = APP_ID, "registered as the primary instance");
    }

    // Forward only argv[0]: clap has already consumed our real arguments and GTK
    // must not try to parse them. `run_with_args` re-runs registration (a no-op
    // now) and then drives the appropriate role — the main loop for the primary,
    // or forward-activation-and-exit-0 for a remote launch.
    let argv0 = std::env::args()
        .next()
        .unwrap_or_else(|| "settings4000".to_owned());
    app.run_with_args(&[argv0])
}

/// Handles the `activate` signal: presents the existing window if there is one,
/// otherwise builds the initial window.
///
/// On the primary instance this fires once at startup (no window yet, so a
/// window is built) and again every time a later launch forwards an activation
/// (a window exists, so it is presented) — exactly the single-instance focus
/// behavior required by R8.4.
fn on_activate(app: &Application) {
    if let Some(window) = app.active_window() {
        tracing::debug!("activation received; presenting the existing window");
        window.present();
    } else {
        tracing::debug!("first activation; building the main window");
        build_main_window(app).present();
    }
}

/// Builds the top-level [`ApplicationWindow`] with its sidebar-plus-stack content
/// (task 5.1/5.4).
///
/// Returns immediately: [`window::build`] shows a loading placeholder and runs
/// installed-app detection (task 4.3) plus config parsing on a worker thread, then
/// populates the store and builds the sidebar/pages on the main thread when that
/// completes (architecture §8). Doing the slow work off-thread — rather than
/// synchronously here — is what keeps cold start inside the <500 ms budget (R8.1); a
/// missing tool or unreadable config never blocks startup (R4.3).
fn build_main_window(app: &Application) -> ApplicationWindow {
    window::build(app)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_id_is_a_valid_gapplication_id() {
        // Single-instance activation (R8.4) depends on registering this exact ID
        // on the session bus; an invalid ID would make registration fail at
        // runtime. Guard the constant here so such a typo is caught headlessly:
        // `id_is_valid` is a pure string check that needs neither a display nor
        // GTK initialization.
        assert!(
            gio::Application::id_is_valid(APP_ID),
            "APP_ID `{APP_ID}` is not a valid GApplication ID"
        );
    }
}
