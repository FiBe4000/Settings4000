//! GTK-free Notifications-page model: the runtime-only controls of the notifd
//! notification daemon (tasks 11.7, 13.1; architecture §3, §7; analysis §7.5, §8.2;
//! R4.2, R5.2, R6.1, R6.2, R7.3).
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
//!
//! # Opening the centre runs the eww script, never `SetCenter` (task 13.1)
//!
//! The notification centre is an eww window. notifd's `SetCenter(s)` method, despite
//! its name, does not open it: it only records the daemon's "center open" flag,
//! hides the popups on screen and mutes new ones. The window is opened by the eww
//! script `~/.config/eww/scripts/notif-center open` — what the bar click and
//! `SUPER+N` run — which opens the window on the focused monitor and only *then*
//! tells notifd (`notifctl center open`). The app therefore runs that script and
//! never calls `SetCenter`: a direct call would leave popups muted with no window on
//! screen (analysis §7.5). The button exists only when the script was detected and
//! `eww` is on `$PATH` ([`center_script`], R4.2).
//!
//! The script can take longer than the runner's usual 5 s: it retries the eww
//! daemon for 2–3 s before it gives up, and each of its own `eww`/`hyprctl` calls is
//! bounded at 5 s. So the open is split like the laptop-display toggle (task 10.3):
//! [`NotificationsModel::open_center_request`] captures what the run needs on the
//! main thread, [`OpenCenterRequest::run`] runs the script (under
//! [`CENTER_SCRIPT_TIMEOUT`]) and then re-reads the daemon's state on any thread,
//! and [`NotificationsModel::finish_open_center`] stores that read back on the main
//! thread. The page shows whether the centre is open from that read, never from an
//! assumption. Whether an open is in flight is kept by the page, not here, for the
//! same reason as the laptop toggle's flag: the page and its model may be rebuilt
//! while the script runs.

use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::core::apply::stderr_excerpt;
use crate::core::detect::{Binary, Capabilities};
use crate::core::notifd::{
    self, NotifdMethod, NotifdState, NotifdVersion, capture_stdout, control_command,
    name_has_owner_command, server_information_command,
};
use crate::system::command::{Command, CommandError, CommandRunner};

/// The state file notifd ≥ 1.0 rewrites with the four state fields, relative to
/// `$XDG_RUNTIME_DIR`. It holds no message text (unlike its neighbours `center.json`
/// and `popup<k>.json`, which the app never opens).
const BAR_JSON_RELATIVE: &[&str] = &["notifd", "bar.json"];

/// How long one run of the notification-centre script may take before the runner
/// kills it (task 13.1) — longer than the runner's usual 5 s on purpose.
///
/// A successful `open` on a busy eww daemon can need several seconds: the script
/// first retries `eww active-windows` for up to 2–3 s to see whether eww is up, checks
/// again whether the window is already open (another 2–3 s at worst), then runs a
/// few `hyprctl`/`eww` calls that it bounds at 5 s each, among them the `eww open`
/// itself, and finally `notifctl center open`. Killing it half-way would be worse
/// than waiting: after the window opened but before the daemon was told, popups and
/// the centre would disagree. Twenty seconds covers that sequence with margin while
/// still ending a genuinely wedged run; the run happens on a worker thread, so the
/// window stays responsive meanwhile.
///
/// One case outlasts this bound on purpose: an eww daemon that is running but wedged
/// (its process answers no request). The script's `eww_up` check then makes ten
/// attempts at `timeout 5 eww active-windows`, each followed by a 0.2 s pause, so
/// about 52 s pass before the script itself gives up with exit status 1. The runner
/// kills it at 20 s instead. The page then reports that the script timed out rather
/// than "eww is not running". That is accepted: the window stays responsive
/// throughout, the state re-read still follows, and the 20 s bound plus the 5 s read
/// stays inside the page's 30 s watchdog.
pub const CENTER_SCRIPT_TIMEOUT: Duration = Duration::from_secs(20);

/// `<script> open` under [`CENTER_SCRIPT_TIMEOUT`] — the command that opens the
/// notification centre exactly as the bar click and `SUPER+N` do (task 13.1).
///
/// `script` is the detected `~/.config/eww/scripts/notif-center`. The argument is the
/// fixed word `open` (the script's `open` is idempotent: an already open centre stays
/// open), never `toggle`, so a click can never close the centre. The script inherits
/// the app's environment, which it needs: `HOME` to find `notifctl`,
/// `XDG_RUNTIME_DIR` for its lock and notifd's state files, and
/// `HYPRLAND_INSTANCE_SIGNATURE` for `hyprctl`.
pub fn center_open_command(script: &str) -> Command {
    Command::new(script)
        .arg("open")
        .with_timeout(CENTER_SCRIPT_TIMEOUT)
}

/// The notification-centre script the Notifications page may run, or `None` when its
/// "open the notification center" button must be hidden (task 13.1; R4.2).
///
/// Both halves are required: the eww `notif-center` script detected at its live XDG
/// path ([`Capabilities::notif_center_script`]) and the `eww` binary on `$PATH`
/// ([`Binary::Eww`]) — the script drives eww and can do nothing without it. Without
/// either the button is hidden like any control whose application is missing; the
/// rest of the page is unaffected. A script path that is not UTF-8 never reaches
/// here (detection drops it), so the returned program string names exactly the
/// detected file.
pub fn center_script(capabilities: &Capabilities) -> Option<String> {
    let script = capabilities.notif_center_script()?;
    if !capabilities.has_binary(Binary::Eww) {
        // `debug`, not `info`: this runs on every page build and Refresh, and
        // detection has already logged the missing eww binary once at `info` (R4.2).
        tracing::debug!(
            script = %script.display(),
            "the eww notif-center script is present but eww is not on PATH; the \
             Notifications page offers no \"open the notification center\" button (R4.2)"
        );
        return None;
    }
    script.to_str().map(str::to_owned)
}

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
    /// The eww notification-centre script the "open" action runs, or `None` when the
    /// action is not offered (see [`center_script`]).
    center_script: Option<String>,
}

impl NotificationsModel {
    /// A model that has not queried anything yet. `bar_json` is the pre-1.1 state
    /// file location — [`Self::bar_json_path`] in production, a temp path in tests.
    /// `center_script` is what [`center_script`] decided from the detected
    /// capabilities: the script that opens the notification centre, or `None` to
    /// hide that action (task 13.1).
    pub fn new(bar_json: Option<PathBuf>, center_script: Option<String>) -> NotificationsModel {
        NotificationsModel {
            bar_json,
            view: DaemonView::NotQueried,
            clear_pending: false,
            center_script,
        }
    }

    /// Whether the "open the notification center" action is offered at all (task
    /// 13.1): the script and eww were detected. The page hides the button otherwise.
    pub fn can_open_center(&self) -> bool {
        self.center_script.is_some()
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
        let state = StateSource::for_version(version, self.bar_json.as_deref()).read(runner);
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

    /// Step 1 of opening the notification centre (task 13.1): checks that the action
    /// is offered and the daemon was last seen running, and captures what the run
    /// needs into an [`OpenCenterRequest`] — the script and where to re-read the state
    /// from afterwards — so it can run on a worker thread without borrowing the model.
    ///
    /// Runs nothing. Without a detected script (or eww) this is
    /// [`OpenCenterError::Unavailable`]; with the daemon last seen not running it is
    /// [`OpenCenterError::NotRunning`], because the script's final step,
    /// `notifctl center open`, could not reach the daemon and the window would open
    /// with notifd unaware of it.
    pub fn open_center_request(&self) -> Result<OpenCenterRequest, OpenCenterError> {
        let Some(script) = self.center_script.clone() else {
            tracing::info!(
                "opening the notification center is not offered (no notif-center script \
                 or no eww); nothing was run"
            );
            return Err(OpenCenterError::Unavailable);
        };
        let DaemonView::Running { version, .. } = self.view else {
            tracing::info!(
                "notifd is not running; the notification center was not opened (its script \
                 reports the open to the daemon)"
            );
            return Err(OpenCenterError::NotRunning);
        };
        Ok(OpenCenterRequest {
            script,
            state_source: StateSource::for_version(version, self.bar_json.as_deref()),
        })
    }

    /// Step 3 of opening the notification centre (task 13.1): stores the daemon state
    /// the finished run read after the script — so the page shows whether the centre
    /// really is open — and returns the run's result for the page to show (R5.5).
    ///
    /// The read is stored only while the model still sees the daemon running; a
    /// refresh in the meantime that found it stopped wins. When the re-read failed
    /// the previously shown state stays, as on any other failed read of a single
    /// value; the result still says whether the script succeeded.
    pub fn finish_open_center(
        &mut self,
        outcome: OpenCenterOutcome,
    ) -> Result<(), OpenCenterError> {
        let OpenCenterOutcome { state, result } = outcome;
        match (&mut self.view, state) {
            (DaemonView::Running { state: shown, .. }, Some(read)) => *shown = Some(read),
            (_, Some(_)) => tracing::debug!(
                "the notification center run finished after the daemon was seen stopped; \
                 its state read is not stored"
            ),
            (_, None) => tracing::info!(
                "the notifd state could not be re-read after the notification center script; \
                 the page keeps the state it showed"
            ),
        }
        result
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

/// Where the daemon's state is read from: `GetState()` on notifd ≥ 1.1, else the
/// pre-1.1 state file `bar.json` (see the module docs). Owned, so an
/// [`OpenCenterRequest`] can carry it to a worker thread.
#[derive(Clone, Debug, PartialEq, Eq)]
enum StateSource {
    /// `GetState()` over D-Bus (daemon ≥ 1.1).
    GetState,
    /// `bar.json` at this path, or `None` when `$XDG_RUNTIME_DIR` is unset (the
    /// state is then unknown).
    BarJson(Option<PathBuf>),
}

impl StateSource {
    /// The source for a daemon that reported `version` (`None`: no usable version,
    /// treated as older than 1.1).
    fn for_version(version: Option<NotifdVersion>, bar_json: Option<&Path>) -> StateSource {
        match version {
            Some(version) if version.supports_state_and_reload() => StateSource::GetState,
            _ => StateSource::BarJson(bar_json.map(Path::to_path_buf)),
        }
    }

    /// Reads the state, degrading every failure to `None` (logged at `info`).
    fn read(&self, runner: &dyn CommandRunner) -> Option<NotifdState> {
        match self {
            StateSource::GetState => state_from_dbus(runner),
            StateSource::BarJson(path) => state_from_bar_json(path.as_deref()),
        }
    }
}

/// The state from `GetState()` (daemon ≥ 1.1).
fn state_from_dbus(runner: &dyn CommandRunner) -> Option<NotifdState> {
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
fn state_from_bar_json(path: Option<&Path>) -> Option<NotifdState> {
    let Some(path) = path else {
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

/// Why the notification centre was not opened (task 13.1). Its text completes the
/// page's "The notification center could not be opened: …" line.
#[derive(Debug)]
pub enum OpenCenterError {
    /// The action is not offered: no `notif-center` script or no eww was detected.
    /// The page hides the button then, so only a caller that skipped
    /// [`NotificationsModel::can_open_center`] sees this.
    Unavailable,
    /// The daemon was last seen not running; nothing was run.
    NotRunning,
    /// The script exited with status 1 with its valid `open` argument. The script
    /// (dotfiles commit `e8f7558`) has three causes for exit status 1:
    ///
    /// - eww's daemon does not answer (`eww_up` fails). This is the usual case, when
    ///   eww is installed but its daemon is not running.
    /// - An `eww open` fails against a daemon that answered a moment earlier.
    /// - The script's last step, `notifctl center open`, fails. That `busctl` call
    ///   runs after the window has opened, so the centre may be on screen without
    ///   notifd knowing.
    ///
    /// The page says "eww is not running" for all three, as task 13.1 specifies:
    /// the script reports no finer cause, and the first is by far the most common.
    /// The state re-read after the script keeps the page honest whichever cause it
    /// was, because it shows what notifd actually reports.
    EwwNotRunning,
    /// The script ran and exited unsuccessfully with any other status. Its stderr is
    /// logged at `debug` only and is not part of the message.
    Failed {
        /// The exit code, or `None` when the process was terminated by a signal.
        code: Option<i32>,
    },
    /// The script could not be run: it failed to start, or it overran
    /// [`CENTER_SCRIPT_TIMEOUT`] and was killed.
    Command(CommandError),
}

impl fmt::Display for OpenCenterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpenCenterError::Unavailable => {
                f.write_str("the eww notification center script or eww itself was not found")
            }
            OpenCenterError::NotRunning => f.write_str("the notification daemon is not running"),
            OpenCenterError::EwwNotRunning => f.write_str("eww is not running"),
            OpenCenterError::Failed { code: Some(code) } => {
                write!(f, "the notif-center script exited with status {code}")
            }
            OpenCenterError::Failed { code: None } => {
                f.write_str("the notif-center script was terminated by a signal")
            }
            OpenCenterError::Command(error) => {
                write!(f, "the notif-center script could not be run: {error}")
            }
        }
    }
}

impl std::error::Error for OpenCenterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            OpenCenterError::Command(error) => Some(error),
            _ => None,
        }
    }
}

/// An open of the notification centre ready to run, made by
/// [`NotificationsModel::open_center_request`] (step 1; task 13.1).
///
/// It owns copies of what the commands need and nothing else, and is [`Send`], so
/// step 2 ([`Self::run`]) can issue them on a worker thread while the model stays on
/// the main thread (see the module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenCenterRequest {
    /// The detected `notif-center` script, as the program string of a [`Command`].
    script: String,
    /// Where the state is re-read from after the script.
    state_source: StateSource,
}

impl OpenCenterRequest {
    /// Step 2 of opening the notification centre (task 13.1): runs the eww script and
    /// then re-reads the daemon's state, blocking for as long as both take — so the
    /// page calls it on a worker thread. It touches no model.
    ///
    /// Issues exactly this sequence through `runner`:
    ///
    /// 1. `<script> open` ([`center_open_command`], bounded by
    ///    [`CENTER_SCRIPT_TIMEOUT`]) — the script opens the eww window on the focused
    ///    monitor and then tells notifd itself;
    /// 2. the state read: `GetState` on a daemon ≥ 1.1 (on an older one, `bar.json` is
    ///    read and no command is issued).
    ///
    /// The read follows every script outcome, failures included: it is what the page
    /// shows, and after a timeout it is the only way to learn whether the centre got
    /// open anyway. Nothing here ever calls notifd's `SetCenter`. The script's failure
    /// is logged at `error` (its stderr at `debug` only) and returned in the outcome;
    /// none is fatal (R5.5).
    pub fn run(self, runner: &dyn CommandRunner) -> OpenCenterOutcome {
        let OpenCenterRequest {
            script,
            state_source,
        } = self;
        let result = run_center_script(runner, &script);
        let state = state_source.read(runner);
        if result.is_ok() {
            tracing::info!(
                center_open = ?state.map(|state| state.center_open),
                "ran the eww notif-center script to open the notification center (R5.2)"
            );
        }
        OpenCenterOutcome { state, result }
    }
}

/// What an open of the notification centre read and how it ended, produced on any
/// thread by [`OpenCenterRequest::run`] and stored by
/// [`NotificationsModel::finish_open_center`] (task 13.1). [`Send`], so a worker
/// thread can hand it back to the main thread.
#[derive(Debug)]
pub struct OpenCenterOutcome {
    /// The daemon state read after the script, or `None` when that read failed.
    state: Option<NotifdState>,
    /// Whether the script reported success, and why not.
    result: Result<(), OpenCenterError>,
}

/// Runs `<script> open` and classifies its outcome (see [`OpenCenterError`]).
fn run_center_script(runner: &dyn CommandRunner, script: &str) -> Result<(), OpenCenterError> {
    let command = center_open_command(script);
    let output = match runner.run(&command) {
        Ok(output) => output,
        Err(error) => {
            tracing::error!(%command, %error, "could not run the eww notif-center script");
            return Err(OpenCenterError::Command(error));
        }
    };
    if output.success() {
        return Ok(());
    }
    // The script's diagnostics are for debugging only; the page names the cause.
    tracing::debug!(
        %command,
        stderr = ?stderr_excerpt(output.stderr()),
        "eww notif-center script stderr"
    );
    let error = match output.code() {
        Some(1) => OpenCenterError::EwwNotRunning,
        code => OpenCenterError::Failed { code },
    };
    tracing::error!(%command, exit_code = ?output.code(), %error, "the eww notif-center script failed");
    Err(error)
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
        let mut model = NotificationsModel::new(None, None);
        let runner = MockCommandRunner::with_outcomes([ok(OWNED), ok(INFO_1_1), ok(STATE)]);
        model.refresh(&runner);
        model
    }

    #[test]
    fn refresh_on_a_1_1_daemon_reads_get_state() {
        let mut model = NotificationsModel::new(None, None);
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
        let mut model = NotificationsModel::new(None, None);
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
        let mut model = NotificationsModel::new(Some(dir.path().join("bar.json")), None);
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
        let mut model = NotificationsModel::new(Some(dir.path().join("bar.json")), None);
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
        let mut model = NotificationsModel::new(Some(dir.path().join("bar.json")), None);
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
        let mut model = NotificationsModel::new(Some(dir.path().join("bar.json")), None);
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
        let mut model = NotificationsModel::new(None, None);
        let runner = MockCommandRunner::with_outcomes([ok("b false\n")]);
        assert_eq!(model.refresh(&runner), DaemonView::NotRunning);
    }

    #[test]
    fn an_unowned_daemon_is_asked_nothing_more() {
        // Liveness is the bus name; the page never asks systemd or the daemon itself.
        let mut model = NotificationsModel::new(None, None);
        let runner = MockCommandRunner::with_outcomes([ok("b false\n")]);
        model.refresh(&runner);
        assert_eq!(runner.recorded(), vec![name_has_owner_command()]);
        assert!(!runner.recorded().contains(&unit_is_active_command()));
    }

    #[test]
    fn a_failed_owner_query_reads_as_not_running() {
        let mut model = NotificationsModel::new(None, None);
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

    /// A placeholder path for the detected `notif-center` script; nothing is executed.
    const CENTER_SCRIPT: &str = "/test/config/eww/scripts/notif-center";

    /// [`STATE`] with the centre open, as notifd reports it once the script told it.
    const STATE_CENTER_OPEN: &str = "bubb false 22 false true\n";

    /// A model with the centre script, that already saw a running 1.1 daemon with
    /// [`STATE`].
    fn running_model_with_center() -> NotificationsModel {
        let mut model = NotificationsModel::new(None, Some(CENTER_SCRIPT.to_string()));
        let runner = MockCommandRunner::with_outcomes([ok(OWNED), ok(INFO_1_1), ok(STATE)]);
        model.refresh(&runner);
        model
    }

    /// The centre-open flag the model currently shows, if its state is known.
    fn shown_center_open(model: &NotificationsModel) -> Option<bool> {
        match model.view() {
            DaemonView::Running {
                state: Some(state), ..
            } => Some(state.center_open),
            _ => None,
        }
    }

    /// Runs the three steps of an open against `runner`, returning the result.
    fn open_center(
        model: &mut NotificationsModel,
        runner: &MockCommandRunner,
    ) -> Result<(), OpenCenterError> {
        let request = model.open_center_request().expect("the open is offered");
        let outcome = request.run(runner);
        model.finish_open_center(outcome)
    }

    #[test]
    fn opening_the_center_runs_the_eww_script_then_reads_get_state() {
        // Task 13.1: exactly what the bar click and SUPER+N do — `notif-center open`,
        // which opens the window and then tells notifd — followed by a GetState read
        // that decides what the page shows. Never SetCenter.
        let mut model = running_model_with_center();
        assert!(model.can_open_center());
        assert_eq!(shown_center_open(&model), Some(false));

        let runner = MockCommandRunner::with_outcomes([ok(""), ok(STATE_CENTER_OPEN)]);
        open_center(&mut model, &runner).expect("the script succeeds");

        let recorded = runner.recorded();
        assert_eq!(
            recorded,
            vec![
                center_open_command(CENTER_SCRIPT),
                control_command(NotifdMethod::GetState),
            ]
        );
        assert_eq!(recorded[0].program(), CENTER_SCRIPT);
        assert_eq!(recorded[0].args_slice(), ["open"]);
        let timeout = recorded[0].timeout().expect("the script has its own bound");
        assert!(timeout >= Duration::from_secs(15), "{timeout:?}");
        assert_eq!(
            recorded[1].timeout(),
            None,
            "GetState keeps the 5 s default"
        );
        assert!(
            recorded
                .iter()
                .all(|command| !command.to_string().contains("SetCenter")),
            "{recorded:?}"
        );
        assert_eq!(shown_center_open(&model), Some(true), "shown from the read");
    }

    #[test]
    fn the_center_is_shown_from_the_read_not_from_the_script_status() {
        // A script that exits 0 while the daemon still reports the centre closed is
        // shown as closed: the page never assumes the open worked.
        let mut model = running_model_with_center();
        let runner = MockCommandRunner::with_outcomes([ok(""), ok(STATE)]);
        open_center(&mut model, &runner).expect("the script succeeds");
        assert_eq!(shown_center_open(&model), Some(false));
    }

    #[test]
    fn exit_status_1_means_eww_is_not_running() {
        // The script exits 1 when eww's daemon does not answer. The state is still
        // re-read, and the message names eww.
        let mut model = running_model_with_center();
        let runner = MockCommandRunner::with_outcomes([
            Ok(CommandOutput::fake_with_streams(1, "", "")),
            ok(STATE),
        ]);
        let error = open_center(&mut model, &runner).expect_err("eww is down");
        assert!(matches!(error, OpenCenterError::EwwNotRunning));
        assert_eq!(error.to_string(), "eww is not running");
        assert_eq!(
            runner.recorded(),
            vec![
                center_open_command(CENTER_SCRIPT),
                control_command(NotifdMethod::GetState),
            ]
        );
        assert_eq!(shown_center_open(&model), Some(false));
    }

    #[test]
    fn another_failure_names_the_exit_status_but_not_the_stderr() {
        // Exit 2 (a bad argument, which `open` never is) or any other status: the
        // generic message with the code; stderr stays in the debug log.
        let mut model = running_model_with_center();
        let runner = MockCommandRunner::with_outcomes([
            Ok(CommandOutput::fake_with_streams(
                2,
                "",
                "usage: notif-center toggle|open|close",
            )),
            ok(STATE),
        ]);
        let error = open_center(&mut model, &runner).expect_err("the script failed");
        assert!(matches!(error, OpenCenterError::Failed { code: Some(2) }));
        assert_eq!(
            error.to_string(),
            "the notif-center script exited with status 2"
        );
        assert_eq!(
            OpenCenterError::Failed { code: None }.to_string(),
            "the notif-center script was terminated by a signal"
        );
    }

    #[test]
    fn a_script_that_overruns_its_bound_is_reported_and_the_state_still_read() {
        // After a timeout the window may or may not have opened; the read decides.
        let mut model = running_model_with_center();
        let runner = MockCommandRunner::with_outcomes([
            Err(CommandError::Timeout {
                limit: CENTER_SCRIPT_TIMEOUT,
            }),
            ok(STATE_CENTER_OPEN),
        ]);
        let error = open_center(&mut model, &runner).expect_err("the script timed out");
        assert!(matches!(
            error,
            OpenCenterError::Command(CommandError::Timeout { .. })
        ));
        assert!(
            error
                .to_string()
                .starts_with("the notif-center script could not be run")
        );
        assert_eq!(runner.recorded().len(), 2);
        assert_eq!(shown_center_open(&model), Some(true));
    }

    #[test]
    fn a_failed_re_read_keeps_the_shown_state() {
        let mut model = running_model_with_center();
        let runner = MockCommandRunner::with_outcomes([ok(""), Ok(CommandOutput::fake(1))]);
        open_center(&mut model, &runner).expect("the script succeeded");
        assert_eq!(shown_center_open(&model), Some(false));
    }

    #[test]
    fn a_pre_1_1_daemon_is_re_read_from_bar_json_after_the_script() {
        // A daemon without GetState: the script, then bar.json — and no D-Bus call.
        let dir = bar_json_dir();
        std::fs::write(
            dir.path().join("bar.json"),
            br#"{"count": 4, "dnd": true, "inhibited": true, "center_open": true}"#,
        )
        .expect("rewrite bar.json");
        let mut model = NotificationsModel::new(
            Some(dir.path().join("bar.json")),
            Some(CENTER_SCRIPT.to_string()),
        );
        model.refresh(&MockCommandRunner::with_outcomes([ok(OWNED), ok(INFO_1_0)]));

        let runner = MockCommandRunner::new();
        open_center(&mut model, &runner).expect("the script succeeds");
        assert_eq!(runner.recorded(), vec![center_open_command(CENTER_SCRIPT)]);
        assert_eq!(shown_center_open(&model), Some(true));
    }

    #[test]
    fn the_open_is_refused_without_a_script_or_a_running_daemon() {
        // Without the script (or eww) the action is not offered at all.
        let model = running_model();
        assert!(!model.can_open_center());
        assert!(matches!(
            model.open_center_request(),
            Err(OpenCenterError::Unavailable)
        ));

        // With the script, but the daemon not running (or not yet read): refused, so
        // the window is never opened behind a daemon that cannot be told.
        let mut model = NotificationsModel::new(None, Some(CENTER_SCRIPT.to_string()));
        assert!(matches!(
            model.open_center_request(),
            Err(OpenCenterError::NotRunning)
        ));
        model.refresh(&MockCommandRunner::with_outcomes([ok("b false\n")]));
        assert!(matches!(
            model.open_center_request(),
            Err(OpenCenterError::NotRunning)
        ));
    }

    #[test]
    fn a_read_that_arrives_after_the_daemon_stopped_is_not_stored() {
        let mut model = running_model_with_center();
        let request = model.open_center_request().expect("offered");
        let outcome = request.run(&MockCommandRunner::with_outcomes([
            ok(""),
            ok(STATE_CENTER_OPEN),
        ]));
        model.refresh(&MockCommandRunner::with_outcomes([ok("b false\n")]));
        model
            .finish_open_center(outcome)
            .expect("the script succeeded");
        assert_eq!(model.view(), DaemonView::NotRunning);
    }

    #[test]
    fn the_button_needs_both_the_script_and_eww() {
        // Task 13.1, R4.2: the gate is the detected script *and* eww on $PATH.
        let script = std::path::PathBuf::from(CENTER_SCRIPT);
        let both = Capabilities::for_tests(&[Binary::Eww], &[], false)
            .with_notif_center_script_for_tests(script.clone());
        assert_eq!(center_script(&both).as_deref(), Some(CENTER_SCRIPT));

        let no_eww =
            Capabilities::for_tests(&[], &[], false).with_notif_center_script_for_tests(script);
        assert_eq!(center_script(&no_eww), None);

        let no_script = Capabilities::for_tests(&[Binary::Eww], &[], false);
        assert_eq!(center_script(&no_script), None);
        assert!(!NotificationsModel::new(None, center_script(&no_script)).can_open_center());
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
        for mut model in [NotificationsModel::new(None, None), {
            let mut model = NotificationsModel::new(None, None);
            model.refresh(&MockCommandRunner::with_outcomes([ok("b false\n")]));
            model
        }] {
            let runner = MockCommandRunner::new();
            assert!(matches!(
                model.set_dnd(&runner, true),
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
