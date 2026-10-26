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

use super::about::{self, AboutInfo};
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
pub(super) const APP_ID: &str = "org.settings4000.Settings4000";

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
        install_about_action(app, &window);
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

/// Registers the `app.about` action the header bar's menu item triggers (task 10.2,
/// R2.5), opening the About window over `window`.
///
/// It is registered here, alongside the one main window it parents, rather than in
/// [`run`]: only the primary instance ever builds that window, and this runs exactly
/// once per process (the first activation), so the action can neither be registered
/// twice nor exist without a window to attach the About window to. Registration does
/// not touch the single-instance protocol (R8.4) — that is decided by
/// [`gio::prelude::ApplicationExt::register`] before any activation.
///
/// The window is held weakly, so the action does not keep a closed window alive;
/// once it is gone the action does nothing.
fn install_about_action(app: &Application, window: &ApplicationWindow) {
    let action = gio::SimpleAction::new(about::ABOUT_ACTION, None);
    let window = window.downgrade();
    action.connect_activate(move |_, _| {
        if let Some(window) = window.upgrade() {
            about::show_about(&window, &AboutInfo::from_cargo());
        }
    });
    app.add_action(&action);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_install_artifact_carries_the_application_id() {
        // Task 8.2: on Wayland, GTK sets every window's app_id to `APP_ID`, and
        // compositors/launchers associate that window (and its icon) with the
        // desktop file named `<app-id>.desktop`. The desktop entry's file name,
        // its `StartupWMClass`/`Icon` values, the installed icon's file name,
        // install.sh's own copy of the ID, and the Arch PKGBUILD's copy (task
        // 12.2; neither a shell script nor a PKGBUILD can read the Rust
        // constant, so each hardcodes one) must therefore all equal the
        // GApplication ID — if `APP_ID` is ever changed, this guard fails
        // headlessly until `data/`, `install.sh` and the PKGBUILD move with it.
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let data_dir = manifest_dir.join("data");

        let desktop_path = data_dir.join(format!("{APP_ID}.desktop"));
        let desktop = std::fs::read_to_string(&desktop_path).unwrap_or_else(|error| {
            panic!(
                "the desktop entry must live at {} (freedesktop convention: \
                 named after the application ID): {error}",
                desktop_path.display()
            )
        });
        for key in ["StartupWMClass", "Icon"] {
            let expected = format!("{key}={APP_ID}");
            assert!(
                desktop.lines().any(|line| line == expected),
                "{} must contain the line `{expected}`",
                desktop_path.display()
            );
        }

        let icon_path = data_dir.join(format!("{APP_ID}.svg"));
        assert!(
            icon_path.is_file(),
            "the icon must live at {} so GTK's by-app-id icon lookup finds it",
            icon_path.display()
        );

        // install.sh derives every install path from its `app_id` variable, so
        // pin that assignment line to the constant to complete the guard.
        let installer_path = manifest_dir.join("install.sh");
        let installer = std::fs::read_to_string(&installer_path).unwrap_or_else(|error| {
            panic!(
                "the installer must live at {}: {error}",
                installer_path.display()
            )
        });
        let expected = format!("app_id=\"{APP_ID}\"");
        assert!(
            installer.lines().any(|line| line == expected),
            "{} must contain the line `{expected}`",
            installer_path.display()
        );

        // Both PKGBUILDs (tasks 12.2 and 12.5) name the installed desktop entry
        // and icon after their `_app_id` variable in the same way.
        let expected = format!("_app_id={APP_ID}");
        for package in [VCS_PACKAGE, RELEASE_PACKAGE] {
            let (pkgbuild_path, pkgbuild) = read_arch_pkgbuild(package);
            assert!(
                pkgbuild.lines().any(|line| line == expected),
                "{} must contain the line `{expected}`",
                pkgbuild_path.display()
            );
        }
    }

    /// The Arch VCS package (task 12.2), which builds the latest commit; also
    /// the name of its directory under `packaging/arch/`.
    const VCS_PACKAGE: &str = "settings4000-git";

    /// The Arch release package (task 12.5), which builds the git tag of the
    /// version in Cargo.toml; also the name of its directory under
    /// `packaging/arch/`.
    const RELEASE_PACKAGE: &str = "settings4000";

    /// Reads the `PKGBUILD` of the Arch package `package` from
    /// `packaging/arch/<package>/`, returning its path for failure messages
    /// together with its text.
    fn read_arch_pkgbuild(package: &str) -> (std::path::PathBuf, String) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("packaging/arch")
            .join(package)
            .join("PKGBUILD");
        let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!("the Arch PKGBUILD must live at {}: {error}", path.display())
        });
        (path, text)
    }

    /// The lines strictly between the line `opener` and the next line equal to
    /// `closer` in a `PKGBUILD`, verbatim.
    ///
    /// Both PKGBUILDs are written so that every multi-line block can be found
    /// this way: an array is opened by `name=(` and closed by a `)` line, and a
    /// function is opened by `name() {` and closed by a `}` at column 0 (the
    /// bodies are indented, so an inner `}` never matches). The opener must
    /// appear exactly once: a second definition of a function or array would
    /// silently replace the first when makepkg sources the file, so checking
    /// the first one would miss the definition that takes effect.
    fn pkgbuild_block<'a>(
        path: &std::path::Path,
        pkgbuild: &'a str,
        opener: &str,
        closer: &str,
    ) -> Vec<&'a str> {
        let path = path.display();
        let openers = pkgbuild.lines().filter(|line| *line == opener).count();
        assert_eq!(
            openers, 1,
            "{path} must contain the line `{opener}` exactly once"
        );
        let mut body = Vec::new();
        for line in pkgbuild.lines().skip_while(|line| *line != opener).skip(1) {
            if line == closer {
                return body;
            }
            body.push(line);
        }
        panic!("{path}: the block opened by `{opener}` is never closed by `{closer}`")
    }

    /// The entries of the one-entry-per-line array `name=(` … `)`, verbatim.
    fn pkgbuild_array<'a>(path: &std::path::Path, pkgbuild: &'a str, name: &str) -> Vec<&'a str> {
        pkgbuild_block(path, pkgbuild, &format!("{name}=("), ")")
    }

    /// The body of the function `name() {` … `}`, verbatim.
    fn pkgbuild_function<'a>(
        path: &std::path::Path,
        pkgbuild: &'a str,
        name: &str,
    ) -> Vec<&'a str> {
        pkgbuild_block(path, pkgbuild, &format!("{name}() {{"), "}")
    }

    /// The lines of a `PKGBUILD` that assign the variable `name`, such as
    /// `pkgver=0` for `pkgver`.
    fn pkgbuild_assignments<'a>(pkgbuild: &'a str, name: &str) -> Vec<&'a str> {
        let prefix = format!("{name}=");
        pkgbuild
            .lines()
            .filter(|line| line.starts_with(&prefix))
            .collect()
    }

    /// Asserts that a `PKGBUILD` carries the project URL and the license from
    /// the Cargo metadata. A `PKGBUILD` is a shell file that cannot read
    /// Cargo.toml, so it keeps its own copies; this keeps them equal.
    fn assert_pkgbuild_follows_cargo_metadata(path: &std::path::Path, pkgbuild: &str) {
        let path = path.display();
        let expected = format!("url='{}'", env!("CARGO_PKG_REPOSITORY"));
        assert!(
            pkgbuild.lines().any(|line| line == expected),
            "{path} must contain the line `{expected}` (Cargo.toml `repository`)"
        );
        let expected = format!("license=({})", env!("CARGO_PKG_LICENSE"));
        assert!(
            pkgbuild.lines().any(|line| line == expected),
            "{path} must contain the line `{expected}` (Cargo.toml `license`)"
        );
    }

    /// The Arch package providing a detected tool, or `None` for a tool that
    /// every system able to install the package already has, so the `PKGBUILD`
    /// need not list it in `optdepends`.
    ///
    /// An exhaustive `match` rather than a lookup table, so adding a
    /// [`Binary`](crate::core::detect::Binary) variant fails to compile until its
    /// package is decided here (and, through the test below, in the `PKGBUILD`).
    fn arch_package_of(binary: crate::core::detect::Binary) -> Option<&'static str> {
        use crate::core::detect::Binary;
        match binary {
            Binary::Hyprctl => Some("hyprland"),
            // gsettings ships with glib2, a direct dependency of the package.
            Binary::Gsettings => None,
            Binary::Pactl => Some("libpulse"),
            Binary::Wpctl => Some("wireplumber"),
            Binary::Kitty => Some("kitty"),
            Binary::Eww => Some("eww"),
            // busctl ships with systemd, part of every Arch base system.
            Binary::Busctl => None,
            Binary::Hyprpaper => Some("hyprpaper"),
            Binary::Hypridle => Some("hypridle"),
            Binary::Hyprlock => Some("hyprlock"),
            Binary::Nmcli => Some("networkmanager"),
            Binary::NmConnectionEditor => Some("nm-connection-editor"),
            Binary::Dconf => Some("dconf"),
        }
    }

    #[test]
    fn arch_pkgbuild_follows_the_cargo_metadata_and_the_detected_tools() {
        // Task 12.2 (R8.7): the PKGBUILD is a shell file that cannot read the
        // Cargo metadata, so it carries its own copies of the project URL and
        // the license, and its own list of the optional tools the app detects
        // (R4). This guard keeps each copy equal to its source of truth.
        let (path_buf, pkgbuild) = read_arch_pkgbuild(VCS_PACKAGE);
        assert_pkgbuild_follows_cargo_metadata(&path_buf, &pkgbuild);
        let path = path_buf.display();

        // R8.6: Cargo.toml is the single version source. makepkg refuses a
        // PKGBUILD without a non-empty `pkgver=` line, so the file carries the
        // placeholder `0` and a pkgver() function that reads Cargo.toml; makepkg
        // rewrites the placeholder with the computed version on every build, and
        // that rewritten line must not be committed (it would be a second,
        // soon-stale version literal).
        assert_eq!(
            pkgbuild_assignments(&pkgbuild, "pkgver"),
            ["pkgver=0"],
            "{path} must carry only the placeholder `pkgver=0`; if makepkg rewrote \
             it during a local build, discard that change with `git restore`"
        );
        assert!(
            pkgbuild.lines().any(|line| line == "pkgver() {"),
            "{path} must define a pkgver() function"
        );
        let version_source = r#"sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml"#;
        assert!(
            pkgbuild.contains(version_source),
            "{path}'s pkgver() must read the version with `{version_source}`"
        );
        // Comment lines are skipped: prose may mention unrelated numbers (such
        // as a makepkg version) that a short Cargo version would match.
        let version = env!("CARGO_PKG_VERSION");
        if let Some(line) = pkgbuild
            .lines()
            .find(|line| !line.trim_start().starts_with('#') && line.contains(version))
        {
            panic!("{path} must not contain the version literal `{version}`: `{line}`");
        }

        // Every `optdepends` entry is a `'package: reason'` line between
        // `optdepends=(` and the closing `)`.
        let optdepends: Vec<(&str, &str)> = pkgbuild_array(&path_buf, &pkgbuild, "optdepends")
            .into_iter()
            .map(|line| {
                let entry = line.trim().trim_matches('\'');
                entry.split_once(": ").unwrap_or_else(|| {
                    panic!("{path}: optdepends entry `{entry}` must read `package: reason`")
                })
            })
            .collect();
        assert!(
            !optdepends.is_empty(),
            "{path} must declare an `optdepends=(` array, one entry per line"
        );
        for (package, reason) in &optdepends {
            assert!(
                !reason.trim().is_empty(),
                "{path}: optdepends entry `{package}` needs a reason"
            );
        }
        for &binary in crate::core::detect::Binary::ALL {
            let Some(package) = arch_package_of(binary) else {
                continue;
            };
            assert!(
                optdepends.iter().any(|(name, _)| *name == package),
                "{path} must list `{package}` in optdepends: it provides `{}`, \
                 which the app detects at startup",
                binary.command_name()
            );
        }
    }

    #[test]
    fn arch_release_pkgbuild_builds_the_tag_of_the_cargo_version() {
        // Task 12.5 (R8.6, R8.7): the release package builds the annotated git
        // tag `v<version>`, so unlike the -git package it must name its version
        // as a literal. Cargo.toml stays the single version source: this guard
        // fails as soon as the two disagree, so a release commit cannot bump
        // one without the other (docs/releasing.md lists the steps).
        let (path_buf, pkgbuild) = read_arch_pkgbuild(RELEASE_PACKAGE);
        assert_pkgbuild_follows_cargo_metadata(&path_buf, &pkgbuild);
        let path = path_buf.display();

        let version = env!("CARGO_PKG_VERSION");
        assert_eq!(
            pkgbuild_assignments(&pkgbuild, "pkgver"),
            [format!("pkgver={version}")],
            "{path} must carry exactly one `pkgver=` line, equal to the version \
             in Cargo.toml"
        );
        // A pkgver() function would replace the literal with a computed
        // version after the checkout, so the package could build one version
        // and be labelled with another.
        assert!(
            !pkgbuild.lines().any(|line| line.starts_with("pkgver()")),
            "{path} must not define a pkgver() function: the version selects the tag"
        );
        let pkgrel = pkgbuild_assignments(&pkgbuild, "pkgrel");
        assert!(
            matches!(
                pkgrel.as_slice(),
                [line] if line
                    .strip_prefix("pkgrel=")
                    .and_then(|number| number.parse::<u32>().ok())
                    .is_some_and(|number| number >= 1)
            ),
            "{path} must carry exactly one `pkgrel=` line with a positive integer, \
             got {pkgrel:?}"
        );

        // The source is the project's own repository, pinned to the tag that
        // `pkgver` names; makepkg expands `$pkgver` when it reads the array.
        let sources = pkgbuild_assignments(&pkgbuild, "source");
        let [source] = sources.as_slice() else {
            panic!("{path} must carry exactly one `source=` line, got {sources:?}");
        };
        let repository = format!("git+{}.git", env!("CARGO_PKG_REPOSITORY"));
        assert!(
            source.contains(&repository) && source.contains("#tag=v$pkgver\""),
            "{path}: `{source}` must clone `{repository}` at `#tag=v$pkgver`"
        );
    }

    #[test]
    fn arch_release_and_vcs_pkgbuilds_build_and_install_identically() {
        // Task 12.5: the two Arch packages differ only in where the source comes
        // from (a tag versus the latest commit) and in how the version is
        // found. A PKGBUILD cannot source a shared file, so the build steps and
        // the dependency lists are duplicated; this guard keeps the copies
        // byte-identical, so a fix made in one package cannot be forgotten in
        // the other. The source directory differs by necessity and is read
        // through the `_srcdir` variable, which keeps the function bodies equal.
        let (vcs_path, vcs) = read_arch_pkgbuild(VCS_PACKAGE);
        let (release_path, release) = read_arch_pkgbuild(RELEASE_PACKAGE);
        let (vcs_name, release_name) = (vcs_path.display(), release_path.display());

        // The function bodies only stay equivalent while each file defines the
        // clone directory once and clones its source into it: with the
        // `_srcdir=` line deleted, `cd "$_srcdir"` would still compare equal
        // here but, as `cd ""`, stay in makepkg's `$srcdir` at build time.
        for (path, pkgbuild) in [(&vcs_path, &vcs), (&release_path, &release)] {
            let path = path.display();
            let srcdirs = pkgbuild_assignments(pkgbuild, "_srcdir");
            assert_eq!(
                srcdirs.len(),
                1,
                "{path} must assign `_srcdir=` exactly once, got {srcdirs:?}"
            );
            let sources = pkgbuild_assignments(pkgbuild, "source");
            assert!(
                matches!(sources.as_slice(), [source] if source.starts_with("source=(\"$_srcdir::")),
                "{path} must carry one `source=` line cloning into `$_srcdir`, got {sources:?}"
            );
        }
        for name in ["pkgdesc", "arch", "_app_id"] {
            assert_eq!(
                pkgbuild_assignments(&release, name),
                pkgbuild_assignments(&vcs, name),
                "`{name}=` must be identical in {release_name} and {vcs_name}"
            );
        }
        for name in ["depends", "makedepends", "optdepends"] {
            assert_eq!(
                pkgbuild_array(&release_path, &release, name),
                pkgbuild_array(&vcs_path, &vcs, name),
                "the `{name}` array must be identical in {release_name} and {vcs_name}"
            );
        }
        for name in ["prepare", "build", "check", "package"] {
            assert_eq!(
                pkgbuild_function(&release_path, &release, name),
                pkgbuild_function(&vcs_path, &vcs, name),
                "the body of `{name}()` must be identical in {release_name} and {vcs_name}"
            );
        }
    }

    #[test]
    fn crate_metadata_carries_the_about_window_fields() {
        // Task 10.1 (R8.6, R2.5): the About window and the CLI `--version` read
        // the version, author and repository URL from Cargo's compile-time
        // package metadata rather than from literals. Cargo silently expands a
        // missing `authors`/`repository` key to an empty string, so without this
        // guard deleting either key from Cargo.toml would still compile and only
        // show up as a blank field in the About window.
        let authors = env!("CARGO_PKG_AUTHORS");
        assert!(
            !authors.trim().is_empty(),
            "Cargo.toml must declare `authors` for the About window"
        );
        assert!(
            authors.contains("FiBe4000"),
            "`authors` must name the project author's GitHub handle, got `{authors}`"
        );

        let repository = env!("CARGO_PKG_REPOSITORY");
        assert!(
            repository.starts_with("https://github.com/FiBe4000/Settings4000"),
            "`repository` must point at the GitHub project, got `{repository}`"
        );

        // A semantic version is `MAJOR.MINOR.PATCH`, optionally followed by a
        // `-pre-release` and/or `+build` suffix, which this check strips. 1.0.0
        // is the feature-complete release (R8.6), so the major part may never
        // drop back below 1. Cargo has already validated the string as semver
        // before this test can run, so this only pins the shape and the major; it
        // does not re-check leading zeros or the suffix grammar.
        let version = env!("CARGO_PKG_VERSION");
        let core = version.split(['-', '+']).next().unwrap_or_default();
        let parts = core
            .split('.')
            .map(str::parse::<u64>)
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_else(|error| {
                panic!("CARGO_PKG_VERSION `{version}` is not a semantic version: {error}")
            });
        assert_eq!(
            parts.len(),
            3,
            "CARGO_PKG_VERSION `{version}` must have exactly MAJOR.MINOR.PATCH"
        );
        assert!(
            parts[0] >= 1,
            "CARGO_PKG_VERSION `{version}` must be at least 1.0.0"
        );
    }

    #[test]
    fn crate_declares_the_mit_license_and_ships_its_text() {
        // Task 12.1 (R8.7): the project is MIT-licensed. A distribution package
        // must declare its license, and Arch's `licenses` package does not carry
        // the MIT text, so the Arch `PKGBUILD` (task 12.2) both declares the SPDX
        // identifier from Cargo.toml and installs the `LICENSE` file from the
        // repository root. Cargo silently expands a missing `license` key to an
        // empty string, so without this guard deleting the key, or the file, would
        // still compile and only surface as a package without a license.
        assert_eq!(
            env!("CARGO_PKG_LICENSE"),
            "MIT",
            "Cargo.toml must declare `license = \"MIT\"`"
        );

        let license_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("LICENSE");
        let license = std::fs::read_to_string(&license_path).unwrap_or_else(|error| {
            panic!(
                "the license text must live at {}: {error}",
                license_path.display()
            )
        });
        // The SPDX "MIT" text opens with this title line; the copyright holder is
        // the GitHub handle, the same one `authors` carries, never a real name.
        for expected in ["MIT License", "Copyright (c) 2026 FiBe4000"] {
            assert!(
                license.lines().any(|line| line.trim() == expected),
                "{} must contain the line `{expected}`",
                license_path.display()
            );
        }
    }

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
