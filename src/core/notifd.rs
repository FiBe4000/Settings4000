//! The D-Bus surface of the notifd notification daemon: the exact `busctl` /
//! `systemctl` commands the app issues and strict parsers for their replies (task
//! 11.7; architecture §3, §4; analysis §7.5, §8.2; R4.1, R5.2, R6.1, R7.3).
//!
//! # What notifd is
//!
//! The target dotfiles run their own notification server, `scripts/notifd` (a
//! Python/Gio daemon started as the systemd user unit `notifd.service`). It owns two
//! session-bus names: the standard `org.freedesktop.Notifications` (where
//! applications send notifications and where `GetServerInformation` lives, on the
//! object `/org/freedesktop/Notifications`), and a private control name
//! `se.dotfiles.Notifd`, which exposes the interface `se.dotfiles.Notifd` on the
//! object `/se/dotfiles/Notifd` from a **second** bus connection. The two names are
//! deliberately served by different connections, so a method of one interface is
//! not reachable through the other name.
//!
//! The control interface is what this app drives: `GetState()` (signature `bubb`:
//! do-not-disturb, history count, inhibited, center open — a pure query),
//! `SetDnd(s on|off)`, `Clear()` and `Reload()`. `GetState` and `Reload` exist from
//! daemon version **1.1** on (analysis §8.2);
//! [`NotifdVersion::supports_state_and_reload`] is the feature test.
//!
//! The interface also has `SetCenter(s open|close)`, which the app deliberately
//! never calls (task 13.1; analysis §7.5). Despite its name it opens nothing: it
//! only records the daemon's "center open" flag, which mutes popups. The centre is
//! an eww window that the eww script `notif-center` opens and *then* reports to the
//! daemon; calling `SetCenter open` directly would leave popups muted with no window
//! on screen. The app opens the centre through that script instead
//! ([`crate::core::notifications`]), so [`NotifdMethod`] has no variant for it.
//!
//! # Why `busctl`, and why `--auto-start=no`
//!
//! The app has no D-Bus library; every side effect goes through the
//! [`CommandRunner`] seam as an argument vector (no shell). `busctl` is systemd's
//! D-Bus CLI and is what the dotfiles' own `scripts/notifctl` uses, so the app makes
//! exactly the calls the desktop already makes. Every call addressed to a notifd
//! name passes `--auto-start=no`: `org.freedesktop.Notifications` is D-Bus
//! activatable (it starts `notifd.service`), so a probe without that flag would
//! *start* the daemon it is trying to detect. `se.dotfiles.Notifd` has no activation
//! file, but the flag is passed there too so no call can ever activate anything. The
//! one call without it, `NameHasOwner`, goes to the bus itself
//! (`org.freedesktop.DBus`), which is never activated.
//!
//! Detection is by bus-name ownership, never by a binary on `$PATH`: the daemon
//! notifd replaced may still be installed, so a binary check for a notification
//! daemon would be a false positive, and its client tool would D-Bus-activate the
//! old daemon (analysis §7.5, §8.3). The app never invokes that client.
//!
//! # Reply parsing is strict
//!
//! `busctl` prints a method reply as the reply signature followed by the values, all
//! on one line: `b true`, `bubb false 22 false false`,
//! `ssss "notifd" "dotfiles" "1.1" "1.3"`. The parsers here accept exactly the
//! expected signature and value count and return `None` for anything else, so a
//! changed daemon, a foreign server or an error text degrades to "unknown" instead of
//! being guessed at.
//!
//! # What is never read
//!
//! notifd also mirrors its state into JSON files under `$XDG_RUNTIME_DIR/notifd/`.
//! `center.json` and `popup<k>.json` (and the persistent `history.json`) carry
//! message bodies, which may include one-time codes, so the app never opens them.
//! Only `bar.json` — four state fields and no message text — is read, and only as
//! the fallback for a daemon older than 1.1 that has no `GetState` ([`parse_bar_json`]).

use std::fmt;

use crate::system::command::{Command, CommandRunner};

/// The `busctl` executable, systemd's D-Bus command-line client.
pub const BUSCTL: &str = "busctl";
/// notifd's private control bus name, owned only while the daemon runs.
pub const NOTIFD_BUS_NAME: &str = "se.dotfiles.Notifd";
/// The object path of notifd's control interface.
pub const NOTIFD_OBJECT_PATH: &str = "/se/dotfiles/Notifd";
/// The name of notifd's control interface (identical to the bus name).
pub const NOTIFD_INTERFACE: &str = "se.dotfiles.Notifd";
/// The systemd user unit that runs notifd.
pub const NOTIFD_UNIT: &str = "notifd.service";
/// The server name notifd reports as the first field of `GetServerInformation`.
pub const NOTIFD_SERVER_NAME: &str = "notifd";

/// The standard notification bus name, on which notifd answers
/// `GetServerInformation`.
const NOTIFICATIONS_BUS_NAME: &str = "org.freedesktop.Notifications";
/// The object path of the standard notification interface.
const NOTIFICATIONS_OBJECT_PATH: &str = "/org/freedesktop/Notifications";
/// The standard notification interface name.
const NOTIFICATIONS_INTERFACE: &str = "org.freedesktop.Notifications";

/// A notifd daemon version as reported by `GetServerInformation` (`"1.1"`),
/// compared as `major.minor`.
///
/// The version is the daemon's feature-detection key (analysis §8.2): 1.1 added
/// `GetState`, `Reload` and the config file. Ordering is numeric per component, so
/// `1.10` is newer than `1.9`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NotifdVersion {
    /// The major version component.
    pub major: u32,
    /// The minor version component.
    pub minor: u32,
}

impl NotifdVersion {
    /// The first version with `GetState()`, `Reload()` and `~/.config/notifd/config`.
    pub const STATE_AND_RELOAD: NotifdVersion = NotifdVersion { major: 1, minor: 1 };

    /// Parses a version string of the form `major.minor` (further `.patch`
    /// components are accepted and ignored), or `None` for anything else — an empty
    /// string, a bare `1`, a suffix like `1.1-dev`. Rejecting rather than guessing
    /// keeps an unrecognised daemon on the conservative pre-1.1 path.
    pub fn parse(text: &str) -> Option<NotifdVersion> {
        let mut parts = text.split('.');
        let major = parse_component(parts.next()?)?;
        let minor = parse_component(parts.next()?)?;
        for rest in parts {
            parse_component(rest)?;
        }
        Some(NotifdVersion { major, minor })
    }

    /// Whether this daemon has `GetState()` and `Reload()` (version ≥ 1.1).
    pub fn supports_state_and_reload(self) -> bool {
        self >= NotifdVersion::STATE_AND_RELOAD
    }
}

/// Parses one all-digit version component.
fn parse_component(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

impl fmt::Display for NotifdVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// Whether notifd is available on this machine, the capability that gates the
/// Notifications page (task 11.7; architecture §4; R4.1, R4.2).
///
/// Resolved by [`NotifdStatus::resolve`] from the replies [`NotifdReplies::gather`]
/// captured, so the decision itself is pure and testable with injected replies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotifdStatus {
    /// notifd owns `se.dotfiles.Notifd` right now. `version` is what it reported
    /// through `GetServerInformation`, or `None` when that call failed or did not
    /// identify the server as notifd — treated as older than 1.1, so only the
    /// calls every version has are used.
    Running {
        /// The reported daemon version, if any.
        version: Option<NotifdVersion>,
    },
    /// notifd is not running but is installed: its user unit file exists or systemd
    /// knows the unit. The daemon is D-Bus activated by the first notification an
    /// application sends, so it is expected to appear without the user's help.
    Installed,
    /// No sign of notifd, or the session bus could not be queried at all (no
    /// `busctl`, no session bus). Every detection failure lands here (R4.3).
    Absent,
}

impl NotifdStatus {
    /// Decides the status from captured probe replies (pure; see the type docs).
    ///
    /// `unit_file_present` is whether `~/.config/systemd/user/notifd.service`
    /// exists — the dotfiles link it there, so a present file means "installed,
    /// will activate" even when systemd reports the unit inactive.
    ///
    /// - The `NameHasOwner` reply decides liveness. When it is missing or not a
    ///   well-formed `b true|false`, the bus could not be asked and the result is
    ///   [`Absent`](Self::Absent): every later call would go through the same
    ///   `busctl`, so nothing on the page could work.
    /// - An owner means [`Running`](Self::Running), with the version taken from the
    ///   `GetServerInformation` reply only when its server name is `notifd` (another
    ///   server could own `org.freedesktop.Notifications`).
    /// - No owner means [`Installed`](Self::Installed) when the unit file exists or
    ///   `systemctl --user is-active` names a state only a known unit can be in
    ///   (`active`, `activating`, `failed`, …), else [`Absent`](Self::Absent).
    ///   `inactive` alone proves nothing — systemd prints it for units that do not
    ///   exist.
    pub fn resolve(replies: &NotifdReplies, unit_file_present: bool) -> NotifdStatus {
        match replies
            .name_has_owner
            .as_deref()
            .and_then(parse_name_has_owner)
        {
            None => NotifdStatus::Absent,
            Some(true) => NotifdStatus::Running {
                version: replies
                    .server_information
                    .as_deref()
                    .and_then(parse_notifd_version),
            },
            Some(false) => {
                let unit_known = replies
                    .unit_active
                    .as_deref()
                    .is_some_and(unit_state_implies_installed);
                if unit_file_present || unit_known {
                    NotifdStatus::Installed
                } else {
                    NotifdStatus::Absent
                }
            }
        }
    }

    /// Whether the Notifications page should be offered: the daemon runs or is
    /// installed (architecture §4 — an installed daemon still gets the runtime rows,
    /// which report that it is not running until it starts).
    pub fn is_present(self) -> bool {
        !matches!(self, NotifdStatus::Absent)
    }

    /// The running daemon's reported version, if it runs and reported one.
    pub fn version(self) -> Option<NotifdVersion> {
        match self {
            NotifdStatus::Running { version } => version,
            NotifdStatus::Installed | NotifdStatus::Absent => None,
        }
    }

    /// Whether a running daemon has `GetState()` and `Reload()` (version ≥ 1.1).
    /// This is the gate the file-backed notification rows and the `Reload()` call
    /// need (tasks 11.14, 11.15).
    pub fn supports_state_and_reload(self) -> bool {
        self.version()
            .is_some_and(NotifdVersion::supports_state_and_reload)
    }
}

impl fmt::Display for NotifdStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NotifdStatus::Running {
                version: Some(version),
            } => write!(f, "running {version}"),
            NotifdStatus::Running { version: None } => f.write_str("running (version unknown)"),
            NotifdStatus::Installed => f.write_str("installed, not running"),
            NotifdStatus::Absent => f.write_str("absent"),
        }
    }
}

/// The raw stdout of the detection probes, captured by [`Self::gather`] and decided
/// by [`NotifdStatus::resolve`] (R6.1: tests inject fake replies instead).
///
/// Each field is `None` when its command was not issued or could not be run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NotifdReplies {
    /// The reply of `busctl … NameHasOwner s se.dotfiles.Notifd` (`b true`), from
    /// a successful run only.
    pub name_has_owner: Option<String>,
    /// The reply of `GetServerInformation` (`ssss "notifd" "dotfiles" "1.1" "1.3"`),
    /// from a successful run only; asked only when the name has an owner.
    pub server_information: Option<String>,
    /// The stdout of `systemctl --user is-active notifd.service` (`inactive`,
    /// `active`, …) whatever its exit status — `is-active` exits non-zero for every
    /// state but `active`, and the word is what matters; asked only when the name
    /// has no owner.
    pub unit_active: Option<String>,
}

impl NotifdReplies {
    /// Runs the detection probes through `runner` (R4.1).
    ///
    /// At most two commands run: `NameHasOwner` always, then `GetServerInformation`
    /// when the name is owned or `systemctl --user is-active notifd.service` when it
    /// is not (the unit state only matters for a daemon that is not running). When
    /// `NameHasOwner` fails or its reply is malformed nothing else is asked. Every
    /// failure is logged at `debug` and leaves its field `None`; nothing here can
    /// start the daemon (see the module docs).
    pub fn gather(runner: &dyn CommandRunner) -> NotifdReplies {
        let name_has_owner = capture_stdout(runner, &name_has_owner_command(), true);
        let mut replies = NotifdReplies {
            name_has_owner,
            ..NotifdReplies::default()
        };
        match replies
            .name_has_owner
            .as_deref()
            .and_then(parse_name_has_owner)
        {
            Some(true) => {
                replies.server_information =
                    capture_stdout(runner, &server_information_command(), true);
            }
            Some(false) => {
                replies.unit_active = capture_stdout(runner, &unit_is_active_command(), false);
            }
            None => {}
        }
        replies
    }
}

/// `busctl --user call org.freedesktop.DBus /org/freedesktop/DBus
/// org.freedesktop.DBus NameHasOwner s se.dotfiles.Notifd` — asks the bus itself
/// whether notifd's control name has an owner. The bus driver is never activated,
/// so this call cannot start anything.
pub fn name_has_owner_command() -> Command {
    Command::new(BUSCTL).args([
        "--user",
        "call",
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "NameHasOwner",
        "s",
        NOTIFD_BUS_NAME,
    ])
}

/// `busctl --user --auto-start=no call org.freedesktop.Notifications
/// /org/freedesktop/Notifications org.freedesktop.Notifications
/// GetServerInformation` — the daemon's name and version.
///
/// The method lives on the standard notification object, which notifd serves from
/// its main connection; its control connection (`se.dotfiles.Notifd`) exports only
/// `/se/dotfiles/Notifd`, so the call must be addressed to the standard name. The
/// standard name is D-Bus activatable, which is why `--auto-start=no` is essential
/// here.
pub fn server_information_command() -> Command {
    Command::new(BUSCTL).args([
        "--user",
        "--auto-start=no",
        "call",
        NOTIFICATIONS_BUS_NAME,
        NOTIFICATIONS_OBJECT_PATH,
        NOTIFICATIONS_INTERFACE,
        "GetServerInformation",
    ])
}

/// `systemctl --user is-active notifd.service` — the unit's state word.
pub fn unit_is_active_command() -> Command {
    Command::new("systemctl").args(["--user", "is-active", NOTIFD_UNIT])
}

/// A method of notifd's control interface the app calls.
///
/// The Notifications page uses the state and action methods; the reload table
/// ([`crate::core::reload`]) uses [`Reload`](Self::Reload).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotifdMethod {
    /// `GetState()` → `bubb` (≥ 1.1).
    GetState,
    /// `SetDnd(s "on"|"off")` → `b` (the new state).
    SetDnd(bool),
    /// `Clear()` — removes every notification from the history and the screen.
    Clear,
    /// `Reload()` (≥ 1.1) — re-reads `~/.config/notifd/config` and the icon theme
    /// (from `gtk-3.0/settings.ini`) in place, keeping on-screen popups, timers,
    /// inhibitors and the history (analysis §8.2). This is the only way the app
    /// makes notifd pick up a change: a unit restart would drop the popups and
    /// SIGHUP stops the daemon.
    Reload,
}

/// `busctl --user --auto-start=no call se.dotfiles.Notifd /se/dotfiles/Notifd
/// se.dotfiles.Notifd <Method> [<signature> <args>…]` — one control call, the exact
/// form `scripts/notifctl` uses.
///
/// Arguments are fixed tokens (`on`/`off`); no user text ever reaches the argument
/// vector. There is deliberately no `SetCenter` call (see the module docs).
pub fn control_command(method: NotifdMethod) -> Command {
    let command = Command::new(BUSCTL).args([
        "--user",
        "--auto-start=no",
        "call",
        NOTIFD_BUS_NAME,
        NOTIFD_OBJECT_PATH,
        NOTIFD_INTERFACE,
    ]);
    match method {
        NotifdMethod::GetState => command.arg("GetState"),
        NotifdMethod::SetDnd(enabled) => {
            command.args(["SetDnd", "s", if enabled { "on" } else { "off" }])
        }
        NotifdMethod::Clear => command.arg("Clear"),
        NotifdMethod::Reload => command.arg("Reload"),
    }
}

/// Runs `command` and returns its stdout as text, or `None` when it could not run
/// or — with `require_success` — exited non-zero. Failures are logged at `debug`
/// only: callers degrade them to "unknown" or "absent", never to an error.
pub fn capture_stdout(
    runner: &dyn CommandRunner,
    command: &Command,
    require_success: bool,
) -> Option<String> {
    match runner.run(command) {
        Ok(output) if output.success() || !require_success => {
            Some(String::from_utf8_lossy(output.stdout()).into_owned())
        }
        Ok(output) => {
            tracing::debug!(%command, exit_code = ?output.code(), "notifd probe reported failure");
            None
        }
        Err(error) => {
            tracing::debug!(%command, %error, "notifd probe could not be run");
            None
        }
    }
}

/// The notifd state shown on the Notifications page — the four `GetState()`
/// fields, which are also the four fields of `bar.json`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotifdState {
    /// Do-not-disturb is on: popups are suppressed, notifications still collect in
    /// the history.
    pub dnd: bool,
    /// How many notifications the history holds (what the bar's counter shows).
    pub count: u32,
    /// At least one application asked notifd to hold popups back (for example a
    /// screen-sharing session).
    pub inhibited: bool,
    /// The notification center is currently open.
    pub center_open: bool,
}

/// One whitespace-separated token of a `busctl` reply line.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ReplyToken {
    /// An unquoted word: the signature, a boolean or a number.
    Bare(String),
    /// A double-quoted string, with its quotes removed and `\"`/`\\` unescaped.
    Quoted(String),
}

/// Splits a `busctl` reply into tokens, or `None` when it is not one well-formed
/// line: a second non-empty line, an unterminated string, or an escape other than
/// `\"` and `\\` (busctl escapes other bytes too, but no value the app compares
/// against contains them, so such a reply is simply not one the app expects).
fn reply_tokens(text: &str) -> Option<Vec<ReplyToken>> {
    let line = text.trim_end_matches(['\n', '\r']);
    if line.contains('\n') {
        return None;
    }
    let mut tokens = Vec::new();
    let mut chars = line.chars().peekable();
    while let Some(&next) = chars.peek() {
        if next == ' ' || next == '\t' {
            chars.next();
            continue;
        }
        if next == '"' {
            chars.next();
            let mut value = String::new();
            loop {
                match chars.next()? {
                    '"' => break,
                    '\\' => match chars.next()? {
                        escaped @ ('"' | '\\') => value.push(escaped),
                        _ => return None,
                    },
                    other => value.push(other),
                }
            }
            // A closing quote must end the token.
            if chars.peek().is_some_and(|c| *c != ' ' && *c != '\t') {
                return None;
            }
            tokens.push(ReplyToken::Quoted(value));
        } else {
            let mut word = String::new();
            while let Some(&c) = chars.peek() {
                if c == ' ' || c == '\t' {
                    break;
                }
                if c == '"' {
                    return None;
                }
                word.push(c);
                chars.next();
            }
            tokens.push(ReplyToken::Bare(word));
        }
    }
    Some(tokens)
}

/// Splits a reply into its signature and values, requiring the signature to be
/// `expected`.
fn reply_values(text: &str, expected: &str) -> Option<Vec<ReplyToken>> {
    let mut tokens = reply_tokens(text)?;
    if tokens.first() != Some(&ReplyToken::Bare(expected.to_string())) {
        return None;
    }
    tokens.remove(0);
    Some(tokens)
}

/// A `true`/`false` reply value.
fn bool_value(token: &ReplyToken) -> Option<bool> {
    match token {
        ReplyToken::Bare(word) if word == "true" => Some(true),
        ReplyToken::Bare(word) if word == "false" => Some(false),
        _ => None,
    }
}

/// An unsigned 32-bit reply value.
fn u32_value(token: &ReplyToken) -> Option<u32> {
    match token {
        ReplyToken::Bare(word) => parse_component(word),
        ReplyToken::Quoted(_) => None,
    }
}

/// Parses the `NameHasOwner` reply (`b true` / `b false`).
pub fn parse_name_has_owner(text: &str) -> Option<bool> {
    match reply_values(text, "b")?.as_slice() {
        [value] => bool_value(value),
        _ => None,
    }
}

/// Parses a `SetDnd` reply (`b <new state>`); the same shape as
/// [`parse_name_has_owner`].
pub fn parse_bool_reply(text: &str) -> Option<bool> {
    parse_name_has_owner(text)
}

/// Parses the `GetServerInformation` reply (`ssss "notifd" "dotfiles" "1.1"
/// "1.3"`: name, vendor, version, spec version) into notifd's version — `None` when
/// the reply is malformed, the server is not notifd, or the version is not
/// `major.minor`.
pub fn parse_notifd_version(text: &str) -> Option<NotifdVersion> {
    match reply_values(text, "ssss")?.as_slice() {
        [
            ReplyToken::Quoted(name),
            ReplyToken::Quoted(_vendor),
            ReplyToken::Quoted(version),
            ReplyToken::Quoted(_spec),
        ] if name == NOTIFD_SERVER_NAME => NotifdVersion::parse(version),
        _ => None,
    }
}

/// Parses the `GetState` reply (`bubb false 22 false false`: dnd, count,
/// inhibited, center_open).
pub fn parse_get_state(text: &str) -> Option<NotifdState> {
    match reply_values(text, "bubb")?.as_slice() {
        [dnd, count, inhibited, center_open] => Some(NotifdState {
            dnd: bool_value(dnd)?,
            count: u32_value(count)?,
            inhibited: bool_value(inhibited)?,
            center_open: bool_value(center_open)?,
        }),
        _ => None,
    }
}

/// Parses notifd's `bar.json` (`{"count": 3, "dnd": false, "inhibited": false,
/// "center_open": false}`), the state source for a daemon older than 1.1.
///
/// Only those four fields are read; anything else in the object is ignored, and a
/// missing or wrongly typed field makes the whole state unknown. The file holds no
/// message text, but its contents are still never logged.
pub fn parse_bar_json(bytes: &[u8]) -> Option<NotifdState> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let object = value.as_object()?;
    let flag = |key: &str| object.get(key)?.as_bool();
    Some(NotifdState {
        dnd: flag("dnd")?,
        count: u32::try_from(object.get("count")?.as_u64()?).ok()?,
        inhibited: flag("inhibited")?,
        center_open: flag("center_open")?,
    })
}

/// Whether a `systemctl is-active` state word can only come from a unit systemd
/// knows. `inactive` (and `unknown`, or anything unrecognised) is excluded because
/// systemd reports it for units that do not exist as well.
fn unit_state_implies_installed(text: &str) -> bool {
    matches!(
        text.trim(),
        "active"
            | "reloading"
            | "activating"
            | "deactivating"
            | "refreshing"
            | "failed"
            | "maintenance"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::command::{CommandError, CommandOutput, MockCommandRunner};

    /// The reply of a running 1.1 daemon (verified in analysis §8.2).
    const SERVER_INFO_1_1: &str = "ssss \"notifd\" \"dotfiles\" \"1.1\" \"1.3\"\n";
    /// The reply of the first notifd release, which had no `GetState`/`Reload`.
    const SERVER_INFO_1_0: &str = "ssss \"notifd\" \"dotfiles\" \"1.0\" \"1.3\"\n";

    fn argv(command: &Command) -> Vec<String> {
        std::iter::once(command.program().to_string())
            .chain(command.args_slice().iter().cloned())
            .collect()
    }

    #[test]
    fn every_command_has_its_exact_argument_vector() {
        assert_eq!(
            argv(&name_has_owner_command()).join(" "),
            "busctl --user call org.freedesktop.DBus /org/freedesktop/DBus \
             org.freedesktop.DBus NameHasOwner s se.dotfiles.Notifd"
        );
        assert_eq!(
            argv(&server_information_command()).join(" "),
            "busctl --user --auto-start=no call org.freedesktop.Notifications \
             /org/freedesktop/Notifications org.freedesktop.Notifications GetServerInformation"
        );
        assert_eq!(
            argv(&unit_is_active_command()),
            ["systemctl", "--user", "is-active", "notifd.service"]
        );
        let prefix = "busctl --user --auto-start=no call se.dotfiles.Notifd \
                      /se/dotfiles/Notifd se.dotfiles.Notifd";
        for (method, tail) in [
            (NotifdMethod::GetState, "GetState"),
            (NotifdMethod::SetDnd(true), "SetDnd s on"),
            (NotifdMethod::SetDnd(false), "SetDnd s off"),
            (NotifdMethod::Clear, "Clear"),
            (NotifdMethod::Reload, "Reload"),
        ] {
            assert_eq!(
                argv(&control_command(method)).join(" "),
                format!("{prefix} {tail}")
            );
        }
    }

    /// Every [`NotifdMethod`] value, `SetDnd` with both arguments, built as a chain in
    /// which each value's `match` arm names the next one.
    ///
    /// The match has one arm per variant and no wildcard, so a variant added to the
    /// enum does not compile here until it gets an arm — and an arm only makes sense
    /// with a link to its successor, which puts the new value on the list the tests
    /// below check (the `SetCenter` guard among them).
    fn every_method() -> Vec<NotifdMethod> {
        let mut methods = Vec::new();
        let mut next = Some(NotifdMethod::GetState);
        while let Some(method) = next {
            methods.push(method);
            next = match method {
                NotifdMethod::GetState => Some(NotifdMethod::SetDnd(true)),
                NotifdMethod::SetDnd(true) => Some(NotifdMethod::SetDnd(false)),
                NotifdMethod::SetDnd(false) => Some(NotifdMethod::Clear),
                NotifdMethod::Clear => Some(NotifdMethod::Reload),
                NotifdMethod::Reload => None,
            };
        }
        methods
    }

    /// Every control call the app can make — one per value of [`every_method`].
    fn every_control_command() -> Vec<Command> {
        every_method().into_iter().map(control_command).collect()
    }

    #[test]
    fn no_control_command_ever_sets_the_center_flag() {
        // Task 13.1: `SetCenter` only records notifd's "center open" flag (which mutes
        // popups) and never opens the eww window, so the flag must only ever be set by
        // the eww `notif-center` script, after its window exists. No call the app can
        // build may name it.
        for command in every_control_command() {
            assert!(
                command
                    .args_slice()
                    .iter()
                    .all(|arg| !arg.contains("SetCenter")),
                "{command}"
            );
        }
    }

    #[test]
    fn every_call_to_a_notifd_name_disables_auto_start() {
        // Never start the daemon we are probing: every command addressed to a notifd
        // name carries --auto-start=no; only the bus-driver query may omit it.
        let notifd_calls = [
            server_information_command(),
            control_command(NotifdMethod::GetState),
            control_command(NotifdMethod::SetDnd(true)),
            control_command(NotifdMethod::Clear),
            control_command(NotifdMethod::Reload),
        ];
        for command in notifd_calls {
            assert!(
                command.args_slice().iter().any(|a| a == "--auto-start=no"),
                "{command} must not auto-start"
            );
        }
        assert!(
            name_has_owner_command()
                .args_slice()
                .contains(&"org.freedesktop.DBus".to_string())
        );
    }

    #[test]
    fn versions_parse_as_major_minor() {
        assert_eq!(
            NotifdVersion::parse("1.1"),
            Some(NotifdVersion { major: 1, minor: 1 })
        );
        assert_eq!(
            NotifdVersion::parse("2.0.3"),
            Some(NotifdVersion { major: 2, minor: 0 })
        );
        for bad in [
            "", "1", "1.", ".1", "1.1-dev", "v1.1", "1.x", "1..1", " 1.1",
        ] {
            assert_eq!(NotifdVersion::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn versions_compare_numerically() {
        let v = |text| NotifdVersion::parse(text).expect("valid version");
        assert!(v("1.10") > v("1.9"));
        assert!(!v("1.0").supports_state_and_reload());
        assert!(v("1.1").supports_state_and_reload());
        assert!(v("2.0").supports_state_and_reload());
        assert_eq!(v("1.1").to_string(), "1.1");
    }

    #[test]
    fn get_state_replies_parse_strictly() {
        assert_eq!(
            parse_get_state("bubb false 22 false false\n"),
            Some(NotifdState {
                dnd: false,
                count: 22,
                inhibited: false,
                center_open: false
            })
        );
        assert_eq!(
            parse_get_state("bubb true 0 true true"),
            Some(NotifdState {
                dnd: true,
                count: 0,
                inhibited: true,
                center_open: true
            })
        );
        for malformed in [
            "",
            "bubb",
            "bubb false 22 false",
            "bubb false 22 false false extra",
            "bub false 22 false",
            "bubb 0 22 false false",
            "bubb false -1 false false",
            "bubb false 4294967296 false false",
            "bubb false \"22\" false false",
            "bubb FALSE 22 false false",
            "Call failed: The name is not activatable",
            "bubb false 22 false false\nbubb true 1 true true",
        ] {
            assert_eq!(parse_get_state(malformed), None, "{malformed:?}");
        }
    }

    #[test]
    fn server_information_yields_the_version_only_for_notifd() {
        assert_eq!(
            parse_notifd_version(SERVER_INFO_1_1),
            Some(NotifdVersion::STATE_AND_RELOAD)
        );
        assert_eq!(
            parse_notifd_version(SERVER_INFO_1_0),
            Some(NotifdVersion { major: 1, minor: 0 })
        );
        // Another server on the standard name, a malformed reply, a bad version.
        for other in [
            "ssss \"other-daemon\" \"someone\" \"0.12.6\" \"1.2\"",
            "ssss \"notifd\" \"dotfiles\" \"1.1\"",
            "ssss notifd dotfiles 1.1 1.3",
            "ssss \"notifd\" \"dotfiles\" \"one\" \"1.3\"",
            "ssss \"notifd\" \"dotfiles\" \"1.1\" \"1.3",
            "ssss \"notifd\"x \"dotfiles\" \"1.1\" \"1.3\"",
            "ssss \"not\\nifd\" \"dotfiles\" \"1.1\" \"1.3\"",
        ] {
            assert_eq!(parse_notifd_version(other), None, "{other:?}");
        }
        // Escaped quotes and backslashes are decoded, and still compared exactly.
        assert_eq!(
            parse_notifd_version("ssss \"notifd\" \"a \\\"b\\\\\" \"1.1\" \"1.3\""),
            Some(NotifdVersion::STATE_AND_RELOAD)
        );
    }

    #[test]
    fn boolean_replies_parse_strictly() {
        assert_eq!(parse_name_has_owner("b true\n"), Some(true));
        assert_eq!(parse_name_has_owner("b false"), Some(false));
        assert_eq!(parse_bool_reply("b true"), Some(true));
        for malformed in ["", "b", "b yes", "s \"true\"", "b true false", "true"] {
            assert_eq!(parse_name_has_owner(malformed), None, "{malformed:?}");
        }
    }

    #[test]
    fn bar_json_yields_only_the_four_state_fields() {
        let bar = br#"{"count":3,"dnd":true,"inhibited":false,"center_open":false}"#;
        assert_eq!(
            parse_bar_json(bar),
            Some(NotifdState {
                dnd: true,
                count: 3,
                inhibited: false,
                center_open: false
            })
        );
        // Extra fields are ignored; a missing or mistyped one makes the state unknown.
        assert!(
            parse_bar_json(br#"{"count":0,"dnd":false,"inhibited":true,"center_open":true,"x":1}"#)
                .is_some()
        );
        for bad in [
            &br#"{"count":3,"dnd":true,"inhibited":false}"#[..],
            br#"{"count":"3","dnd":true,"inhibited":false,"center_open":false}"#,
            br#"{"count":-1,"dnd":true,"inhibited":false,"center_open":false}"#,
            br#"{"count":3,"dnd":1,"inhibited":false,"center_open":false}"#,
            br#"[1,2]"#,
            b"",
            b"{not json",
        ] {
            assert_eq!(parse_bar_json(bad), None);
        }
    }

    fn replies(owner: Option<&str>, info: Option<&str>, active: Option<&str>) -> NotifdReplies {
        NotifdReplies {
            name_has_owner: owner.map(str::to_string),
            server_information: info.map(str::to_string),
            unit_active: active.map(str::to_string),
        }
    }

    #[test]
    fn status_resolution_covers_every_daemon_state() {
        // Running 1.1 and 1.0.
        let running_1_1 = NotifdStatus::resolve(
            &replies(Some("b true\n"), Some(SERVER_INFO_1_1), None),
            true,
        );
        assert_eq!(
            running_1_1,
            NotifdStatus::Running {
                version: Some(NotifdVersion::STATE_AND_RELOAD)
            }
        );
        assert!(running_1_1.is_present() && running_1_1.supports_state_and_reload());

        let running_1_0 = NotifdStatus::resolve(
            &replies(Some("b true\n"), Some(SERVER_INFO_1_0), None),
            true,
        );
        assert_eq!(running_1_0.version(), NotifdVersion::parse("1.0"));
        assert!(running_1_0.is_present() && !running_1_0.supports_state_and_reload());

        // Running, but the version query failed or was malformed: still running, and
        // treated as older than 1.1.
        let unversioned =
            NotifdStatus::resolve(&replies(Some("b true"), Some("garbage"), None), false);
        assert_eq!(unversioned, NotifdStatus::Running { version: None });
        assert!(!unversioned.supports_state_and_reload());

        // Installed but stopped: no owner, inactive unit, unit file present.
        let stopped = replies(Some("b false\n"), None, Some("inactive\n"));
        assert_eq!(
            NotifdStatus::resolve(&stopped, true),
            NotifdStatus::Installed
        );
        // ...without the unit file, `inactive` proves nothing.
        assert_eq!(NotifdStatus::resolve(&stopped, false), NotifdStatus::Absent);
        // A unit systemd knows (failed, activating) counts as installed even without
        // the file in ~/.config.
        for state in ["failed\n", "activating\n", "active\n"] {
            assert_eq!(
                NotifdStatus::resolve(&replies(Some("b false"), None, Some(state)), false),
                NotifdStatus::Installed,
                "{state:?}"
            );
        }

        // Absent: nothing anywhere.
        let absent =
            NotifdStatus::resolve(&replies(Some("b false"), None, Some("inactive")), false);
        assert_eq!(absent, NotifdStatus::Absent);
        assert!(!absent.is_present());

        // A failed or malformed bus query is Absent even with the unit installed: no
        // control call could work without the bus.
        assert_eq!(
            NotifdStatus::resolve(&replies(None, None, None), true),
            NotifdStatus::Absent
        );
        assert_eq!(
            NotifdStatus::resolve(&replies(Some("Failed to connect to bus"), None, None), true),
            NotifdStatus::Absent
        );
    }

    #[test]
    fn gather_asks_the_version_only_when_the_name_is_owned() {
        let running = MockCommandRunner::with_outcomes([
            Ok(CommandOutput::fake_with_streams(0, "b true\n", "")),
            Ok(CommandOutput::fake_with_streams(0, SERVER_INFO_1_1, "")),
        ]);
        let gathered = NotifdReplies::gather(&running);
        assert_eq!(
            running.recorded(),
            vec![name_has_owner_command(), server_information_command()]
        );
        assert_eq!(
            gathered,
            replies(Some("b true\n"), Some(SERVER_INFO_1_1), None)
        );
    }

    #[test]
    fn gather_asks_systemd_only_when_the_name_is_not_owned() {
        // `is-active` exits 3 for an inactive unit; its word is still captured.
        let stopped = MockCommandRunner::with_outcomes([
            Ok(CommandOutput::fake_with_streams(0, "b false\n", "")),
            Ok(CommandOutput::fake_with_streams(3, "inactive\n", "")),
        ]);
        let gathered = NotifdReplies::gather(&stopped);
        assert_eq!(
            stopped.recorded(),
            vec![name_has_owner_command(), unit_is_active_command()]
        );
        assert_eq!(
            gathered,
            replies(Some("b false\n"), None, Some("inactive\n"))
        );
    }

    #[test]
    fn gather_stops_after_a_failed_or_malformed_bus_query() {
        for outcome in [
            Ok(CommandOutput::fake_with_streams(
                1,
                "",
                "Failed to connect to bus",
            )),
            Ok(CommandOutput::fake_with_streams(0, "huh\n", "")),
            Err(CommandError::Spawn(std::io::Error::from(
                std::io::ErrorKind::NotFound,
            ))),
        ] {
            let runner = MockCommandRunner::with_outcomes([outcome]);
            let gathered = NotifdReplies::gather(&runner);
            assert_eq!(runner.recorded(), vec![name_has_owner_command()]);
            assert_eq!(gathered.server_information, None);
            assert_eq!(gathered.unit_active, None);
            assert_eq!(NotifdStatus::resolve(&gathered, true), NotifdStatus::Absent);
        }
    }

    #[test]
    fn no_command_ever_names_the_replaced_daemon() {
        // The predecessor's client tool would D-Bus-activate the old daemon; nothing
        // here may ever invoke it (analysis §7.5).
        let all = [
            name_has_owner_command(),
            server_information_command(),
            unit_is_active_command(),
            control_command(NotifdMethod::GetState),
            control_command(NotifdMethod::SetDnd(false)),
            control_command(NotifdMethod::Clear),
            control_command(NotifdMethod::Reload),
        ];
        for command in all {
            assert!(!command.to_string().contains("sway"), "{command}");
        }
    }
}
