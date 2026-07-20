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

use std::cell::Cell;
use std::rc::Rc;
use std::time::Instant;

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
///
/// `process_started` is the timestamp `main` captured as its very first
/// statement; it feeds the one-shot startup benchmark mark logged when the
/// window's first frame is painted (task 7.3, R8.1 — see
/// [`arm_first_frame_mark`] and `docs/benchmarks.md`).
pub fn run(process_started: Instant) -> glib::ExitCode {
    let app = Application::builder()
        .application_id(APP_ID)
        // The default flags are exactly what we want: a single primary instance
        // with `activate` semantics. We deliberately do not set `NON_UNIQUE`
        // (which would allow multiple instances, defeating R8.4) or
        // `HANDLES_COMMAND_LINE` (the command line belongs to clap, and we
        // forward no arguments to GTK — see the module docs).
        .flags(gio::ApplicationFlags::empty())
        .build();

    // The timestamp is handed to the activate handler through a take-once cell:
    // only the very first activation can receive it, so the benchmark mark is
    // armed at most once per process even if a future lifecycle change (e.g. a
    // hide-on-close/tray mode) ever rebuilds the window on a later activation —
    // re-arming then would log a nonsense `startup_ms`.
    let process_started = Cell::new(Some(process_started));
    app.connect_activate(move |app| on_activate(app, process_started.take()));

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
/// behavior required by R8.4. Only the first build arms the startup benchmark
/// mark: a re-activation presents a window that was painted long ago, so timing
/// it against process start would be meaningless. `process_started` is `Some`
/// only on the very first activation (the caller passes it through a take-once
/// cell — see [`run`]), so a window built on any later activation finds nothing
/// to arm.
fn on_activate(app: &Application, process_started: Option<Instant>) {
    if let Some(window) = app.active_window() {
        tracing::debug!("activation received; presenting the existing window");
        window.present();
    } else {
        tracing::debug!("first activation; building the main window");
        let window = build_main_window(app);
        if let Some(process_started) = process_started {
            arm_first_frame_mark(&window, process_started);
        }
        window.present();
    }
}

/// Arms the one-shot startup benchmark mark (task 7.3, R8.1): logs
/// `startup_ms` at `info` when GTK finishes painting `window`'s first frame.
///
/// # Why "first frame" is the GDK frame clock's `after-paint`
///
/// The R8.1 budget is "cold startup to interactive window", and the closest
/// point to "the window is on screen" that an app can observe through GTK is
/// the frame clock's `after-paint` phase of the window's first frame cycle: at
/// that moment GTK has laid out, rendered, and handed the finished frame to the
/// compositor. The only later step — the compositor actually scanning the
/// buffer out — happens outside the process and is not *synchronously*
/// observable (GDK does expose the compositor's presentation timestamp
/// post hoc, via `GdkFrameTimings`'s presentation time, but only well after
/// the frame was shown — useless as a live mark), so `after-paint` is the most
/// honest approximation available. (A `map` signal or the first tick would
/// fire earlier, *before* the frame is rendered, and would flatter the
/// number.)
///
/// Note the first frame shows the window shell with its loading placeholder —
/// deliberately so: detection and config parsing run on a worker thread
/// (architecture §8), and that immediately-mapped shell *is* the interactive
/// window the budget targets. The moment the pages fill in can be read from
/// the adjacent "startup load complete" log line's own journald timestamp.
///
/// # Mechanism and overhead
///
/// The tick callback fires at the start of the window's first frame cycle
/// (the frame clock only runs once the window is mapped) and immediately
/// unregisters itself ([`glib::ControlFlow::Break`]); within it, a handler is
/// connected to that same cycle's `after-paint`, which logs the mark and
/// disconnects itself. Nothing remains connected afterwards, so steady-state
/// overhead is zero (task 7.3's instrumentation constraint).
fn arm_first_frame_mark(window: &ApplicationWindow, process_started: Instant) {
    window.add_tick_callback(move |_window, frame_clock| {
        // The handler id is shared into the closure so the handler can
        // disconnect itself after the first `after-paint` (disconnecting a
        // handler from within its own emission is supported by GLib). A `Cell`
        // suffices: everything here runs on the GTK main thread and `take` is
        // the only access.
        let handler: Rc<Cell<Option<glib::SignalHandlerId>>> = Rc::new(Cell::new(None));
        let armed = Rc::clone(&handler);
        let id = frame_clock.connect_after_paint(move |frame_clock| {
            // Milliseconds since `main`'s first statement. The u128→u64
            // conversion cannot overflow in practice (that would be a
            // 500-million-year startup); saturate rather than unwrap so this
            // diagnostic can never panic (no `unwrap` on a fallible path).
            let startup_ms =
                u64::try_from(process_started.elapsed().as_millis()).unwrap_or(u64::MAX);
            tracing::info!(
                startup_ms,
                "first frame painted {startup_ms} ms after process start \
                 (task 7.3 startup mark; R8.1 budget: 500 ms)"
            );
            if let Some(id) = armed.take() {
                frame_clock.disconnect(id);
            }
        });
        handler.set(Some(id));
        glib::ControlFlow::Break
    });
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
