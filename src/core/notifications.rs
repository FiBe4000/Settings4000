//! GTK-free Notifications-page model: the runtime-only controls of the notifd
//! notification daemon (task 11.7; architecture §3, §7; analysis §7.5, §8.2; R4.2,
//! R5.2, R6.1, R6.2, R7.3).
//!
//! # What this module is
//!
//! The Notifications page drives the dotfiles' notification daemon, notifd, over
//! D-Bus: it shows the do-not-disturb state, the number of notifications in the
//! history and whether an application is holding popups back, and it offers three
//! actions — switch do-not-disturb, open the notification center, clear every
//! notification. All of these are **runtime-only** (R5.2): they change live daemon
//! state, touch no file, are applied the moment the user acts, and are never staged,
//! dirty or part of an Apply. (notifd's file-backed settings in
//! `~/.config/notifd/config` are the page's separate, staged half — task 11.15,
//! [`crate::core::notifd_config`].)
//!
//! [`NotificationsModel`] holds what the page last read and the one piece of
//! interaction state the page has — a pending "clear all" awaiting confirmation —
//! and runs every call through the [`CommandRunner`] seam using the commands of
//! [`crate::core::notifd`]. It stays GTK-free so the whole behaviour, including the
//! confirmation step, is unit-tested headlessly against a mock runner (R6.2); the
//! layering guard in `tests/module_boundaries.rs` forbids any `gtk`/`relm4` import.
//!
//! # Reading the state
//!
//! [`NotificationsModel::refresh`] is called from the window's deferred page-entry
//! hook (the task 9.26 rule: nothing is spawned while the window builds its pages,
//! so a wedged bus can never delay startup, R8.1). It asks, in order: is the daemon
//! running (`NameHasOwner`), which version is it (`GetServerInformation`), and then
//! the state itself — `GetState()` on a daemon ≥ 1.1, or, on an older daemon that
//! lacks it, the state file `$XDG_RUNTIME_DIR/notifd/bar.json`. The daemon is
//! re-probed on every entry rather than trusting the startup detection, because it
//! may have started (it is D-Bus activated by the first notification), stopped or
//! been upgraded since.
//!
//! # "Clear all" needs a confirmation
//!
//! Clearing discards every notification in the history and cannot be undone, so it
//! is a two-step operation: [`NotificationsModel::request_clear`] only marks the
//! request pending (the UI then asks the user), and [`NotificationsModel::confirm_clear`]
//! is the only path that issues `Clear()` — and it refuses unless a request is
//! pending. A cancelled dialog calls [`NotificationsModel::cancel_clear`].

use std::ffi::OsStr;
use std::path::PathBuf;

use crate::core::apply::stderr_excerpt;
use crate::core::notifd::{
    self, NotifdMethod, NotifdState, NotifdVersion, capture_stdout, control_command,
    name_has_owner_command, server_information_command,
};
use crate::system::command::{Command, CommandError, CommandRunner};

/// The state file notifd ≥ 1.0 rewrites with the four state fields, relative to
/// `$XDG_RUNTIME_DIR`. It holds no message text (unlike its neighbours `center.json`
/// and `popup<k>.json`, which the app never opens).
const BAR_JSON_RELATIVE: &[&str] = &["notifd", "bar.json"];

/// What the page knows about the daemon right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaemonView {
    /// The page has not been entered yet, so nothing has been asked. The page shows
    /// a "reading…" placeholder.
    NotQueried,
    /// notifd does not own its bus name. It is installed (the page is only offered
    /// for an installed or running daemon) and starts with the next notification an
    /// application sends; until then no control can reach it.
    NotRunning,
    /// notifd is running.
    Running {
        /// The version it reported, or `None` when it did not report one (treated
        /// as older than 1.1).
        version: Option<NotifdVersion>,
        /// Its current state, or `None` when the state query failed or its reply
        /// was malformed — the page then shows the values as unknown.
        state: Option<NotifdState>,
    },
}

/// Why a Notifications-page action did not happen.
#[derive(Debug)]
pub enum NotifdActionError {
    /// The last read found the daemon not running, so no call was attempted (a
    /// call with auto-start disabled would only fail).
    NotRunning,
    /// [`NotificationsModel::confirm_clear`] was called without a pending
    /// [`NotificationsModel::request_clear`]; nothing was cleared.
    NotRequested,
    /// The `busctl` call could not be run (not found, timed out).
    Command(CommandError),
    /// The call ran and failed — e.g. the daemon exited in the meantime, or (for an
    /// unknown method) is too old. Carries the exit code and a bounded stderr
    /// excerpt for the log and the page.
    Failed {
        /// The exit code, or `None` when the process was terminated by a signal.
        code: Option<i32>,
        /// The start of the captured stderr, bounded and marked exactly like the
        /// Apply pipeline's excerpts (`core::apply::stderr_excerpt`: at most
        /// [`STDERR_EXCERPT_LIMIT`](crate::system::command::STDERR_EXCERPT_LIMIT) bytes,
        /// decoded lossily, `…` when content was cut), or `None` when the call wrote
        /// nothing to stderr.
        stderr: Option<String>,
    },
}

impl std::fmt::Display for NotifdActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotifdActionError::NotRunning => f.write_str("the notification daemon is not running"),
            NotifdActionError::NotRequested => {
                f.write_str("clearing the notifications was not requested")
            }
            NotifdActionError::Command(error) => write!(f, "could not run busctl: {error}"),
            NotifdActionError::Failed { code, stderr } => {
                match code {
                    Some(code) => {
                        write!(f, "the notification daemon call exited with status {code}")?
                    }
                    None => {
                        f.write_str("the notification daemon call was terminated by a signal")?
                    }
                }
                match stderr {
                    Some(stderr) => write!(f, ": {stderr}"),
                    None => Ok(()),
                }
            }
        }
    }
}

impl std::error::Error for NotifdActionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            NotifdActionError::Command(error) => Some(error),
            _ => None,
        }
    }
}

/// The Notifications page's model (see the module docs).
///
/// Built by the window whenever it builds the page, which it does only when
/// detection reported notifd running or installed
/// ([`NotifdStatus::is_present`](crate::core::notifd::NotifdStatus::is_present));
/// otherwise the whole category is hidden (R4.2).
#[derive(Debug)]
pub struct NotificationsModel {
    /// Where an older daemon's `bar.json` is read from, or `None` when
    /// `$XDG_RUNTIME_DIR` is unset (the pre-1.1 state is then unknown).
    bar_json: Option<PathBuf>,
    /// What the last [`Self::refresh`] (or action) found.
    view: DaemonView,
    /// Whether a "clear all" was requested and awaits confirmation.
    clear_pending: bool,
}

impl NotificationsModel {
    /// A model that has not queried anything yet. `bar_json` is the pre-1.1 state
    /// file location — [`Self::bar_json_path`] in production, a temp path in tests.
    pub fn new(bar_json: Option<PathBuf>) -> NotificationsModel {
        NotificationsModel {
            bar_json,
            view: DaemonView::NotQueried,
            clear_pending: false,
        }
    }

    /// `<runtime_dir>/notifd/bar.json`, or `None` when `runtime_dir` (the value of
    /// `$XDG_RUNTIME_DIR`, which the caller reads) is unset or empty.
    ///
    /// notifd itself honours a `NOTIFD_RUNTIME_DIR` override, but that is a testing
    /// aid read from the daemon's own environment, which the app cannot see; the
    /// systemd unit never sets it.
    pub fn bar_json_path(runtime_dir: Option<&OsStr>) -> Option<PathBuf> {
        let runtime_dir = runtime_dir.filter(|dir| !dir.is_empty())?;
        Some(
            BAR_JSON_RELATIVE
                .iter()
                .fold(PathBuf::from(runtime_dir), |path, part| path.join(part)),
        )
    }

    /// What the page currently knows about the daemon.
    pub fn view(&self) -> DaemonView {
        self.view
    }

    /// Whether the controls can act: the last read found the daemon running.
    pub fn is_running(&self) -> bool {
        matches!(self.view, DaemonView::Running { .. })
    }

    /// Re-reads whether notifd runs, its version and its state (see the module docs
    /// for the order), and returns the new view.
    ///
    /// Issues `NameHasOwner`, then — only when the name is owned —
    /// `GetServerInformation`, then `GetState` when the version is ≥ 1.1. Every
    /// failure degrades (to [`DaemonView::NotRunning`] when the bus cannot confirm an
    /// owner, to an unknown state otherwise) and is logged at `info`; this never
    /// fails and never starts the daemon. A refresh also drops a pending "clear all"
    /// request, since what the user was asked to confirm may no longer be what is on
    /// screen.
    pub fn refresh(&mut self, runner: &dyn CommandRunner) -> DaemonView {
        self.clear_pending = false;
        let owned = capture_stdout(runner, &name_has_owner_command(), true)
            .as_deref()
            .and_then(notifd::parse_name_has_owner);
        if owned != Some(true) {
            tracing::info!(
                owned = ?owned,
                "notifd is not running; the Notifications controls wait for it to start \
                 (it is activated by the next notification)"
            );
            self.view = DaemonView::NotRunning;
            return self.view;
        }

        let version = capture_stdout(runner, &server_information_command(), true)
            .as_deref()
            .and_then(notifd::parse_notifd_version);
        let state = match version {
            Some(version) if version.supports_state_and_reload() => self.state_from_dbus(runner),
            _ => self.state_from_bar_json(),
        };
        // Per page entry, so `debug`: R7.3 keeps `info` for detection, writes and
        // reloads.
        tracing::debug!(
            version = %version.map_or_else(|| "unknown".to_string(), |v| v.to_string()),
            state_known = state.is_some(),
            "read the notifd state (runtime-only, R5.2)"
        );
        tracing::debug!(state = ?state, "notifd state");
        self.view = DaemonView::Running { version, state };
        self.view
    }

    /// The state from `GetState()` (daemon ≥ 1.1).
    fn state_from_dbus(&self, runner: &dyn CommandRunner) -> Option<NotifdState> {
        let state = capture_stdout(runner, &control_command(NotifdMethod::GetState), true)
            .as_deref()
            .and_then(notifd::parse_get_state);
        if state.is_none() {
            tracing::info!("notifd GetState failed or its reply was not `bubb …`; state unknown");
        }
        state
    }

    /// The state from `bar.json` (daemon older than 1.1, which has no `GetState`).
    /// Only that file is read — never the message-bearing files beside it.
    fn state_from_bar_json(&self) -> Option<NotifdState> {
        let Some(path) = &self.bar_json else {
            tracing::info!("XDG_RUNTIME_DIR is unset; the state of a pre-1.1 notifd is unknown");
            return None;
        };
        match std::fs::read(path) {
            Ok(bytes) => {
                let state = notifd::parse_bar_json(&bytes);
                if state.is_none() {
                    tracing::info!(
                        path = %path.display(),
                        "notifd bar.json does not hold the expected state fields; state unknown"
                    );
                }
                state
            }
            Err(error) => {
                tracing::info!(
                    path = %path.display(),
                    %error,
                    "could not read notifd bar.json; state unknown"
                );
                None
            }
        }
    }

    /// Switches do-not-disturb on or off immediately (R5.2) with `SetDnd s on|off`.
    ///
    /// On success the shown state takes the daemon's reply (`b <new state>`) when it
    /// parses, else the requested value. Nothing is staged or written. Refused
    /// without a call when the daemon was last seen not running.
    pub fn set_dnd(
        &mut self,
        runner: &dyn CommandRunner,
        enabled: bool,
    ) -> Result<(), NotifdActionError> {
        let reply = self.call(runner, NotifdMethod::SetDnd(enabled))?;
        let dnd = notifd::parse_bool_reply(&reply).unwrap_or(enabled);
        self.update_state(|state| state.dnd = dnd);
        Ok(())
    }

    /// Opens the notification center immediately with `SetCenter s open`. Refused
    /// without a call when the daemon was last seen not running.
    pub fn open_center(&mut self, runner: &dyn CommandRunner) -> Result<(), NotifdActionError> {
        let reply = self.call(runner, NotifdMethod::OpenCenter)?;
        let open = notifd::parse_bool_reply(&reply).unwrap_or(true);
        self.update_state(|state| state.center_open = open);
        Ok(())
    }

    /// Marks a "clear all" as requested; the UI must now ask the user and answer
    /// with [`Self::confirm_clear`] or [`Self::cancel_clear`]. Issues nothing.
    pub fn request_clear(&mut self) {
        self.clear_pending = true;
    }

    /// Drops a pending "clear all" request (the user cancelled). Issues nothing.
    pub fn cancel_clear(&mut self) {
        self.clear_pending = false;
    }

    /// Whether a "clear all" request awaits the user's confirmation.
    pub fn is_clear_pending(&self) -> bool {
        self.clear_pending
    }

    /// Clears every notification with `Clear()` — only after [`Self::request_clear`].
    ///
    /// Without a pending request this returns [`NotifdActionError::NotRequested`] and
    /// issues nothing; the request is consumed either way. The shown state is not
    /// guessed afterwards: the UI re-reads it with [`Self::refresh`].
    pub fn confirm_clear(&mut self, runner: &dyn CommandRunner) -> Result<(), NotifdActionError> {
        if !std::mem::replace(&mut self.clear_pending, false) {
            tracing::warn!("a notification clear was confirmed without being requested; ignored");
            return Err(NotifdActionError::NotRequested);
        }
        self.call(runner, NotifdMethod::Clear).map(drop)
    }

    /// Runs one control call, logging it and its outcome (R7.3), and returns its
    /// stdout.
    fn call(
        &self,
        runner: &dyn CommandRunner,
        method: NotifdMethod,
    ) -> Result<String, NotifdActionError> {
        if !self.is_running() {
            tracing::info!(?method, "notifd is not running; the action was not sent");
            return Err(NotifdActionError::NotRunning);
        }
        let command: Command = control_command(method);
        match runner.run(&command) {
            Ok(output) if output.success() => {
                tracing::info!(%command, "notifd call succeeded (runtime-only, R5.2)");
                Ok(String::from_utf8_lossy(output.stdout()).into_owned())
            }
            Ok(output) => {
                let stderr = stderr_excerpt(output.stderr());
                tracing::error!(%command, code = ?output.code(), stderr = ?stderr, "notifd call failed");
                Err(NotifdActionError::Failed {
                    code: output.code(),
                    stderr,
                })
            }
            Err(error) => {
                tracing::error!(%command, %error, "could not run the notifd call");
                Err(NotifdActionError::Command(error))
            }
        }
    }

    /// Applies `change` to the known state, if any.
    fn update_state(&mut self, change: impl FnOnce(&mut NotifdState)) {
        if let DaemonView::Running {
            state: Some(state), ..
        } = &mut self.view
        {
            change(state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::notifd::unit_is_active_command;
    use crate::system::command::{CommandOutput, MockCommandRunner};

    const OWNED: &str = "b true\n";
    const INFO_1_1: &str = "ssss \"notifd\" \"dotfiles\" \"1.1\" \"1.3\"\n";
    const INFO_1_0: &str = "ssss \"notifd\" \"dotfiles\" \"1.0\" \"1.3\"\n";
    const STATE: &str = "bubb false 22 false false\n";

    fn ok(stdout: &str) -> Result<CommandOutput, CommandError> {
        Ok(CommandOutput::fake_with_streams(0, stdout, ""))
    }

    /// A model that already saw a running 1.1 daemon with [`STATE`].
    fn running_model() -> NotificationsModel {
        let mut model = NotificationsModel::new(None);
        let runner = MockCommandRunner::with_outcomes([ok(OWNED), ok(INFO_1_1), ok(STATE)]);
        model.refresh(&runner);
        model
    }

    #[test]
    fn refresh_on_a_1_1_daemon_reads_get_state() {
        let mut model = NotificationsModel::new(None);
        assert_eq!(model.view(), DaemonView::NotQueried);
        let runner = MockCommandRunner::with_outcomes([ok(OWNED), ok(INFO_1_1), ok(STATE)]);
        let view = model.refresh(&runner);
        assert_eq!(
            view,
            DaemonView::Running {
                version: Some(NotifdVersion::STATE_AND_RELOAD),
                state: Some(NotifdState {
                    dnd: false,
                    count: 22,
                    inhibited: false,
                    center_open: false
                }),
            }
        );
        assert_eq!(
            runner.recorded(),
            vec![
                name_has_owner_command(),
                server_information_command(),
                control_command(NotifdMethod::GetState),
            ]
        );
    }

    #[test]
    fn a_malformed_get_state_reply_leaves_the_state_unknown() {
        let mut model = NotificationsModel::new(None);
        let runner = MockCommandRunner::with_outcomes([
            ok(OWNED),
            ok(INFO_1_1),
            ok("bubb false twenty false false\n"),
        ]);
        assert_eq!(
            model.refresh(&runner),
            DaemonView::Running {
                version: Some(NotifdVersion::STATE_AND_RELOAD),
                state: None
            }
        );
        // A failing GetState degrades the same way.
        let failing =
            MockCommandRunner::with_outcomes([ok(OWNED), ok(INFO_1_1), Ok(CommandOutput::fake(1))]);
        assert!(matches!(
            model.refresh(&failing),
            DaemonView::Running { state: None, .. }
        ));
        assert!(
            model.is_running(),
            "an unknown state still leaves the controls usable"
        );
    }

    /// A temp dir holding a `bar.json` with `dnd` and `inhibited` on and four
    /// notifications, next to a message-bearing `center.json` that must never matter.
    fn bar_json_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("bar.json"),
            br#"{"count": 4, "dnd": true, "inhibited": true, "center_open": false}"#,
        )
        .expect("write bar.json");
        std::fs::write(dir.path().join("center.json"), b"{\"groups\":[]}").expect("center");
        dir
    }

    /// The state [`bar_json_dir`]'s `bar.json` describes.
    const BAR_STATE: NotifdState = NotifdState {
        dnd: true,
        count: 4,
        inhibited: true,
        center_open: false,
    };

    #[test]
    fn a_1_0_daemon_reads_its_state_from_bar_json() {
        let dir = bar_json_dir();
        let mut model = NotificationsModel::new(Some(dir.path().join("bar.json")));
        let runner = MockCommandRunner::with_outcomes([ok(OWNED), ok(INFO_1_0)]);
        assert_eq!(
            model.refresh(&runner),
            DaemonView::Running {
                version: NotifdVersion::parse("1.0"),
                state: Some(BAR_STATE),
            }
        );
    }

    #[test]
    fn a_1_0_daemon_is_never_sent_get_state() {
        let dir = bar_json_dir();
        let mut model = NotificationsModel::new(Some(dir.path().join("bar.json")));
        let runner = MockCommandRunner::with_outcomes([ok(OWNED), ok(INFO_1_0)]);
        model.refresh(&runner);
        assert_eq!(
            runner.recorded(),
            vec![name_has_owner_command(), server_information_command()],
            "no GetState on a daemon that lacks it"
        );
    }

    #[test]
    fn an_unversioned_daemon_reads_its_state_from_bar_json() {
        // GetServerInformation failed, so the version is unknown and the conservative
        // pre-1.1 source is used.
        let dir = bar_json_dir();
        let mut model = NotificationsModel::new(Some(dir.path().join("bar.json")));
        let runner = MockCommandRunner::with_outcomes([ok(OWNED), Ok(CommandOutput::fake(1))]);
        assert_eq!(
            model.refresh(&runner),
            DaemonView::Running {
                version: None,
                state: Some(BAR_STATE),
            }
        );
        assert_eq!(runner.recorded().len(), 2, "no GetState either");
    }

    #[test]
    fn a_missing_bar_json_leaves_the_state_unknown() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut model = NotificationsModel::new(Some(dir.path().join("bar.json")));
        let runner = MockCommandRunner::with_outcomes([ok(OWNED), ok(INFO_1_0)]);
        assert_eq!(
            model.refresh(&runner),
            DaemonView::Running {
                version: NotifdVersion::parse("1.0"),
                state: None
            }
        );
    }

    #[test]
    fn an_unowned_bus_name_reads_as_not_running() {
        let mut model = NotificationsModel::new(None);
        let runner = MockCommandRunner::with_outcomes([ok("b false\n")]);
        assert_eq!(model.refresh(&runner), DaemonView::NotRunning);
    }

    #[test]
    fn an_unowned_daemon_is_asked_nothing_more() {
        // Liveness is the bus name; the page never asks systemd or the daemon itself.
        let mut model = NotificationsModel::new(None);
        let runner = MockCommandRunner::with_outcomes([ok("b false\n")]);
        model.refresh(&runner);
        assert_eq!(runner.recorded(), vec![name_has_owner_command()]);
        assert!(!runner.recorded().contains(&unit_is_active_command()));
    }

    #[test]
    fn a_failed_owner_query_reads_as_not_running() {
        let mut model = NotificationsModel::new(None);
        let failing = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake(1))]);
        assert_eq!(model.refresh(&failing), DaemonView::NotRunning);
    }

    #[test]
    fn a_daemon_that_stops_refuses_later_actions() {
        // The page's main failure mode composed: the daemon ran at one page entry,
        // has exited by the next (its name has no owner any more), and the user then
        // flips the switch. The view must say not running, and the toggle must be
        // refused without a call — `--auto-start=no` would only fail, and nothing may
        // start the daemon behind systemd's back.
        let mut model = running_model();
        assert!(model.is_running());

        let stopped = MockCommandRunner::with_outcomes([ok("b false\n")]);
        assert_eq!(model.refresh(&stopped), DaemonView::NotRunning);

        let runner = MockCommandRunner::new();
        assert!(matches!(
            model.set_dnd(&runner, true),
            Err(NotifdActionError::NotRunning)
        ));
        assert!(runner.recorded().is_empty(), "no call to a stopped daemon");
    }

    #[test]
    fn a_failed_call_message_quotes_the_stderr_excerpt() {
        let mut model = running_model();
        let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake_with_streams(
            1,
            "",
            "Call failed: The name is not activatable\n",
        ))]);
        let error = model.set_dnd(&runner, true).expect_err("the call failed");
        assert_eq!(
            error.to_string(),
            "the notification daemon call exited with status 1: Call failed: The name is \
             not activatable"
        );
        let silent = NotifdActionError::Failed {
            code: None,
            stderr: None,
        };
        assert_eq!(
            silent.to_string(),
            "the notification daemon call was terminated by a signal"
        );
    }

    #[test]
    fn toggling_dnd_records_exactly_the_set_dnd_call() {
        let mut model = running_model();
        let runner = MockCommandRunner::with_outcomes([ok("b true\n")]);
        model.set_dnd(&runner, true).expect("SetDnd succeeds");
        assert_eq!(
            runner.recorded(),
            vec![control_command(NotifdMethod::SetDnd(true))]
        );
        assert_eq!(
            runner.recorded()[0].args_slice(),
            [
                "--user",
                "--auto-start=no",
                "call",
                "se.dotfiles.Notifd",
                "/se/dotfiles/Notifd",
                "se.dotfiles.Notifd",
                "SetDnd",
                "s",
                "on"
            ]
        );
        assert!(matches!(
            model.view(),
            DaemonView::Running {
                state: Some(NotifdState { dnd: true, .. }),
                ..
            }
        ));

        let off = MockCommandRunner::with_outcomes([ok("b false\n")]);
        model.set_dnd(&off, false).expect("SetDnd succeeds");
        assert_eq!(
            off.recorded(),
            vec![control_command(NotifdMethod::SetDnd(false))]
        );
        assert!(matches!(
            model.view(),
            DaemonView::Running {
                state: Some(NotifdState { dnd: false, .. }),
                ..
            }
        ));
    }

    #[test]
    fn a_failed_dnd_call_keeps_the_shown_state() {
        // The failure is still returned as an error.
        let mut model = running_model();
        let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake_with_streams(
            1,
            "",
            "Call failed: The name is not activatable",
        ))]);
        let error = model.set_dnd(&runner, true).expect_err("the call failed");
        assert!(matches!(
            error,
            NotifdActionError::Failed { code: Some(1), .. }
        ));
        assert!(matches!(
            model.view(),
            DaemonView::Running {
                state: Some(NotifdState { dnd: false, .. }),
                ..
            }
        ));
    }

    #[test]
    fn open_center_issues_set_center_open() {
        let mut model = running_model();
        let runner = MockCommandRunner::with_outcomes([ok("b true\n")]);
        model.open_center(&runner).expect("SetCenter succeeds");
        assert_eq!(
            runner.recorded(),
            vec![control_command(NotifdMethod::OpenCenter)]
        );
        assert!(matches!(
            model.view(),
            DaemonView::Running {
                state: Some(NotifdState {
                    center_open: true,
                    ..
                }),
                ..
            }
        ));
    }

    #[test]
    fn clear_requires_confirmation() {
        let mut model = running_model();
        let runner = MockCommandRunner::new();

        // Confirming without a request clears nothing.
        assert!(matches!(
            model.confirm_clear(&runner),
            Err(NotifdActionError::NotRequested)
        ));
        assert!(runner.recorded().is_empty());

        // Requesting alone clears nothing either; it only waits for the user.
        model.request_clear();
        assert!(model.is_clear_pending());
        assert!(runner.recorded().is_empty(), "no call before confirmation");

        // A cancelled request is gone.
        model.cancel_clear();
        assert!(!model.is_clear_pending());
        assert!(matches!(
            model.confirm_clear(&runner),
            Err(NotifdActionError::NotRequested)
        ));
        assert!(runner.recorded().is_empty());

        // Request + confirm issues exactly Clear, once, and consumes the request.
        model.request_clear();
        model.confirm_clear(&runner).expect("Clear succeeds");
        assert_eq!(
            runner.recorded(),
            vec![control_command(NotifdMethod::Clear)]
        );
        assert!(!model.is_clear_pending());
        assert!(matches!(
            model.confirm_clear(&runner),
            Err(NotifdActionError::NotRequested)
        ));
        assert_eq!(runner.recorded().len(), 1);
    }

    #[test]
    fn a_refresh_drops_a_pending_clear() {
        let mut model = running_model();
        model.request_clear();
        let runner = MockCommandRunner::with_outcomes([ok(OWNED), ok(INFO_1_1), ok(STATE)]);
        model.refresh(&runner);
        assert!(!model.is_clear_pending());
    }

    #[test]
    fn no_action_is_sent_to_a_daemon_that_is_not_running() {
        for mut model in [NotificationsModel::new(None), {
            let mut model = NotificationsModel::new(None);
            model.refresh(&MockCommandRunner::with_outcomes([ok("b false\n")]));
            model
        }] {
            let runner = MockCommandRunner::new();
            assert!(matches!(
                model.set_dnd(&runner, true),
                Err(NotifdActionError::NotRunning)
            ));
            assert!(matches!(
                model.open_center(&runner),
                Err(NotifdActionError::NotRunning)
            ));
            model.request_clear();
            assert!(matches!(
                model.confirm_clear(&runner),
                Err(NotifdActionError::NotRunning)
            ));
            assert!(runner.recorded().is_empty());
        }
    }

    #[test]
    fn the_bar_json_path_is_under_the_runtime_dir() {
        assert_eq!(
            NotificationsModel::bar_json_path(Some(OsStr::new("/run/user/1000"))),
            Some(PathBuf::from("/run/user/1000/notifd/bar.json"))
        );
        assert_eq!(
            NotificationsModel::bar_json_path(Some(OsStr::new(""))),
            None
        );
        assert_eq!(NotificationsModel::bar_json_path(None), None);
    }
}
