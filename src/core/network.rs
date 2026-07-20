//! GTK-free Network-page domain model (task 6.9; architecture §7 Network; R3.1,
//! R4.2, R6.2).
//!
//! # What this module is
//!
//! The Network page is **read-only** in v1 (requirements §3): it shows which
//! NetworkManager connections are currently active and offers exactly one action —
//! an "Open Network Settings" button that hands deep management to the proper
//! NetworkManager tool. This module is the headless half: it reads and parses the
//! connection status ([`read_status`]) and decides and builds the launcher command
//! ([`launcher`], [`open_settings`]). The bespoke GTK glue in [`crate::ui::network`]
//! renders the status rows and the button from what this module returns.
//!
//! Like the Sound page (task 6.2) it is runtime-backed (R3.1): there is no dotfile
//! behind it, nothing is staged, nothing is ever dirty, and the
//! [`SettingsStore`](crate::core::store) and Apply pipeline are never involved. It
//! stays GTK-free so the parsing and command building are unit-tested headlessly
//! against a [`MockCommandRunner`](crate::system::command::MockCommandRunner)
//! (R6.2); the layering guard in `tests/module_boundaries.rs` forbids any
//! `gtk`/`relm4` import here.
//!
//! # Why `nmcli -t -f NAME,TYPE,DEVICE connection show --active`
//!
//! The status source is the active-connection list, chosen over the alternatives
//! for three reasons. First, it answers the question a settings page is actually
//! asked — *which network am I on?* — by connection name, covering Wi-Fi, wired,
//! VPN, and WireGuard uniformly, where `nmcli general`'s one-word state
//! (`connected`) says nothing about *what* is connected and `nmcli device` reports
//! per-interface plumbing rather than user-named connections. Second, `-t` (terse)
//! is nmcli's machine-readable contract: colon-separated, locale-independent, and
//! documented as the mode for scripting, so the output shape does not shift under
//! translation or cosmetic table changes the way the pretty output can. Third,
//! restricting `-f` to `NAME,TYPE,DEVICE` pins the exact columns the page renders,
//! so a future nmcli that grows or reorders default columns changes nothing here.
//!
//! # Launching the management tool, detached
//!
//! Deep management is delegated (requirements §3, R3.1): to `nm-connection-editor`
//! when installed — a native GUI, so it is preferred — else to `nmtui` inside a
//! `kitty` terminal (`nmtui` needs a terminal to run in, and `nm-applet` is already
//! autostarted, so a bare re-launch of either is not viable). Both are long-running
//! interactive programs, so they must not be spawned like a reload command:
//! [`CommandRunner::run`] waits for the child (with the 5 s timeout). The launch
//! therefore goes through `setsid --fork <tool>…`, marked
//! [detached](crate::system::command::Command::detached) — the same mechanism the
//! hypridle respawn uses (see [`crate::core::reload`]). Two halves make the detach
//! actually work: `setsid --fork` forks the tool into its own new session and exits
//! immediately, so `run` reaps `setsid` at once with no zombie while the tool runs
//! on, independent of this app's lifetime; and the *detached* marker makes the
//! runner discard the output streams instead of capturing them — without it the
//! forked tool would inherit the capture pipes' write ends and `run` would sit
//! draining them (blocking the GTK main thread) until the tool exits, freezing the
//! GUI for the tool's whole lifetime despite `setsid` having returned instantly.

use crate::core::detect::{Binary, Capabilities};
use crate::system::command::{Command, CommandRunner};

/// The `TYPE` value NetworkManager reports for the loopback connection.
///
/// NetworkManager ≥ 1.42 manages `lo` itself and lists it as a permanently active
/// connection. It is not a network the user is "on", so showing it would be pure
/// noise — [`parse_active_connections`] filters it out.
const LOOPBACK_TYPE: &str = "loopback";

/// One active NetworkManager connection as shown on the Network page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ActiveConnection {
    /// The user-facing connection name (`NAME`, e.g. the Wi-Fi SSID profile name).
    name: String,
    /// The raw NetworkManager connection type (`TYPE`, e.g. `802-11-wireless`);
    /// rendered through [`Self::kind_label`].
    kind: String,
    /// The interface the connection is active on (`DEVICE`, e.g. `wlan0`). May be
    /// empty for a connection without a bound device.
    device: String,
}

impl ActiveConnection {
    /// The user-facing connection name.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// A human-readable label for the connection type: the common NetworkManager
    /// type identifiers are mapped to plain words (`802-11-wireless` → "Wi-Fi"),
    /// and an unrecognised type is shown as-is rather than hidden — a raw
    /// identifier is still more informative than nothing.
    pub(crate) fn kind_label(&self) -> &str {
        match self.kind.as_str() {
            "802-11-wireless" => "Wi-Fi",
            "802-3-ethernet" => "Ethernet",
            "vpn" => "VPN",
            "wireguard" => "WireGuard",
            "bluetooth" => "Bluetooth",
            other => other,
        }
    }

    /// The interface the connection is active on; may be empty.
    pub(crate) fn device(&self) -> &str {
        &self.device
    }
}

/// The Network page's read-only status: what [`read_status`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NetworkStatus {
    /// `nmcli` ran successfully and reported these active connections (possibly
    /// none — a machine that is simply offline).
    Connections(Vec<ActiveConnection>),
    /// The status could not be read: `nmcli` could not be run or exited non-zero
    /// (most commonly the NetworkManager daemon is not running). Distinct from an
    /// empty connection list so the page can say "could not read" rather than the
    /// misleading "no active connections".
    Unavailable,
}

/// Reads the current connection status from NetworkManager (R3.1).
///
/// Runs [`status_command`] and parses its terse output; run on page entry and on a
/// manual status refresh. Any failure — `nmcli` missing (it gates the page, but it
/// can disappear between detection and now), a non-zero exit (NetworkManager daemon
/// not running) — degrades to [`NetworkStatus::Unavailable`], never a panic.
pub(crate) fn read_status(runner: &dyn CommandRunner) -> NetworkStatus {
    match runner.run(&status_command()) {
        Ok(output) if output.success() => NetworkStatus::Connections(parse_active_connections(
            &String::from_utf8_lossy(output.stdout()),
        )),
        Ok(output) => {
            tracing::info!(
                code = ?output.code(),
                "nmcli exited non-zero; NetworkManager is likely not running — network status \
                 unavailable (R3.1)"
            );
            NetworkStatus::Unavailable
        }
        Err(error) => {
            tracing::info!(%error, "could not run nmcli; network status unavailable (R3.1)");
            NetworkStatus::Unavailable
        }
    }
}

/// The exact status query the page runs — see the module docs for why these
/// fields and the terse mode were chosen.
fn status_command() -> Command {
    Command::new("nmcli").args([
        "-t",
        "-f",
        "NAME,TYPE,DEVICE",
        "connection",
        "show",
        "--active",
    ])
}

/// Parses terse `nmcli` active-connection output into [`ActiveConnection`]s.
///
/// One connection per line, fields colon-separated with nmcli's terse escaping
/// (see [`split_terse_fields`]). The loopback connection is filtered out (see
/// [`LOOPBACK_TYPE`]); a line without exactly the three requested fields is not a
/// connection record and is skipped rather than panicked on.
fn parse_active_connections(text: &str) -> Vec<ActiveConnection> {
    let mut connections = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let fields = split_terse_fields(line);
        let [name, kind, device] = fields.as_slice() else {
            tracing::debug!(
                fields = fields.len(),
                "skipping an nmcli line that is not a three-field connection record"
            );
            continue;
        };
        if kind == LOOPBACK_TYPE {
            continue;
        }
        connections.push(ActiveConnection {
            name: name.clone(),
            kind: kind.clone(),
            device: device.clone(),
        });
    }
    connections
}

/// Splits one terse `nmcli -t` line into its fields, honouring nmcli's escaping.
///
/// In terse mode fields are separated by `:`, and a literal `:` or `\` inside a
/// value (e.g. in a connection name) is escaped with a backslash — so a plain
/// `split(':')` would break such names apart. A trailing lone backslash (which
/// nmcli never emits) is kept literally rather than dropped.
fn split_terse_fields(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(escaped) => current.push(escaped),
                None => current.push('\\'),
            },
            ':' => fields.push(std::mem::take(&mut current)),
            other => current.push(other),
        }
    }
    fields.push(current);
    fields
}

/// Which tool the "Open Network Settings" button launches (task 6.9, R3.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Launcher {
    /// `nm-connection-editor`, NetworkManager's native GUI — preferred when
    /// installed, since it needs no terminal and matches a GUI settings app.
    ConnectionEditor,
    /// `nmtui` inside a kitty terminal — the fallback when the GUI editor is not
    /// installed. `nmtui` is a TUI, so it must be given a terminal to run in.
    KittyNmtui,
}

impl Launcher {
    /// A short user-facing description of what the button will open, for the
    /// button's tooltip.
    pub(crate) fn description(self) -> &'static str {
        match self {
            Launcher::ConnectionEditor => "Opens nm-connection-editor",
            Launcher::KittyNmtui => "Opens nmtui in a kitty terminal",
        }
    }
}

/// Decides which launcher the "Open Network Settings" button uses, or `None` when
/// the button must be hidden because neither tool is available (R4.2).
///
/// Preference order: `nm-connection-editor` when installed (the native GUI), else
/// `kitty` + `nmtui`. The hidden-button case is logged at `info`, matching the
/// hidden-item convention of the rest of detection-driven visibility.
pub(crate) fn launcher(capabilities: &Capabilities) -> Option<Launcher> {
    if capabilities.has_binary(Binary::NmConnectionEditor) {
        Some(Launcher::ConnectionEditor)
    } else if capabilities.has_binary(Binary::Kitty) {
        Some(Launcher::KittyNmtui)
    } else {
        tracing::info!(
            "neither nm-connection-editor nor kitty is installed; the Open Network Settings \
             button is hidden (R4.2)"
        );
        None
    }
}

/// The detached launch command for `launcher` — `setsid --fork <tool>…`, marked
/// [detached](Command::detached) so the runner captures nothing and returns as
/// soon as `setsid` exits while the interactive tool outlives the
/// [`CommandRunner::run`] call (see the module docs).
fn launch_command(launcher: Launcher) -> Command {
    match launcher {
        Launcher::ConnectionEditor => Command::new("setsid")
            .args(["--fork", "nm-connection-editor"])
            .detached(),
        Launcher::KittyNmtui => Command::new("setsid")
            .args(["--fork", "kitty", "-e", "nmtui"])
            .detached(),
    }
}

/// Launches the network-management tool, detached (task 6.9, R3.1).
///
/// The launch is fire-and-forget by design: the tool is an independent
/// interactive program, so nothing here observes its lifetime. Note the limits of
/// what exit 0 proves — `setsid --fork` exits 0 once it has *forked*, before (and
/// regardless of whether) the detached child manages to exec the tool, so the
/// success log below confirms only that the hand-off happened; a tool that is
/// present at detection time but fails to start in the detached session is not
/// observable from here. A failure to launch `setsid` itself is logged at
/// `error`; it is non-fatal — nothing on disk or in the session changed.
pub(crate) fn open_settings(runner: &dyn CommandRunner, launcher: Launcher) {
    let command = launch_command(launcher);
    match runner.run(&command) {
        Ok(output) if output.success() => {
            tracing::info!(%command, "handed the network settings tool to setsid, detached");
        }
        Ok(output) => {
            tracing::error!(%command, code = ?output.code(), "network settings launch failed");
        }
        Err(error) => {
            tracing::error!(%command, %error, "could not launch the network settings tool");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::command::{CommandError, CommandOutput, MockCommandRunner};

    /// A realistic terse dump: a Wi-Fi connection whose name contains an escaped
    /// colon, a wired connection, a device-less VPN, and the loopback record modern
    /// NetworkManager always lists (which must be filtered out).
    const NMCLI_ACTIVE: &str = "Cafe Wi\\:Fi:802-11-wireless:wlan0\n\
                                Wired connection 1:802-3-ethernet:enp0s31f6\n\
                                work-vpn:vpn:\n\
                                lo:loopback:lo\n";

    #[test]
    fn status_command_is_the_exact_terse_nmcli_query() {
        // The pinned machine-readable query (see the module docs for the choice).
        assert_eq!(
            status_command(),
            Command::new("nmcli").args([
                "-t",
                "-f",
                "NAME,TYPE,DEVICE",
                "connection",
                "show",
                "--active"
            ]),
        );
    }

    #[test]
    fn parse_reads_connections_unescapes_names_and_filters_loopback() {
        let connections = parse_active_connections(NMCLI_ACTIVE);

        // Three user-facing connections; the loopback record is filtered out.
        assert_eq!(connections.len(), 3, "loopback must not be listed");

        let wifi = &connections[0];
        assert_eq!(
            wifi.name(),
            "Cafe Wi:Fi",
            "the terse `\\:` escape must be decoded back to a literal colon"
        );
        assert_eq!(wifi.kind_label(), "Wi-Fi");
        assert_eq!(wifi.device(), "wlan0");

        let wired = &connections[1];
        assert_eq!(wired.name(), "Wired connection 1");
        assert_eq!(wired.kind_label(), "Ethernet");
        assert_eq!(wired.device(), "enp0s31f6");

        // A VPN may carry no bound device; the empty field survives as-is.
        let vpn = &connections[2];
        assert_eq!(vpn.name(), "work-vpn");
        assert_eq!(vpn.kind_label(), "VPN");
        assert_eq!(vpn.device(), "");
    }

    #[test]
    fn parse_skips_lines_that_are_not_three_field_records() {
        // Defensive: anything that is not a NAME:TYPE:DEVICE record (a stray
        // diagnostic, a truncated line) is skipped, never panicked on.
        let connections =
            parse_active_connections("garbage\nonly:two\nhome:802-3-ethernet:eth0\n\n");
        assert_eq!(connections.len(), 1);
        assert_eq!(connections[0].name(), "home");
    }

    #[test]
    fn unknown_connection_type_falls_back_to_the_raw_identifier() {
        // A type this module has no plain word for is shown raw rather than hidden.
        let connections = parse_active_connections("modem:gsm:cdc-wdm0\n");
        assert_eq!(connections[0].kind_label(), "gsm");
    }

    #[test]
    fn split_terse_fields_honours_backslash_escapes() {
        assert_eq!(
            split_terse_fields(r"a\:b:c\\d:e"),
            vec!["a:b".to_string(), r"c\d".to_string(), "e".to_string()],
            "escaped colons stay in the value; escaped backslashes decode; plain colons split"
        );
    }

    #[test]
    fn read_status_parses_a_successful_nmcli_run() {
        // Accept criterion (status renders, via the mock runner, R6.1): a successful
        // nmcli run yields the parsed connections, and exactly the one pinned query
        // was issued.
        let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake_with_streams(
            0,
            NMCLI_ACTIVE,
            "",
        ))]);
        let status = read_status(&runner);
        match status {
            NetworkStatus::Connections(connections) => assert_eq!(connections.len(), 3),
            NetworkStatus::Unavailable => panic!("a successful nmcli run must yield connections"),
        }
        assert_eq!(runner.recorded(), vec![status_command()]);
    }

    #[test]
    fn read_status_degrades_to_unavailable_on_failure() {
        // nmcli exits non-zero (NetworkManager daemon not running).
        let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake(8))]);
        assert_eq!(read_status(&runner), NetworkStatus::Unavailable);

        // nmcli cannot be spawned at all.
        let runner = MockCommandRunner::with_outcomes([Err(CommandError::Spawn(
            std::io::Error::from(std::io::ErrorKind::NotFound),
        ))]);
        assert_eq!(read_status(&runner), NetworkStatus::Unavailable);
    }

    #[test]
    fn launcher_prefers_the_gui_editor_then_kitty_then_hides() {
        // Both tools installed -> the native GUI wins.
        let both = Capabilities::for_tests(
            &[Binary::Nmcli, Binary::NmConnectionEditor, Binary::Kitty],
            &[],
            false,
        );
        assert_eq!(launcher(&both), Some(Launcher::ConnectionEditor));

        // Editor only.
        let editor_only =
            Capabilities::for_tests(&[Binary::Nmcli, Binary::NmConnectionEditor], &[], false);
        assert_eq!(launcher(&editor_only), Some(Launcher::ConnectionEditor));

        // kitty only -> the nmtui fallback.
        let kitty_only = Capabilities::for_tests(&[Binary::Nmcli, Binary::Kitty], &[], false);
        assert_eq!(launcher(&kitty_only), Some(Launcher::KittyNmtui));

        // Neither -> the button is hidden (R4.2). nmcli alone keeps the page
        // visible but offers no launcher.
        let neither = Capabilities::for_tests(&[Binary::Nmcli], &[], false);
        assert_eq!(launcher(&neither), None);
    }

    #[test]
    fn open_settings_spawns_the_expected_detached_command_per_capability() {
        // Accept criterion (button spawns the expected command per available
        // capability, mock runner): drive the full decision + launch for each
        // capability combination and assert the exact setsid arg vector. The
        // expected commands carry the detached marker — equality includes it, so
        // this also proves the launch goes through the runner's no-capture mode
        // (the S1 review fix: a capturing launch would freeze the GUI until the
        // tool exits).
        let editor_caps =
            Capabilities::for_tests(&[Binary::Nmcli, Binary::NmConnectionEditor], &[], false);
        let runner = MockCommandRunner::new();
        open_settings(
            &runner,
            launcher(&editor_caps).expect("the editor capability yields a launcher"),
        );
        assert_eq!(
            runner.recorded(),
            vec![
                Command::new("setsid")
                    .args(["--fork", "nm-connection-editor"])
                    .detached()
            ],
        );

        let kitty_caps = Capabilities::for_tests(&[Binary::Nmcli, Binary::Kitty], &[], false);
        let runner = MockCommandRunner::new();
        open_settings(
            &runner,
            launcher(&kitty_caps).expect("the kitty capability yields a launcher"),
        );
        assert_eq!(
            runner.recorded(),
            vec![
                Command::new("setsid")
                    .args(["--fork", "kitty", "-e", "nmtui"])
                    .detached()
            ],
        );
    }
}
