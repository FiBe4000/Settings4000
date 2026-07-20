//! Installed-app detection and dynamic visibility (task 4.3; architecture §4;
//! R4.1–R4.4, R2.2, R3.2, R8.5, R6.1, R7.3).
//!
//! # What this module is
//!
//! At startup the app must decide which settings to show. A page or row is shown
//! only when the thing it edits is actually present — the compositor is running,
//! the daemon it reloads is installed, the config it writes is readable, the
//! dotfiles palette source exists. This module runs those probes once and returns
//! a plain [`Capabilities`] value that the UI (tasks 5.x/6.x) consults to hide the
//! rows/pages it cannot support (R4.2), that the reload table (task 4.4) consults
//! to skip a reload for a component that is not running, and that the Apply
//! pipeline (task 4.5) consults for the palette source's repo root (R8.5).
//!
//! It lives in `core/` because it is pure domain logic driven by injected inputs
//! (see below), so it is headlessly testable (R6.2) and the layering guard in
//! `tests/module_boundaries.rs` forbids any `gtk`/`relm4` import.
//!
//! # Why detection does *not* use `CommandRunner`
//!
//! Unlike reloads (which spawn processes and therefore go through
//! [`crate::system::command`]), detection only *reads* the environment,
//! filesystem, procfs, and socket paths. Reading is not a process side effect, so
//! there is no shell and no subprocess here: the binary scan is a manual `$PATH`
//! walk (a `which`-equivalent, architecture §4), daemon liveness is a procfs scan,
//! and everything else is a filesystem stat/read. Avoiding a `which` subprocess
//! per binary also keeps cold start inside the budget (R8.1).
//!
//! # The injectable seam (R6.1)
//!
//! Every probe is driven by a [`DetectionInputs`] value rather than by reaching
//! for the live system directly, so tests can exercise each branch deterministically
//! without a running Hyprland, portal, or daemon:
//!
//! - the binary scan takes a `$PATH` **string** ([`DetectionInputs::path`]), so a
//!   test points it at a temp directory holding (or not holding) a fake executable;
//! - daemon liveness reads a supplied list of running process names
//!   ([`DetectionInputs::running_processes`]) — the real detector fills it from a
//!   procfs scan, a test injects a fixed set;
//! - Hyprland liveness and the palette anchor and config files are given as
//!   **paths** ([`DetectionInputs::hyprland_socket`],
//!   [`DetectionInputs::palette_config_anchor`], [`DetectionInputs::config_paths`]),
//!   which [`Capabilities::detect`] checks against the real filesystem — so a test
//!   builds a temp-dir fixture (a fake socket file, a config symlinked into a fake
//!   repo, an unreadable config) and gets faithful behavior.
//!
//! [`DetectionInputs::from_system`] gathers the real inputs (env `$PATH`, a procfs
//! scan, the `$XDG_RUNTIME_DIR` socket path, the XDG config anchor);
//! [`Capabilities::detect`] is the pure, re-runnable routine over any inputs. A
//! manual refresh (R4.3) simply re-gathers the inputs and calls it again.
//!
//! # Everything degrades to "absent" (R4.3, R4.4)
//!
//! No probe failure ever aborts detection or startup: a missing `$PATH`, an
//! unreadable `/proc`, a dangling symlink, an unreadable config — each simply
//! yields the "absent" form of its capability. This is why the routine returns a
//! plain value and never a `Result`.
//!
//! # Logging (R4.2, R7.3)
//!
//! Absent page-gating binaries, an absent palette source, and an absent settings
//! portal are logged at `info` (the "hidden item" signal, R4.2); a missing or
//! unreadable config is logged at `warn` (R4.4); a one-line detection summary is
//! logged at `info` (R7.3). File *contents* are never logged.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// An external program the app scans for on `$PATH`.
///
/// Each variant is a binary whose presence gates a feature: the reload/query
/// tools (`hyprctl`, `gsettings`, `pactl`/`wpctl`, `nmcli`) and the daemon
/// binaries whose pages the app offers. [`Binary::command_name`] is the file name
/// looked up in each `$PATH` directory.
///
/// [`Binary::Dconf`] is scanned only as a proxy for the dconf GSettings backend
/// (see [`Capabilities::settings_portal_available`]); it does not gate a page of
/// its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Binary {
    /// Hyprland's control client — needed by nearly every live reload
    /// (`hyprctl reload`, `hyprctl setcursor`, `hyprctl hyprpaper …`).
    Hyprctl,
    /// The GSettings CLI, used to read/write `org.gnome.desktop.interface`
    /// theme keys (R3.3/R3.4).
    Gsettings,
    /// PulseAudio/PipeWire-pulse control client (Sound page fallback, R3.1).
    Pactl,
    /// WirePlumber control client (the preferred Sound backend, R3.1).
    Wpctl,
    /// The kitty terminal — reloaded via SIGUSR1 on a palette change (task 4.4)
    /// and launched for `nmtui` on the Network page (R3.1).
    Kitty,
    /// The eww bar daemon — reloaded via `eww reload`.
    Eww,
    /// The swaync notification daemon (Notifications page, task 6.7).
    Swaync,
    /// The hyprpaper wallpaper daemon (Wallpaper controls, task 6.5).
    Hyprpaper,
    /// The hypridle idle daemon (Power & Idle page, task 6.8).
    Hypridle,
    /// The hyprlock screen locker (Lock-background controls, task 6.5).
    Hyprlock,
    /// NetworkManager's CLI — gates the read-only Network page (R3.1).
    Nmcli,
    /// NetworkManager's GUI connection editor — the preferred tool behind the
    /// Network page's "Open Network Settings" button (task 6.9, R3.1). It gates
    /// only that button's *choice* of tool, never a page: when absent the button
    /// falls back to `kitty -e nmtui`, and only with kitty also absent is the
    /// button itself hidden.
    NmConnectionEditor,
    /// The dconf CLI, taken as a proxy for the dconf GSettings backend that lets
    /// GTK pick up a live theme change (R2.2). Scanned only for
    /// [`Capabilities::settings_portal_available`]; it gates no page directly.
    Dconf,
}

impl Binary {
    /// Every binary the detector scans for, in a fixed order.
    const ALL: &'static [Binary] = &[
        Binary::Hyprctl,
        Binary::Gsettings,
        Binary::Pactl,
        Binary::Wpctl,
        Binary::Kitty,
        Binary::Eww,
        Binary::Swaync,
        Binary::Hyprpaper,
        Binary::Hypridle,
        Binary::Hyprlock,
        Binary::Nmcli,
        Binary::NmConnectionEditor,
        Binary::Dconf,
    ];

    /// The executable file name looked up in each `$PATH` directory.
    pub(crate) fn command_name(self) -> &'static str {
        match self {
            Binary::Hyprctl => "hyprctl",
            Binary::Gsettings => "gsettings",
            Binary::Pactl => "pactl",
            Binary::Wpctl => "wpctl",
            Binary::Kitty => "kitty",
            Binary::Eww => "eww",
            Binary::Swaync => "swaync",
            Binary::Hyprpaper => "hyprpaper",
            Binary::Hypridle => "hypridle",
            Binary::Hyprlock => "hyprlock",
            Binary::Nmcli => "nmcli",
            Binary::NmConnectionEditor => "nm-connection-editor",
            Binary::Dconf => "dconf",
        }
    }
}

/// A long-running component whose *liveness* the app checks, so a reload is issued
/// only for a component that is actually running (architecture §6, task 4.4).
///
/// Liveness is decided by a procfs "pidof-equivalent" against
/// [`Daemon::process_name`]. Hyprland itself is intentionally absent here: its
/// liveness is the IPC socket, not a process-name match (see
/// [`Capabilities::hyprland_ipc_live`]). hyprlock is absent too — it runs only
/// while the screen is locked and receives no reload (its config is read at the
/// next lock), so a running check would be meaningless.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Daemon {
    /// The kitty terminal (reloaded via SIGUSR1).
    Kitty,
    /// The eww bar daemon (`eww reload`).
    Eww,
    /// The swaync notification daemon (`swaync-client -rs`).
    Swaync,
    /// The hyprpaper wallpaper daemon (`hyprctl hyprpaper …`).
    Hyprpaper,
    /// The hypridle idle daemon (restarted on config change).
    Hypridle,
}

impl Daemon {
    /// Every daemon whose liveness is probed, in a fixed order.
    const ALL: &'static [Daemon] = &[
        Daemon::Kitty,
        Daemon::Eww,
        Daemon::Swaync,
        Daemon::Hyprpaper,
        Daemon::Hypridle,
    ];

    /// The executable name (argv[0] basename, see [`scan_procfs_process_names`])
    /// that identifies a running instance of this daemon. All five are under 15
    /// bytes, so they would survive `/proc/comm` truncation too, but detection
    /// matches them against the untruncated name for uniformity with the portal.
    pub(crate) fn process_name(self) -> &'static str {
        match self {
            Daemon::Kitty => "kitty",
            Daemon::Eww => "eww",
            Daemon::Swaync => "swaync",
            Daemon::Hyprpaper => "hyprpaper",
            Daemon::Hypridle => "hypridle",
        }
    }
}

/// The settings-portal backends that implement `org.freedesktop.impl.portal.Settings`
/// — the interface that actually propagates a live `org.gnome.desktop.interface`
/// (GTK theme) change to running GTK apps, which is what the R2.2 live-restyle claim
/// depends on (see [`Capabilities::settings_portal_available`]).
///
/// Only these Settings-implementing backends count. The base `xdg-desktop-portal`
/// and the non-Settings backends common on a Hyprland session
/// (`xdg-desktop-portal-hyprland`, `-wlr`) are deliberately excluded: they do not
/// implement Settings, so their presence must not be taken as a live-restyle path.
///
/// # Why the full name matters (not `/proc/comm`)
///
/// Matching must use the **full, untruncated** executable name. The kernel caps
/// `/proc/<pid>/comm` at 15 bytes (`TASK_COMM_LEN - 1`), and every member of the
/// `xdg-desktop-portal-*` family — base, `-gtk`, `-gnome`, `-kde`, `-hyprland`,
/// `-wlr` — collapses to the same 15-byte prefix `xdg-desktop-por`. Matching that
/// prefix would report the Settings path as present whenever *any* portal runs
/// (e.g. the base + `-hyprland` that a Hyprland session normally starts), the exact
/// false positive R2.2 forbids. Detection therefore reads argv[0] from
/// `/proc/<pid>/cmdline`, which is not truncated (see [`scan_procfs_process_names`]).
const SETTINGS_PORTAL_BACKENDS: &[&str] = &[
    "xdg-desktop-portal-gtk",
    "xdg-desktop-portal-gnome",
    "xdg-desktop-portal-kde",
];

/// The repo-relative path of the config file used to discover the dotfiles repo
/// root (R8.5).
///
/// The deployed `~/.config/hypr/colors.conf` symlink resolves to
/// `<repo>/config/hypr/colors.conf` (analysis §1), so canonicalizing the anchor
/// and stripping these three trailing components yields the repo root. Keeping the
/// anchor's depth here — rather than a bare "strip three parents" — documents *why*
/// three, and lets [`detect_palette_source`] verify the resolved tail actually
/// matches before trusting the derived root.
const REPO_ANCHOR_RELATIVE: &[&str] = &["config", "hypr", "colors.conf"];

/// The dotfiles palette source located behind a deployed config symlink (R3.2,
/// R8.5).
///
/// Its presence gates the palette switcher (task 6.3); its [`repo_root`](Self::repo_root)
/// is the anchor tasks 3.7/6.3 use to enumerate schemes and task 4.5 uses to run
/// the generator. All three paths are held resolved so consumers never re-derive
/// them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PaletteSource {
    /// The dotfiles repository root (the canonicalized directory the deployed
    /// config symlink resolves into).
    repo_root: PathBuf,
    /// The `colors/` directory holding one file per scheme (task 6.3 enumerates
    /// it; task 3.7 reads a scheme's swatch from it).
    colors_dir: PathBuf,
    /// The `scripts/generate-colors` generator the Apply pipeline runs on a
    /// palette change (task 4.5).
    generate_colors: PathBuf,
}

impl PaletteSource {
    /// The dotfiles repository root.
    pub(crate) fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// The `colors/` directory of scheme files.
    pub(crate) fn colors_dir(&self) -> &Path {
        &self.colors_dir
    }

    /// The `scripts/generate-colors` generator.
    pub(crate) fn generate_colors(&self) -> &Path {
        &self.generate_colors
    }
}

/// The raw, injectable inputs a single detection pass consumes (R6.1).
///
/// A production run builds this with [`DetectionInputs::from_system`]; a test
/// constructs it directly to drive any branch deterministically. See the module
/// docs for how each field is turned into a capability.
#[derive(Clone, Debug)]
pub(crate) struct DetectionInputs {
    /// The `$PATH` to scan for target binaries (colon-separated directories).
    /// `None` (no `$PATH` in the environment) disables the scan, so every binary
    /// reads as absent.
    pub(crate) path: Option<String>,
    /// The names of currently running processes, as **untruncated** executable
    /// names (the basename of argv[0] from `/proc/<pid>/cmdline`) — the real
    /// detector fills this from a procfs scan, a test injects a fixed set. Daemon
    /// and portal liveness are decided against it by exact match. argv[0] is used
    /// rather than `/proc/<pid>/comm` because the kernel truncates `comm` to 15
    /// bytes, which would make the whole `xdg-desktop-portal-*` family
    /// indistinguishable (see [`SETTINGS_PORTAL_BACKENDS`]).
    pub(crate) running_processes: Vec<String>,
    /// The Hyprland IPC socket path whose existence signals a live compositor, or
    /// `None` when it cannot be located from the environment. Its existence is
    /// checked against the real filesystem.
    pub(crate) hyprland_socket: Option<PathBuf>,
    /// The deployed config path used to discover the dotfiles repo root (R8.5),
    /// conventionally `~/.config/hypr/colors.conf`. It must be a symlink into the
    /// repo for the palette source to be considered present.
    pub(crate) palette_config_anchor: PathBuf,
    /// The live XDG paths of backing config files to check for readability (R4.4).
    /// Each unreadable path is recorded and logged at `warn`.
    pub(crate) config_paths: Vec<PathBuf>,
}

impl DetectionInputs {
    /// Gathers the real inputs from the running system (the production entry
    /// point, called at startup and on manual refresh, architecture §8, R4.3).
    ///
    /// `config_paths` are the backing-config XDG paths to check for readability;
    /// the caller (startup, task 5.4) resolves them, since path resolution is its
    /// concern. The `$PATH`, the procfs process list, the Hyprland socket path, and
    /// the palette anchor are all read from the environment here. Every read is
    /// best-effort: a missing environment variable or an unreadable `/proc` simply
    /// yields an empty/`None` input, which [`Capabilities::detect`] turns into the
    /// "absent" capability rather than an error.
    pub(crate) fn from_system(config_paths: Vec<PathBuf>) -> Self {
        DetectionInputs {
            path: std::env::var("PATH").ok(),
            running_processes: scan_procfs_process_names(),
            hyprland_socket: hyprland_socket_path(),
            palette_config_anchor: default_palette_anchor(),
            config_paths,
        }
    }
}

/// The result of one detection pass: which capabilities are present (architecture
/// §4).
///
/// Plain data — no behaviour beyond the query accessors below. It derives equality
/// so a refresh (R4.3) can compare a new pass against the previous one, and it is
/// cheap to hold for the life of a window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Capabilities {
    /// Target binaries found on `$PATH`.
    present_binaries: BTreeSet<Binary>,
    /// Daemons found running by the procfs scan.
    live_daemons: BTreeSet<Daemon>,
    /// Whether the Hyprland IPC socket exists (the compositor is live).
    hyprland_ipc: bool,
    /// Whether a live theme-restyle path (settings portal or dconf backend) is
    /// available (R2.2).
    settings_portal: bool,
    /// The dotfiles palette source, or `None` when there is no repo behind the
    /// config (R3.2, R8.5).
    palette_source: Option<PaletteSource>,
    /// Backing config paths that could not be read (missing or unreadable, R4.4).
    unreadable_configs: BTreeSet<PathBuf>,
}

impl Capabilities {
    /// Runs one detection pass over `inputs` (R4.1).
    ///
    /// This is the pure, re-runnable routine: a manual refresh (R4.3) re-gathers
    /// [`DetectionInputs`] and calls it again. It performs the binary scan, daemon
    /// and Hyprland liveness checks, the settings-portal probe, palette-source
    /// discovery, and config-readability checks, logging absent page-gating items
    /// at `info` and unreadable configs at `warn` (R4.2/R4.4/R7.3), then returns
    /// the plain [`Capabilities`]. It never fails: every probe degrades to "absent".
    pub(crate) fn detect(inputs: &DetectionInputs) -> Capabilities {
        let present_binaries = scan_binaries(inputs.path.as_deref());
        log_binary_results(&present_binaries);

        let live_daemons = scan_daemons(&inputs.running_processes);

        // Hyprland liveness is the socket's existence on the real filesystem; a
        // `None` anchor (no socket path resolvable) is simply "not live".
        let hyprland_ipc = inputs.hyprland_socket.as_deref().is_some_and(Path::exists);
        tracing::debug!(hyprland_ipc, "Hyprland IPC socket liveness");

        let settings_portal = detect_settings_portal(&inputs.running_processes, &present_binaries);
        if !settings_portal {
            // Not a hidden page, but a user-visible behaviour change (theme edits
            // take effect at next launch rather than live), so it earns an `info`.
            tracing::info!(
                "no settings portal or dconf backend detected; live theme restyle \
                 unavailable — changes take effect at next launch (R2.2)"
            );
        }

        let palette_source = detect_palette_source(&inputs.palette_config_anchor);

        let unreadable_configs = check_configs(&inputs.config_paths);

        let capabilities = Capabilities {
            present_binaries,
            live_daemons,
            hyprland_ipc,
            settings_portal,
            palette_source,
            unreadable_configs,
        };
        capabilities.log_summary();
        capabilities
    }

    /// An all-absent capability set: no binary, no live daemon, no portal, no
    /// palette source (task 5.4).
    ///
    /// This is the window's *initial* state before the startup worker delivers real
    /// detection results (architecture §8): the shell is built while detection runs
    /// concurrently on a worker thread, so the window needs a placeholder capability
    /// value before the first real one arrives. Every query reports "absent", so no
    /// category is shown until the worker completes. It is also the value the window
    /// falls back to if the worker fails to deliver a result, so the user still gets
    /// a usable (if empty) window with a working Refresh action (R4.3). Unlike
    /// [`Capabilities::detect`] it runs no probes and logs nothing.
    pub(crate) fn absent() -> Capabilities {
        Capabilities {
            present_binaries: BTreeSet::new(),
            live_daemons: BTreeSet::new(),
            hyprland_ipc: false,
            settings_portal: false,
            palette_source: None,
            unreadable_configs: BTreeSet::new(),
        }
    }

    /// Whether `binary` was found on `$PATH`.
    pub(crate) fn has_binary(&self, binary: Binary) -> bool {
        self.present_binaries.contains(&binary)
    }

    /// Whether any audio control client is on `$PATH` — WirePlumber's `wpctl` or
    /// PulseAudio's `pactl` (R3.1).
    ///
    /// This is the general "is there some audio client at all" query. Note the v1
    /// Sound page (task 6.2) does **not** gate on this: its enumeration (`pw-dump`,
    /// falling back to `wpctl status`) and its controls speak only `wpctl`/`pw-dump`,
    /// so it is gated on [`Binary::Wpctl`] specifically — a `pactl`-only host would
    /// otherwise render a dead, inert page, which R4.2 forbids. `pactl`-only support is
    /// out of v1 scope.
    pub(crate) fn audio_available(&self) -> bool {
        self.has_binary(Binary::Wpctl) || self.has_binary(Binary::Pactl)
    }

    /// Whether `daemon` was found running by the procfs scan (task 4.4 gates a
    /// reload on this).
    pub(crate) fn is_daemon_live(&self, daemon: Daemon) -> bool {
        self.live_daemons.contains(&daemon)
    }

    /// Whether the Hyprland IPC socket exists — i.e. the compositor is live.
    pub(crate) fn hyprland_ipc_live(&self) -> bool {
        self.hyprland_ipc
    }

    /// Whether a Hyprland reload can be issued: the `hyprctl` client is installed
    /// *and* the compositor is live (architecture §4). The reload table (task 4.4)
    /// uses this to decide whether to run `hyprctl reload`.
    pub(crate) fn hyprland_reloadable(&self) -> bool {
        self.has_binary(Binary::Hyprctl) && self.hyprland_ipc
    }

    /// Whether a live theme-restyle path is available — the GTK settings portal is
    /// running or a dconf GSettings backend is present (R2.2). Gates whether the
    /// Theme page may claim a live restyle versus "takes effect at next launch".
    pub(crate) fn settings_portal_available(&self) -> bool {
        self.settings_portal
    }

    /// The discovered dotfiles palette source, or `None` when the palette switcher
    /// must be hidden (R3.2, R8.5).
    pub(crate) fn palette_source(&self) -> Option<&PaletteSource> {
        self.palette_source.as_ref()
    }

    /// Whether the backing config at `path` was found readable (R4.4).
    ///
    /// Returns `true` for any path that was not among the [`DetectionInputs::config_paths`]
    /// checked (nothing was found wrong with it), and `false` for a path that was
    /// checked and could not be read — so a page can gate a control on its config
    /// being readable without the detector needing to know every possible path.
    ///
    /// Caveat: this keys on the exact [`PathBuf`] that was registered in
    /// [`DetectionInputs::config_paths`]; a caller must query with the *same* path it
    /// registered. Two paths that name the same file but differ textually (a relative
    /// vs absolute form, a trailing slash, an unresolved symlink) are treated as
    /// different keys and would misreport, so callers should register and query one
    /// canonical form.
    pub(crate) fn config_readable(&self, path: &Path) -> bool {
        !self.unreadable_configs.contains(path)
    }

    /// The backing config paths that could not be read (missing or unreadable),
    /// for the UI to hide the affected controls (R4.4).
    pub(crate) fn unreadable_configs(&self) -> &BTreeSet<PathBuf> {
        &self.unreadable_configs
    }

    /// Logs a one-line detection summary at `info` (R7.3, architecture §8). Counts
    /// only — never file contents.
    fn log_summary(&self) {
        tracing::info!(
            binaries = self.present_binaries.len(),
            daemons = self.live_daemons.len(),
            hyprland_ipc = self.hyprland_ipc,
            settings_portal = self.settings_portal,
            palette_source = self.palette_source.is_some(),
            unreadable_configs = self.unreadable_configs.len(),
            "capabilities detection complete"
        );
    }
}

#[cfg(test)]
impl Capabilities {
    /// Builds a [`Capabilities`] directly from explicit sets, for tests in other
    /// modules that need a precise capability combination without constructing a
    /// full [`DetectionInputs`] filesystem fixture.
    ///
    /// This is the seam the reload-table tests (task 4.4) use to drive
    /// capability-gating: they pass exactly the binaries and live daemons a
    /// scenario requires. `settings_portal`, `palette_source`, and
    /// `unreadable_configs` are irrelevant to the reload table, so they are fixed to
    /// their "absent"/empty forms; a test that needs them exercises the real
    /// [`Capabilities::detect`] path instead.
    pub(crate) fn for_tests(
        binaries: &[Binary],
        live_daemons: &[Daemon],
        hyprland_ipc: bool,
    ) -> Capabilities {
        Capabilities {
            present_binaries: binaries.iter().copied().collect(),
            live_daemons: live_daemons.iter().copied().collect(),
            hyprland_ipc,
            settings_portal: false,
            palette_source: None,
            unreadable_configs: BTreeSet::new(),
        }
    }

    /// Returns `self` with the dotfiles palette source marked present, for tests that
    /// need the palette-gated path — such as the Theme category's `palette_source`
    /// arm (task 5.1) — without building a real symlinked-repo filesystem fixture.
    ///
    /// The paths are placeholders: a test that needs the *real* discovery logic
    /// exercises [`Capabilities::detect`] against a temp-dir fixture instead. It is
    /// builder-style so it chains onto [`Self::for_tests`], keeping that constructor's
    /// signature (and its "absent" defaults) unchanged.
    pub(crate) fn with_palette_source_for_tests(mut self) -> Capabilities {
        self.palette_source = Some(PaletteSource {
            repo_root: PathBuf::from("/test/dotfiles"),
            colors_dir: PathBuf::from("/test/dotfiles/colors"),
            generate_colors: PathBuf::from("/test/dotfiles/scripts/generate-colors"),
        });
        self
    }
}

/// Scans `path` for each [`Binary`], returning those found (a `which`-equivalent,
/// architecture §4).
///
/// The `$PATH` string is split on `:`; an empty entry is skipped rather than
/// treated as the current directory (the POSIX meaning), so detection never probes
/// the working directory — a small security nicety. A binary is present if any
/// directory holds an executable file of its [`command_name`](Binary::command_name)
/// (see [`is_executable_file`]).
fn scan_binaries(path: Option<&str>) -> BTreeSet<Binary> {
    let mut present = BTreeSet::new();
    let Some(path) = path else {
        return present;
    };

    let dirs: Vec<&str> = path.split(':').filter(|dir| !dir.is_empty()).collect();
    for &binary in Binary::ALL {
        let found = dirs
            .iter()
            .any(|dir| is_executable_file(&Path::new(dir).join(binary.command_name())));
        if found {
            present.insert(binary);
        }
    }
    present
}

/// Logs which page-gating binaries are absent, at `info` (the "hidden item" signal,
/// R4.2/R7.3), and the rest at `debug`.
///
/// [`Binary::Dconf`] is deliberately excluded from the `info` path: it gates no
/// page of its own (only the live-restyle claim, which is logged separately), so
/// flagging it as a hidden item would be misleading.
fn log_binary_results(present: &BTreeSet<Binary>) {
    for &binary in Binary::ALL {
        let name = binary.command_name();
        if present.contains(&binary) {
            tracing::debug!(binary = name, "found on PATH");
        } else if binary == Binary::Dconf {
            tracing::debug!(
                binary = name,
                "dconf not on PATH; relying on the portal-process signal for live restyle"
            );
        } else if binary == Binary::NmConnectionEditor {
            // nm-connection-editor gates no page — it only decides which tool the
            // Network page's launcher button spawns, and `kitty -e nmtui` is the
            // fallback — so its absence hides nothing by itself. Log at debug rather
            // than as a hidden item. (kitty, whose absence *can* hide the button,
            // falls through to the generic hidden-item branch below.)
            tracing::debug!(
                binary = name,
                "nm-connection-editor not on PATH; the Network launcher falls back to kitty+nmtui"
            );
        } else if binary == Binary::Pactl {
            // pactl gates no page in v1: the Sound page speaks only wpctl/pw-dump, so an
            // absent pactl hides nothing. Log at debug rather than as a hidden item.
            // (wpctl, which *does* gate the Sound page, falls through to the generic
            // hidden-item branch below.)
            tracing::debug!(
                binary = name,
                "pactl not on PATH; it gates no settings in v1 (the Sound page uses wpctl)"
            );
        } else {
            tracing::info!(
                binary = name,
                "not found on PATH; dependent settings will be hidden (R4.2)"
            );
        }
    }
}

/// Determines which [`Daemon`]s are running from a list of process names.
fn scan_daemons(running: &[String]) -> BTreeSet<Daemon> {
    Daemon::ALL
        .iter()
        .copied()
        .filter(|daemon| is_process_running(running, daemon.process_name()))
        .collect()
}

/// Whether a live theme-restyle path is present (R2.2).
///
/// Two independent signals, either sufficient: a Settings-implementing portal
/// backend running (one of [`SETTINGS_PORTAL_BACKENDS`], matched by its full
/// untruncated name — see that constant for why the base/non-Settings portals must
/// not count), or the dconf CLI installed (taken as a proxy for the dconf GSettings
/// backend, which ships alongside it). The dconf proxy is a heuristic — the backend
/// is a shared library, not a binary — but reliable in practice, and a false
/// negative only downgrades the UI to the correct "next launch" message.
fn detect_settings_portal(running: &[String], binaries: &BTreeSet<Binary>) -> bool {
    SETTINGS_PORTAL_BACKENDS
        .iter()
        .any(|backend| is_process_running(running, backend))
        || binaries.contains(&Binary::Dconf)
}

/// Whether `running` contains the process `name`, matched exactly.
///
/// The injected names are untruncated argv[0] basenames (see
/// [`scan_procfs_process_names`]), so an exact comparison is correct and there is no
/// `/proc/comm` truncation to work around.
fn is_process_running(running: &[String], name: &str) -> bool {
    running.iter().any(|candidate| candidate == name)
}

/// Discovers the dotfiles palette source behind the config `anchor` (R3.2, R8.5).
///
/// The anchor must be a **symlink** — the deployment method for this dotfiles setup
/// (analysis §1). A plain file means there is no repo behind it, so the palette
/// switcher is hidden exactly like a missing app (the explicit accept criterion);
/// this is logged at `info` as a hidden item. When it is a symlink, it is
/// canonicalized to the real repo file, the repo root is derived by stripping the
/// known [`REPO_ANCHOR_RELATIVE`] tail, and the source is present only when both
/// `colors/` and `scripts/generate-colors` exist under that root. Any filesystem
/// failure (a dangling symlink, an unexpected resolved layout) degrades to `None`.
///
/// Every path that ends in a hidden palette source is logged at `info`, matching
/// the "hidden item" convention for the rest of detection (R4.2), so all the reasons
/// the switcher is absent surface uniformly in the journal.
fn detect_palette_source(anchor: &Path) -> Option<PaletteSource> {
    // `symlink_metadata` does not follow the link, so it tells us whether the
    // anchor *itself* is a symlink — the signal that a dotfiles repo is deployed
    // behind it. A plain file (or a missing/unreadable anchor) means no repo.
    match std::fs::symlink_metadata(anchor) {
        Ok(metadata) if metadata.file_type().is_symlink() => {}
        Ok(_) => {
            tracing::info!(
                anchor = %anchor.display(),
                "palette config is not a symlink into a dotfiles repo; palette switcher hidden (R3.2/R8.5)"
            );
            return None;
        }
        Err(error) => {
            tracing::info!(
                anchor = %anchor.display(),
                %error,
                "palette config anchor is absent or unreadable; palette switcher hidden (R3.2/R8.5)"
            );
            return None;
        }
    }

    // Follow the link to the real repo file.
    let canonical = match std::fs::canonicalize(anchor) {
        Ok(canonical) => canonical,
        Err(error) => {
            tracing::info!(
                anchor = %anchor.display(),
                %error,
                "palette config symlink could not be resolved (dangling?); palette switcher hidden (R3.2/R8.5)"
            );
            return None;
        }
    };

    // Derive the repo root by stripping the known repo-relative tail. Verifying the
    // tail matches (rather than blindly dropping three components) guards against a
    // symlink that points somewhere with an unexpected shape.
    let relative: PathBuf = REPO_ANCHOR_RELATIVE.iter().collect();
    let Some(repo_root) = canonical
        .ends_with(&relative)
        .then(|| canonical.ancestors().nth(REPO_ANCHOR_RELATIVE.len()))
        .flatten()
        .map(Path::to_path_buf)
    else {
        tracing::info!(
            resolved = %canonical.display(),
            "palette config resolved outside the expected repo layout; palette switcher hidden (R3.2/R8.5)"
        );
        return None;
    };

    let colors_dir = repo_root.join("colors");
    let generate_colors = repo_root.join("scripts").join("generate-colors");
    if colors_dir.is_dir() && generate_colors.is_file() {
        tracing::debug!(repo_root = %repo_root.display(), "palette source discovered");
        Some(PaletteSource {
            repo_root,
            colors_dir,
            generate_colors,
        })
    } else {
        tracing::info!(
            repo_root = %repo_root.display(),
            "dotfiles repo found but its palette source is incomplete (missing colors/ or \
             scripts/generate-colors); palette switcher hidden (R3.2)"
        );
        None
    }
}

/// Checks each config path for readability, returning the ones that failed (R4.4).
///
/// A path that cannot be read — missing, permission-revoked, or otherwise
/// inaccessible — is logged at `warn` and returned so the UI hides its controls.
/// Only the path and OS error are logged, never the file's contents (R7.3). The
/// full bytes are read (config files are tiny) but discarded; structural
/// parseability is validated later by the page's own parser.
fn check_configs(paths: &[PathBuf]) -> BTreeSet<PathBuf> {
    let mut unreadable = BTreeSet::new();
    for path in paths {
        match std::fs::read(path) {
            Ok(_) => {
                tracing::debug!(path = %path.display(), "config readable");
            }
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "backing config file is missing or unreadable; its controls will be hidden (R4.4)"
                );
                unreadable.insert(path.clone());
            }
        }
    }
    unreadable
}

/// Whether `path` is a regular file with an executable bit set (a `which`-style
/// test, Unix).
///
/// [`std::fs::metadata`] follows symlinks, so a binary deployed as a symlink
/// resolves to its target before the file-type and permission checks. Any stat
/// failure (a missing file, a broken link) is simply "not executable". On non-Unix
/// targets — which the app does not ship on — any regular file counts, since there
/// is no Unix mode to consult.
fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Scans procfs for the names of currently running processes (the pidof-equivalent,
/// architecture §4).
///
/// Reads argv[0] from `/proc/<pid>/cmdline` for every numeric `/proc` entry and
/// returns its basename (see [`process_name_from_cmdline`]). `cmdline` is used
/// rather than `comm` because the kernel caps `comm` at 15 bytes, which would make
/// the whole `xdg-desktop-portal-*` family indistinguishable and let a non-Settings
/// portal masquerade as the GTK Settings backend (see [`SETTINGS_PORTAL_BACKENDS`]);
/// argv[0] is not truncated. Every read is best-effort: `/proc` being unreadable
/// yields an empty list (all daemons read as absent), and a process that exits
/// between the directory scan and the read (or a kernel thread with an empty
/// `cmdline`) is simply skipped — a benign race, not an error.
fn scan_procfs_process_names() -> Vec<String> {
    let mut names = Vec::new();
    let entries = match std::fs::read_dir("/proc") {
        Ok(entries) => entries,
        Err(error) => {
            tracing::debug!(%error, "could not read /proc; daemon liveness unavailable");
            return names;
        }
    };

    for entry in entries.flatten() {
        let file_name = entry.file_name();
        // Only numeric entries are process directories; skip the rest.
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }

        let cmdline_path = entry.path().join("cmdline");
        if let Ok(cmdline) = std::fs::read(&cmdline_path) {
            if let Some(process_name) = process_name_from_cmdline(&cmdline) {
                names.push(process_name);
            }
        }
    }

    names
}

/// Extracts a process's executable name — the basename of argv[0] — from raw
/// `/proc/<pid>/cmdline` bytes, or `None` when it is unavailable.
///
/// `cmdline` is NUL-separated (`argv` joined by `\0`), so argv[0] is the bytes up to
/// the first NUL; its basename is the executable name daemons and portal backends
/// are matched against. Returns `None` for an empty `cmdline` (a kernel thread or a
/// zombie), a non-UTF-8 argv[0], or a path with no final component — none of which
/// can match a target name, so skipping them is correct.
fn process_name_from_cmdline(cmdline: &[u8]) -> Option<String> {
    let argv0 = cmdline.split(|&byte| byte == 0).next()?;
    if argv0.is_empty() {
        return None;
    }
    let argv0 = std::str::from_utf8(argv0).ok()?;
    let basename = Path::new(argv0).file_name()?.to_str()?;
    Some(basename.to_string())
}

/// The Hyprland IPC socket path derived from the environment, or `None` when it
/// cannot be located (architecture §4).
///
/// Built as `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket.sock`; a
/// missing variable (no running Hyprland, or a non-Hyprland session) yields `None`,
/// which reads as "not live". The path's *existence* is checked later by
/// [`Capabilities::detect`], not here.
fn hyprland_socket_path() -> Option<PathBuf> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    let signature = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    Some(
        Path::new(&runtime_dir)
            .join("hypr")
            .join(signature)
            .join(".socket.sock"),
    )
}

/// The default palette config anchor from the XDG environment (R8.5).
///
/// Prefers `$XDG_CONFIG_HOME/hypr/colors.conf`, falling back to
/// `$HOME/.config/hypr/colors.conf`. When neither variable is set the returned
/// relative path simply fails discovery in [`detect_palette_source`] (the palette
/// source degrades to absent), so this never has to fail.
fn default_palette_anchor() -> PathBuf {
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME") {
        if !config_home.is_empty() {
            return Path::new(&config_home).join("hypr").join("colors.conf");
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Path::new(&home)
            .join(".config")
            .join("hypr")
            .join("colors.conf");
    }
    PathBuf::from("colors.conf")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    /// Detection inputs with everything absent — a blank slate a test tweaks. The
    /// palette anchor points at a nonexistent path so the palette source is absent
    /// by default.
    fn base_inputs() -> DetectionInputs {
        DetectionInputs {
            path: None,
            running_processes: Vec::new(),
            hyprland_socket: None,
            palette_config_anchor: PathBuf::from("/nonexistent/settings4000/hypr/colors.conf"),
            config_paths: Vec::new(),
        }
    }

    /// Writes a fake executable named `name` into `dir` and returns its path. On
    /// Unix the executable bit is set so [`is_executable_file`] accepts it.
    fn write_executable(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, b"#!/bin/sh\n").expect("write a fake binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                .expect("mark the fake binary executable");
        }
        path
    }

    /// Joins directory paths into a `$PATH`-style colon-separated string.
    fn path_string(dirs: &[&Path]) -> String {
        dirs.iter()
            .map(|dir| dir.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(":")
    }

    #[test]
    fn binary_present_vs_absent_on_a_fake_path_flips_the_capability() {
        // Accept criterion (R6.1 fake-PATH): a binary present on the injected PATH
        // reads as present, and the same detector with it gone reads as absent.
        let dir = tempfile::tempdir().expect("temp dir should be creatable");
        write_executable(dir.path(), "hyprctl");

        let mut inputs = base_inputs();
        inputs.path = Some(path_string(&[dir.path()]));
        let caps = Capabilities::detect(&inputs);
        assert!(
            caps.has_binary(Binary::Hyprctl),
            "hyprctl is on the fake PATH"
        );
        assert!(
            !caps.has_binary(Binary::Gsettings),
            "gsettings was never placed on the fake PATH"
        );

        // Remove it and re-run: the capability flips to absent. Re-running the same
        // routine also exercises the manual-refresh path (R4.3).
        fs::remove_file(dir.path().join("hyprctl")).expect("remove the fake binary");
        let caps = Capabilities::detect(&inputs);
        assert!(
            !caps.has_binary(Binary::Hyprctl),
            "hyprctl is gone from the fake PATH"
        );
    }

    #[test]
    fn path_scan_searches_every_directory_and_prefers_executables() {
        // The binary may live in any PATH directory, and the audio composite is
        // satisfied by either client.
        let first = tempfile::tempdir().expect("temp dir");
        let second = tempfile::tempdir().expect("temp dir");
        write_executable(second.path(), "wpctl");

        let mut inputs = base_inputs();
        inputs.path = Some(path_string(&[first.path(), second.path()]));
        let caps = Capabilities::detect(&inputs);

        assert!(
            caps.has_binary(Binary::Wpctl),
            "wpctl found in the second dir"
        );
        assert!(
            caps.audio_available(),
            "wpctl satisfies the audio composite"
        );
        assert!(!caps.has_binary(Binary::Pactl));
    }

    #[cfg(unix)]
    #[test]
    fn a_non_executable_file_is_not_a_binary() {
        // A plain (non-executable) file with a binary's name must not count as the
        // binary being installed — the Unix exec-bit check is what distinguishes
        // an installed tool from an unrelated data file.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("nmcli");
        fs::write(&path, b"not a program").expect("write a plain file");

        let mut inputs = base_inputs();
        inputs.path = Some(path_string(&[dir.path()]));
        let caps = Capabilities::detect(&inputs);
        assert!(
            !caps.has_binary(Binary::Nmcli),
            "a non-executable file must not be detected as the binary"
        );
    }

    #[test]
    fn missing_path_makes_every_binary_absent() {
        let caps = Capabilities::detect(&base_inputs());
        for &binary in Binary::ALL {
            assert!(
                !caps.has_binary(binary),
                "{binary:?} must be absent with no PATH"
            );
        }
        assert!(!caps.audio_available());
    }

    #[test]
    fn daemon_liveness_is_injectable_present_and_absent() {
        // Accept criterion (liveness injectable): a supplied process list flips
        // daemon liveness without a live daemon.
        let mut inputs = base_inputs();
        inputs.running_processes = vec!["swaync".to_string(), "eww".to_string()];
        let caps = Capabilities::detect(&inputs);

        assert!(caps.is_daemon_live(Daemon::Swaync));
        assert!(caps.is_daemon_live(Daemon::Eww));
        assert!(
            !caps.is_daemon_live(Daemon::Hypridle),
            "hypridle was not listed"
        );
        assert!(!caps.is_daemon_live(Daemon::Kitty));

        // Absent case: no processes -> no daemon is live.
        let caps = Capabilities::detect(&base_inputs());
        for &daemon in Daemon::ALL {
            assert!(!caps.is_daemon_live(daemon), "{daemon:?} must be absent");
        }
    }

    #[test]
    fn settings_portal_detected_from_a_settings_backend_by_full_name() {
        // Accept criterion (portal injectable, R2.2): a Settings-implementing backend
        // running signals the live-restyle path. Detection matches the full name, so
        // the injected process list carries the untruncated `xdg-desktop-portal-gtk`.
        let mut inputs = base_inputs();
        inputs.running_processes = vec!["xdg-desktop-portal-gtk".to_string()];
        assert!(
            Capabilities::detect(&inputs).settings_portal_available(),
            "a running xdg-desktop-portal-gtk enables live restyle"
        );

        // A GNOME/KDE Settings backend counts too.
        let mut inputs = base_inputs();
        inputs.running_processes = vec!["xdg-desktop-portal-gnome".to_string()];
        assert!(Capabilities::detect(&inputs).settings_portal_available());
    }

    #[test]
    fn non_settings_portal_alone_does_not_claim_live_restyle() {
        // Finding 1 regression guard: the base portal and the non-Settings backends a
        // Hyprland session normally runs (`-hyprland`, `-wlr`) do NOT implement the
        // Settings interface, so with NO `-gtk`/`-gnome`/`-kde` and NO dconf the app
        // must NOT claim live restyle — the false positive the 15-byte `/proc/comm`
        // prefix (`xdg-desktop-por`, shared by the whole family) would have caused.
        let mut inputs = base_inputs();
        inputs.running_processes = vec![
            "xdg-desktop-portal".to_string(),
            "xdg-desktop-portal-hyprland".to_string(),
            "xdg-desktop-portal-wlr".to_string(),
        ];
        assert!(
            !Capabilities::detect(&inputs).settings_portal_available(),
            "non-Settings portals must not be taken as a live-restyle path (R2.2)"
        );

        // Nothing running at all is likewise absent.
        assert!(!Capabilities::detect(&base_inputs()).settings_portal_available());
    }

    #[test]
    fn settings_portal_detected_from_the_dconf_backend_alone() {
        // The dconf CLI on PATH is the second live-restyle signal, independent of
        // the portal process.
        let dir = tempfile::tempdir().expect("temp dir");
        write_executable(dir.path(), "dconf");

        let mut inputs = base_inputs();
        inputs.path = Some(path_string(&[dir.path()]));
        let caps = Capabilities::detect(&inputs);
        assert!(
            caps.settings_portal_available(),
            "the dconf backend alone enables live restyle"
        );
    }

    #[test]
    fn hyprland_liveness_follows_the_socket_path_existence() {
        // Present: an existing file at the socket path reads as live (we only test
        // existence, so a plain file stands in for the socket).
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join(".socket.sock");
        fs::write(&socket, b"").expect("create a stand-in socket file");

        let mut inputs = base_inputs();
        inputs.hyprland_socket = Some(socket);
        assert!(Capabilities::detect(&inputs).hyprland_ipc_live());

        // Absent: a nonexistent socket path, and no path at all.
        let mut inputs = base_inputs();
        inputs.hyprland_socket = Some(dir.path().join("missing.sock"));
        assert!(!Capabilities::detect(&inputs).hyprland_ipc_live());
        assert!(!Capabilities::detect(&base_inputs()).hyprland_ipc_live());
    }

    #[test]
    fn hyprland_reloadable_requires_both_the_client_and_a_live_socket() {
        let bin = tempfile::tempdir().expect("temp dir");
        write_executable(bin.path(), "hyprctl");
        let run = tempfile::tempdir().expect("temp dir");
        let socket = run.path().join(".socket.sock");
        fs::write(&socket, b"").expect("stand-in socket");

        // Both present -> reloadable.
        let mut inputs = base_inputs();
        inputs.path = Some(path_string(&[bin.path()]));
        inputs.hyprland_socket = Some(socket.clone());
        assert!(Capabilities::detect(&inputs).hyprland_reloadable());

        // Client but no live socket -> not reloadable.
        let mut inputs = base_inputs();
        inputs.path = Some(path_string(&[bin.path()]));
        assert!(!Capabilities::detect(&inputs).hyprland_reloadable());

        // Live socket but no client -> not reloadable.
        let mut inputs = base_inputs();
        inputs.hyprland_socket = Some(socket);
        assert!(!Capabilities::detect(&inputs).hyprland_reloadable());
    }

    /// Builds a fake dotfiles repo under `root` with the deployed anchor symlinked
    /// into it, and returns the anchor path plus the repo root. `with_colors` and
    /// `with_generator` control whether the palette sources are created, so a test
    /// can exercise the complete and incomplete cases.
    #[cfg(unix)]
    fn build_repo_with_symlinked_anchor(
        root: &Path,
        with_colors: bool,
        with_generator: bool,
    ) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::symlink;

        let repo = root.join("dotfiles");
        let repo_config = repo.join("config").join("hypr");
        fs::create_dir_all(&repo_config).expect("create repo config dir");
        let repo_colors_conf = repo_config.join("colors.conf");
        fs::write(&repo_colors_conf, b"# Generated from colors/nord\n")
            .expect("write the generated colors.conf in the repo");

        if with_colors {
            let colors = repo.join("colors");
            fs::create_dir_all(&colors).expect("create colors dir");
            fs::write(colors.join("nord"), b"bg0=2e3440\n").expect("write a scheme");
        }
        if with_generator {
            let scripts = repo.join("scripts");
            fs::create_dir_all(&scripts).expect("create scripts dir");
            fs::write(scripts.join("generate-colors"), b"#!/bin/sh\n")
                .expect("write the generator");
        }

        // The deployed config lives outside the repo and is a symlink into it.
        let deployed = root.join("config").join("hypr");
        fs::create_dir_all(&deployed).expect("create deployed config dir");
        let anchor = deployed.join("colors.conf");
        symlink(&repo_colors_conf, &anchor).expect("symlink the deployed anchor into the repo");

        (anchor, repo)
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_config_into_a_complete_repo_yields_the_palette_source() {
        // Accept criterion (R3.2/R8.5): a config symlinked into a repo that has
        // colors/ and scripts/generate-colors makes the palette source present,
        // with the repo root exposed for tasks 3.7/6.3/4.5.
        let root = tempfile::tempdir().expect("temp dir");
        let (anchor, repo) = build_repo_with_symlinked_anchor(root.path(), true, true);

        let mut inputs = base_inputs();
        inputs.palette_config_anchor = anchor;
        let caps = Capabilities::detect(&inputs);

        let source = caps
            .palette_source()
            .expect("a complete repo behind the symlink yields a palette source");
        let expected_root = fs::canonicalize(&repo).expect("canonicalize the repo root");
        assert_eq!(source.repo_root(), expected_root.as_path());
        assert_eq!(source.colors_dir(), expected_root.join("colors"));
        assert_eq!(
            source.generate_colors(),
            expected_root.join("scripts").join("generate-colors")
        );
    }

    #[test]
    fn a_non_symlinked_plain_config_hides_the_palette_source() {
        // Accept criterion (explicit): a plain (non-symlink) config means no repo
        // behind it, so the palette switcher is hidden.
        let dir = tempfile::tempdir().expect("temp dir");
        let anchor = dir.path().join("colors.conf");
        fs::write(&anchor, b"# Generated from colors/nord\n").expect("write a plain config");

        let mut inputs = base_inputs();
        inputs.palette_config_anchor = anchor;
        assert!(
            Capabilities::detect(&inputs).palette_source().is_none(),
            "a plain config must hide the palette source"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_into_an_incomplete_repo_hides_the_palette_source() {
        // A symlink into a repo missing the generator (or the colors dir) is not a
        // usable palette source, so it is hidden.
        let missing_generator = tempfile::tempdir().expect("temp dir");
        let (anchor, _repo) =
            build_repo_with_symlinked_anchor(missing_generator.path(), true, false);
        let mut inputs = base_inputs();
        inputs.palette_config_anchor = anchor;
        assert!(
            Capabilities::detect(&inputs).palette_source().is_none(),
            "a missing generate-colors hides the palette source"
        );

        let missing_colors = tempfile::tempdir().expect("temp dir");
        let (anchor, _repo) = build_repo_with_symlinked_anchor(missing_colors.path(), false, true);
        let mut inputs = base_inputs();
        inputs.palette_config_anchor = anchor;
        assert!(
            Capabilities::detect(&inputs).palette_source().is_none(),
            "a missing colors/ dir hides the palette source"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_resolving_outside_the_expected_layout_hides_the_palette_source() {
        // Finding 2: exercise the `ends_with(config/hypr/colors.conf)` guard. A
        // symlink that resolves to a target whose tail is NOT the repo-relative
        // anchor path cannot yield a trustworthy repo root, so the source is hidden
        // via the ends_with-false branch — even if the surrounding directory happens
        // to hold a colors/ and scripts/generate-colors.
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("temp dir");
        // A repo-shaped tree, but the anchor points at `somewhere/else.conf` rather
        // than the expected `config/hypr/colors.conf`.
        let repo = root.path().join("dotfiles");
        fs::create_dir_all(repo.join("colors")).expect("create colors dir");
        fs::create_dir_all(repo.join("scripts")).expect("create scripts dir");
        fs::write(repo.join("scripts").join("generate-colors"), b"#!/bin/sh\n")
            .expect("write the generator");
        let odd_target = repo.join("somewhere");
        fs::create_dir_all(&odd_target).expect("create the odd target dir");
        let odd_target = odd_target.join("else.conf");
        fs::write(&odd_target, b"# not at the expected path\n").expect("write the odd target");

        let anchor = root.path().join("colors.conf");
        symlink(&odd_target, &anchor).expect("symlink to an unexpected target");

        let mut inputs = base_inputs();
        inputs.palette_config_anchor = anchor;
        assert!(
            Capabilities::detect(&inputs).palette_source().is_none(),
            "a symlink resolving outside the expected repo layout must hide the source"
        );
    }

    #[test]
    fn an_absent_palette_anchor_hides_the_palette_source() {
        // The default base_inputs anchor points at a nonexistent path.
        assert!(
            Capabilities::detect(&base_inputs())
                .palette_source()
                .is_none()
        );
    }

    #[test]
    fn readable_and_unreadable_configs_are_reported() {
        // Accept criterion (R4.4): a readable config is fine; a missing one degrades
        // to absent and is recorded so the UI can hide its controls.
        let dir = tempfile::tempdir().expect("temp dir");
        let readable = dir.path().join("monitors.conf");
        fs::write(&readable, b"monitor=,preferred,auto,1\n").expect("write a config");
        let missing = dir.path().join("gone.conf");

        let mut inputs = base_inputs();
        inputs.config_paths = vec![readable.clone(), missing.clone()];
        let caps = Capabilities::detect(&inputs);

        assert!(
            caps.config_readable(&readable),
            "an existing config is readable"
        );
        assert!(
            !caps.config_readable(&missing),
            "a missing config degrades to unreadable"
        );
        assert!(caps.unreadable_configs().contains(&missing));
        assert!(!caps.unreadable_configs().contains(&readable));

        // An unchecked path is treated as readable (nothing was found wrong).
        assert!(caps.config_readable(Path::new("/some/unchecked/path")));
    }

    #[cfg(unix)]
    #[test]
    fn a_permission_revoked_config_degrades_to_unreadable() {
        // A config that exists but whose read permission is revoked is reported as
        // unreadable. Guarded so it is a no-op under root (where the mode is ignored).
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("input.conf");
        fs::write(&path, b"kb_layout = us\n").expect("write a config");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000))
            .expect("revoke read permission");

        if fs::read(&path).is_err() {
            let mut inputs = base_inputs();
            inputs.config_paths = vec![path.clone()];
            let caps = Capabilities::detect(&inputs);
            assert!(!caps.config_readable(&path));
            assert!(caps.unreadable_configs().contains(&path));
        }

        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o644));
    }

    #[test]
    fn detection_is_rerunnable_and_deterministic() {
        // R4.3: the routine is re-runnable for a manual refresh; the same inputs
        // must produce an equal result.
        let dir = tempfile::tempdir().expect("temp dir");
        write_executable(dir.path(), "gsettings");
        let mut inputs = base_inputs();
        inputs.path = Some(path_string(&[dir.path()]));
        inputs.running_processes = vec!["eww".to_string()];

        let first = Capabilities::detect(&inputs);
        let second = Capabilities::detect(&inputs);
        assert_eq!(
            first, second,
            "detection must be deterministic across refreshes"
        );
    }

    #[test]
    fn from_system_gathers_inputs_without_panicking() {
        // The production gathering path must run on the real system without failing,
        // producing a usable Capabilities value. We assert only that it completes —
        // the machine running the test may or may not have any given tool.
        let inputs = DetectionInputs::from_system(Vec::new());
        let _ = Capabilities::detect(&inputs);
    }

    #[test]
    fn hidden_items_log_info_and_unreadable_configs_log_warn() {
        // Accept criterion (logging): a hidden item (absent binary, absent palette
        // source) is logged at `info`; an unreadable config is logged at `warn`.
        // Captured via a scoped tracing subscriber so no global state is touched.
        use std::sync::{Arc, Mutex};
        use tracing::Level;
        use tracing::subscriber::with_default;
        use tracing_subscriber::Layer;
        use tracing_subscriber::layer::SubscriberExt;

        #[derive(Clone, Default)]
        struct LogCapture {
            events: Arc<Mutex<Vec<(Level, String)>>>,
        }

        struct MessageVisitor<'a>(&'a mut String);
        impl tracing::field::Visit for MessageVisitor<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    use std::fmt::Write as _;
                    let _ = write!(self.0, "{value:?}");
                }
            }
        }

        impl<S: tracing::Subscriber> Layer<S> for LogCapture {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let mut message = String::new();
                event.record(&mut MessageVisitor(&mut message));
                self.events
                    .lock()
                    .expect("log capture mutex should not be poisoned")
                    .push((*event.metadata().level(), message));
            }
        }

        let dir = tempfile::tempdir().expect("temp dir");
        let missing_config = dir.path().join("missing.conf");
        let plain_anchor = dir.path().join("colors.conf");
        fs::write(&plain_anchor, b"# not a symlink\n").expect("write a plain anchor");

        let mut inputs = base_inputs();
        // No PATH -> every binary absent (info); plain anchor -> palette absent
        // (info); missing config -> unreadable (warn).
        inputs.palette_config_anchor = plain_anchor;
        inputs.config_paths = vec![missing_config];

        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        with_default(subscriber, || {
            // `tracing` keeps a single, process-global, cross-thread cache of each
            // callsite's interest. A parallel detect test that hits these very
            // `info!`/`warn!` callsites under the no-op default subscriber can cache
            // them as "never", which would suppress the events even under this
            // scoped subscriber. Guard against that race: first run detect once to
            // register every callsite inside it, then rebuild the interest cache so
            // all of them are re-evaluated as enabled against this thread's
            // subscriber. Only the measured second run (after clearing) is asserted.
            let _ = Capabilities::detect(&inputs);
            tracing::callsite::rebuild_interest_cache();
            capture
                .events
                .lock()
                .expect("log capture mutex should not be poisoned")
                .clear();
            let _ = Capabilities::detect(&inputs);
        });

        let events = capture
            .events
            .lock()
            .expect("log capture mutex should not be poisoned");
        assert!(
            events
                .iter()
                .any(|(level, message)| *level == Level::INFO && message.contains("palette")),
            "an absent palette source must be logged at info"
        );
        assert!(
            events
                .iter()
                .any(|(level, message)| *level == Level::INFO && message.contains("PATH")),
            "an absent binary must be logged at info"
        );
        assert!(
            events
                .iter()
                .any(|(level, message)| *level == Level::WARN && message.contains("unreadable")),
            "an unreadable config must be logged at warn"
        );
    }

    #[test]
    fn process_matching_is_exact_and_never_matches_a_prefix() {
        // Names are untruncated argv[0] basenames, so matching is exact: a mere
        // prefix must not match (this is what stops the shared `xdg-desktop-por`
        // prefix from making the whole portal family look like the GTK backend).
        assert!(is_process_running(&["eww".to_string()], "eww"));
        assert!(!is_process_running(&["ew".to_string()], "eww"));
        assert!(
            !is_process_running(
                &["xdg-desktop-portal".to_string()],
                "xdg-desktop-portal-gtk"
            ),
            "a shorter portal name must not match the longer GTK backend"
        );
        assert!(
            !is_process_running(
                &["xdg-desktop-portal-gtk-extra".to_string()],
                "xdg-desktop-portal-gtk"
            ),
            "a longer name must not match either"
        );
    }

    #[test]
    fn process_name_is_the_basename_of_argv0_from_cmdline() {
        // `/proc/<pid>/cmdline` is NUL-separated argv; the process name is argv[0]'s
        // basename, untruncated (unlike `/proc/comm`). A full path resolves to its
        // final component, a bare name is returned as-is, and arguments after the
        // first NUL are ignored.
        assert_eq!(
            process_name_from_cmdline(b"/usr/lib/xdg-desktop-portal-gtk\0"),
            Some("xdg-desktop-portal-gtk".to_string()),
            "a long name survives untruncated, and the path is stripped to its basename"
        );
        assert_eq!(
            process_name_from_cmdline(b"kitty\0--session\0foo\0"),
            Some("kitty".to_string()),
            "argv[1..] after the first NUL is ignored"
        );
        assert_eq!(process_name_from_cmdline(b"eww\0"), Some("eww".to_string()));
        // Empty cmdline (kernel thread / zombie) yields no name.
        assert_eq!(process_name_from_cmdline(b""), None);
        assert_eq!(process_name_from_cmdline(b"\0"), None);
    }
}
