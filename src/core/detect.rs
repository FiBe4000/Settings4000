//! Installed-app detection and dynamic visibility (tasks 4.3, 11.2, 11.7, 11.8, 11.12; architecture
//! §4; R4.1–R4.4, R2.2, R3.2, R8.5, R6.1, R7.3).
//!
//! # What this module is
//!
//! At startup the app must decide which settings to show. A page or row is shown
//! only when the thing it edits is actually present — the compositor is running,
//! the daemon it reloads is installed, the config it writes is readable, the
//! dotfiles palette source or laptop-display toggle script exists. This module runs
//! those probes once and returns a plain [`Capabilities`] value that the UI (tasks
//! 5.x/6.x) consults to hide the rows/pages it cannot support (R4.2), that the
//! reload table (task 4.4) consults to skip a reload for a component that is not
//! running, and that the Apply pipeline (task 4.5) consults for the palette
//! source's repo root (R8.5). Two results only inform the user and gate nothing:
//! the detected repo root, which the Theme page compares each live `settings.ini`
//! with, and whether `xsettingsd` — the trace of an external GTK-settings writer —
//! runs (task 11.12, analysis §7.7).
//!
//! It lives in `core/` because it is pure domain logic driven by injected inputs
//! (see below), so it is headlessly testable (R6.2) and the layering guard in
//! `tests/module_boundaries.rs` forbids any `gtk`/`relm4` import.
//!
//! # Detection spawns (almost) nothing
//!
//! Unlike reloads (which spawn processes and therefore go through
//! [`crate::system::command`]), detection mostly *reads* the environment,
//! filesystem, procfs, and socket paths. Reading is not a process side effect, so
//! there is no shell and no subprocess for those probes: the binary scan is a
//! manual `$PATH` walk (a `which`-equivalent, architecture §4), daemon liveness is
//! a procfs scan, and everything else is a filesystem stat/read. Avoiding a
//! `which` subprocess per binary also keeps cold start inside the budget (R8.1).
//!
//! Two probes are exceptions, because the answer is only available from a running
//! program:
//!
//! - Hyprland's **config provider** (task 11.2), which a running compositor reports
//!   only over its IPC (`hyprctl status` → `configProvider: lua`), asked only when
//!   the IPC socket exists and `hyprctl` is on `$PATH`;
//! - the **notifd** notification daemon (task 11.7), whose liveness is ownership of
//!   its D-Bus name and whose version comes from `GetServerInformation` — two
//!   `busctl`/`systemctl` calls, asked only when `busctl` is on `$PATH` (see
//!   [`NotifdReplies::gather`] for the exact sequence and why none of them can start
//!   the daemon).
//!
//! Both are issued by [`DetectionInputs::from_system`] through the injected
//! [`CommandRunner`] (arg vectors, no shell, the usual timeout); their stdout is then
//! just another input. [`Capabilities::detect`] itself still spawns nothing.
//!
//! # The config provider is a guard, not a router (analysis §8.6)
//!
//! Hyprland ≥ 0.55 runs `hyprland.lua` through its Lua config provider, and the
//! target dotfiles deleted their hyprlang main config and input file. The app's
//! Hyprland-side input and cursor writes therefore exist only for the Lua files;
//! there is deliberately no hyprlang code path for them (Hyprland deprecated
//! hyprlang in 0.55). [`HyprConfigProvider`] records which provider is in use so a
//! host that still runs hyprlang gets the Input page and the Hyprland-side cursor
//! write *hidden* (logged at `info`), like a missing app (R4.2), instead of
//! receiving writes its compositor would ignore.
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
//!   [`DetectionInputs::palette_config_anchor`], [`DetectionInputs::config_paths`],
//!   [`DetectionInputs::hyprland_lua_config`],
//!   [`DetectionInputs::bar_text_tone_script`]), which [`Capabilities::detect`]
//!   checks against the real filesystem — so a test builds a temp-dir fixture (a
//!   fake socket file, a config symlinked into a fake repo, an unreadable config)
//!   and gets faithful behavior;
//! - the compositor's own config-provider report is given as the captured
//!   **text** of `hyprctl status` ([`DetectionInputs::hyprctl_status`]), and the
//!   notifd probe replies likewise ([`DetectionInputs::notifd_replies`], with the
//!   unit-file path [`DetectionInputs::notifd_unit_file`]), so a test injects fake
//!   replies.
//!
//! [`DetectionInputs::from_system`] gathers the real inputs (env `$PATH`, a procfs
//! scan, the `$XDG_RUNTIME_DIR` socket path, the XDG config paths, the
//! `hyprctl status` reply and the notifd probe replies);
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
//! Absent page-gating binaries, an absent palette source, an absent settings
//! portal, a non-Lua Hyprland config provider and an absent or stopped notifd are
//! logged at `info` (the "hidden item" signal, R4.2), as are a running `xsettingsd`
//! (a user-visible Theme-page note, task 11.12) and a missing eww `bar-text-tone`
//! script (a skipped wallpaper follow-up, task 11.14); a missing or unreadable config
//! is logged at `warn` (R4.4); a one-line detection summary, naming the config
//! provider and the notifd status, is logged at `info` (R7.3). File *contents* are
//! never logged.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::core::notifd::{NotifdReplies, NotifdStatus};
use crate::system::command::{Command, CommandRunner};

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
pub enum Binary {
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
    /// systemd's D-Bus client. Every notifd probe and control call goes through it
    /// (task 11.7), so without it the notifd capability is
    /// [`NotifdStatus::Absent`] and the Notifications page is hidden.
    Busctl,
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
        Binary::Busctl,
        Binary::Hyprpaper,
        Binary::Hypridle,
        Binary::Hyprlock,
        Binary::Nmcli,
        Binary::NmConnectionEditor,
        Binary::Dconf,
    ];

    /// The executable file name looked up in each `$PATH` directory.
    pub fn command_name(self) -> &'static str {
        match self {
            Binary::Hyprctl => "hyprctl",
            Binary::Gsettings => "gsettings",
            Binary::Pactl => "pactl",
            Binary::Wpctl => "wpctl",
            Binary::Kitty => "kitty",
            Binary::Eww => "eww",
            Binary::Busctl => "busctl",
            Binary::Hyprpaper => "hyprpaper",
            Binary::Hypridle => "hypridle",
            Binary::Hyprlock => "hyprlock",
            Binary::Nmcli => "nmcli",
            Binary::NmConnectionEditor => "nm-connection-editor",
            Binary::Dconf => "dconf",
        }
    }
}

/// A long-running component whose *liveness* the app checks: mostly so a reload is
/// issued only for a component that is actually running (architecture §6, task 4.4),
/// and in one case ([`Daemon::Xsettingsd`]) only so the Theme page can tell the user
/// that another program is managing GTK settings (task 11.12).
///
/// Liveness is decided by a procfs "pidof-equivalent" against
/// [`Daemon::process_name`]. Hyprland itself is intentionally absent here: its
/// liveness is the IPC socket, not a process-name match (see
/// [`Capabilities::hyprland_ipc_live`]). hyprlock is absent too — it runs only
/// while the screen is locked and receives no reload (its config is read at the
/// next lock), so a running check would be meaningless.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Daemon {
    /// The kitty terminal (reloaded via SIGUSR1).
    Kitty,
    /// The eww bar daemon (`eww reload`).
    Eww,
    /// The hyprpaper wallpaper daemon (`hyprctl hyprpaper …`).
    Hyprpaper,
    /// The hypridle idle daemon (restarted on config change).
    Hypridle,
    /// The `xsettingsd` XSETTINGS daemon. The app never reloads or signals it; it is
    /// scanned because it is the visible trace of an *external GTK-settings writer*:
    /// KDE's kded6 `gtkconfig` module starts it while that module manages GTK
    /// settings, and the dotfiles' `scripts/kded-gtkconfig off` kills it again at
    /// Hyprland start (analysis §7.7). When it runs, `settings.ini` and the GTK
    /// gsettings keys may be rewritten behind the app's back, so the Theme page shows
    /// a note (task 11.12). It gates nothing.
    Xsettingsd,
}

impl Daemon {
    /// Every daemon whose liveness is probed, in a fixed order.
    const ALL: &'static [Daemon] = &[
        Daemon::Kitty,
        Daemon::Eww,
        Daemon::Hyprpaper,
        Daemon::Hypridle,
        Daemon::Xsettingsd,
    ];

    /// The executable name (argv[0] basename, see [`scan_procfs_process_names`])
    /// that identifies a running instance of this daemon. All of them are under 15
    /// bytes, so they would survive `/proc/comm` truncation too, but detection
    /// matches them against the untruncated name for uniformity with the portal.
    pub fn process_name(self) -> &'static str {
        match self {
            Daemon::Kitty => "kitty",
            Daemon::Eww => "eww",
            Daemon::Hyprpaper => "hyprpaper",
            Daemon::Hypridle => "hypridle",
            Daemon::Xsettingsd => "xsettingsd",
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

/// The prefix of the `hyprctl status` line that names the active config provider
/// (`configProvider: lua`, verified on Hyprland 0.56.2, analysis §7.1).
const CONFIG_PROVIDER_PREFIX: &str = "configProvider:";

/// Which configuration language the Hyprland compositor runs (task 11.2; analysis
/// §7.1, §8.6; R4.1).
///
/// Hyprland ≥ 0.55 loads `~/.config/hypr/hyprland.lua` through its Lua provider
/// when that file exists and falls back to the legacy hyprlang main config
/// otherwise. The app's Hyprland-side input and cursor writes target only the Lua
/// files (`input.lua`, the two `hl.env` lines of `hyprland.lua`), so this value is
/// a **guard**: anything other than [`Lua`](Self::Lua) hides those controls (see
/// [`Capabilities::hypr_config_provider`]). It never selects an alternative
/// hyprlang writer — there is none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HyprConfigProvider {
    /// The Lua config provider (`hyprland.lua`) — the provider the target dotfiles
    /// run, and the only one the Input page and the Hyprland-side cursor write
    /// support.
    Lua,
    /// The legacy hyprlang provider (a `.conf` main config). Also the conservative answer
    /// when nothing indicates Lua (no session reports it and no `hyprland.lua`
    /// exists), since then the Lua files the app would write are not in use.
    Hyprlang,
}

impl HyprConfigProvider {
    /// The provider's name as `hyprctl status` spells it (`lua` / `hyprlang`), used
    /// in log messages.
    pub fn as_str(self) -> &'static str {
        match self {
            HyprConfigProvider::Lua => "lua",
            HyprConfigProvider::Hyprlang => "hyprlang",
        }
    }
}

impl fmt::Display for HyprConfigProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The repo-relative path of the config file used to detect the dotfiles repo
/// root (R8.5).
///
/// The deployed `~/.config/hypr/colors.conf` symlink resolves to
/// `<repo>/config/hypr/colors.conf` (analysis §1), so canonicalizing the anchor
/// and stripping these three trailing components yields the repo root. Keeping the
/// anchor's depth here — rather than a bare "strip three parents" — documents *why*
/// three, and lets [`detect_palette_source`] verify the resolved tail actually
/// matches before trusting the derived root.
const REPO_ANCHOR_RELATIVE: &[&str] = &["config", "hypr", "colors.conf"];

/// The repo-relative path of the dotfiles' laptop-display toggle script (analysis
/// §7.3), whose presence gates the Display page's laptop-panel switch (task 11.8).
/// It has no XDG location — the dotfiles bind it to a key and call it by repo path —
/// so it is found only under the repo root resolved from [`REPO_ANCHOR_RELATIVE`]
/// (R8.5).
const LAPTOP_TOGGLE_SCRIPT_RELATIVE: &[&str] = &["scripts", "hypr-toggle-laptop-display"];

/// The eww bar's text-tone script, relative to the XDG config directory (task 11.14;
/// analysis §7.8). The dotfiles deploy `eww/scripts` as a directory link into the
/// repo, so the live path reaches the repo copy without the app resolving the repo.
const BAR_TEXT_TONE_SCRIPT_RELATIVE: &[&str] = &["eww", "scripts", "bar-text-tone"];

/// The dotfiles palette source located behind a deployed config symlink (R3.2,
/// R8.5).
///
/// Its presence gates the palette switcher (task 6.3); its [`repo_root`](Self::repo_root)
/// is the anchor tasks 3.7/6.3 use to scan schemes and task 4.5 uses to run
/// the generator. All three paths are held resolved so consumers never re-derive
/// them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaletteSource {
    /// The dotfiles repository root (the canonicalized directory the deployed
    /// config symlink resolves into).
    repo_root: PathBuf,
    /// The `colors/` directory holding one file per scheme (task 6.3 scans
    /// it; task 3.7 reads a scheme's swatch from it).
    colors_dir: PathBuf,
    /// The `scripts/generate-colors` generator the Apply pipeline runs on a
    /// palette change (task 4.5).
    generate_colors: PathBuf,
}

impl PaletteSource {
    /// The dotfiles repository root.
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// The `colors/` directory of scheme files.
    pub fn colors_dir(&self) -> &Path {
        &self.colors_dir
    }

    /// The `scripts/generate-colors` generator.
    pub fn generate_colors(&self) -> &Path {
        &self.generate_colors
    }
}

/// The raw, injectable inputs a single detection pass consumes (R6.1).
///
/// A production run builds this with [`DetectionInputs::from_system`]; a test
/// constructs it directly to drive any branch deterministically. See the module
/// docs for how each field is turned into a capability.
#[derive(Clone, Debug)]
pub struct DetectionInputs {
    /// The `$PATH` to scan for target binaries (colon-separated directories).
    /// `None` (no `$PATH` in the environment) disables the scan, so every binary
    /// reads as absent.
    pub path: Option<String>,
    /// The names of currently running processes, as **untruncated** executable
    /// names (the basename of argv[0] from `/proc/<pid>/cmdline`) — the real
    /// detector fills this from a procfs scan, a test injects a fixed set. Daemon
    /// and portal liveness are decided against it by exact match. argv[0] is used
    /// rather than `/proc/<pid>/comm` because the kernel truncates `comm` to 15
    /// bytes, which would make the whole `xdg-desktop-portal-*` family
    /// indistinguishable (see [`SETTINGS_PORTAL_BACKENDS`]).
    pub running_processes: Vec<String>,
    /// The Hyprland IPC socket path whose existence signals a live compositor, or
    /// `None` when it cannot be located from the environment. Its existence is
    /// checked against the real filesystem.
    pub hyprland_socket: Option<PathBuf>,
    /// The deployed config path used to detect the dotfiles repo root (R8.5),
    /// conventionally `~/.config/hypr/colors.conf`. It must be a symlink into the
    /// repo for the repo-only sources — the palette source and the laptop-display
    /// toggle script (task 11.8) — to be considered present.
    pub palette_config_anchor: PathBuf,
    /// The live XDG paths of backing config files to check for readability (R4.4).
    /// Each unreadable path is recorded and logged at `warn`.
    pub config_paths: Vec<PathBuf>,
    /// The captured stdout of `hyprctl status`, or `None` when it was not gathered
    /// (no live IPC socket, `hyprctl` not on `$PATH`) or the command failed. Its
    /// `configProvider:` line, when present, is the authoritative config provider
    /// of the running session (task 11.2); it is consulted only when
    /// [`Self::hyprland_socket`] exists.
    pub hyprctl_status: Option<String>,
    /// The live XDG path of Hyprland's Lua config, conventionally
    /// `~/.config/hypr/hyprland.lua`. When no running session reports its provider,
    /// the file's existence decides it: Hyprland itself prefers this file whenever it
    /// exists (analysis §7.1).
    pub hyprland_lua_config: PathBuf,
    /// The captured replies of the notifd probes (task 11.7); all `None` when they
    /// were not gathered (no `busctl` on `$PATH`), which resolves to
    /// [`NotifdStatus::Absent`].
    pub notifd_replies: NotifdReplies,
    /// The live path of notifd's systemd user unit, conventionally
    /// `~/.config/systemd/user/notifd.service` (the dotfiles link it there). Its
    /// existence marks notifd as installed when the daemon is not running.
    pub notifd_unit_file: PathBuf,
    /// The live path of the eww bar's text-tone script, conventionally
    /// `~/.config/eww/scripts/bar-text-tone` (task 11.14). When it is an executable
    /// file the reload table runs it with `--apply` after a wallpaper change, so the
    /// bar re-picks its text colour from the new image (architecture §6, analysis
    /// §7.8). It is addressed by its XDG path like every other file the app uses,
    /// not through the dotfiles repo, because it ships inside the deployed eww
    /// config directory.
    pub bar_text_tone_script: PathBuf,
}

impl DetectionInputs {
    /// Gathers the real inputs from the running system (the production entry
    /// point, called at startup and on manual refresh, architecture §8, R4.3).
    ///
    /// `config_paths` are the backing-config XDG paths to check for readability;
    /// the caller (startup, task 5.4) resolves them, since path resolution is its
    /// concern. The `$PATH`, the procfs process list, the Hyprland socket path, the
    /// palette anchor, and the Lua config path are all read from the environment
    /// here. Every read is best-effort: a missing environment variable or an
    /// unreadable `/proc` simply yields an empty/`None` input, which
    /// [`Capabilities::detect`] turns into the "absent" capability rather than an
    /// error.
    ///
    /// `runner` is used for `hyprctl status`, issued only when the Hyprland IPC
    /// socket exists and `hyprctl` is on `$PATH`, and for the two notifd probes
    /// ([`NotifdReplies::gather`]), issued only when `busctl` is on `$PATH` — so a
    /// session with neither spawns nothing; a failed query degrades to `None`.
    pub fn from_system(runner: &dyn CommandRunner, config_paths: Vec<PathBuf>) -> Self {
        let path = std::env::var("PATH").ok();
        let notifd_replies = if is_on_path(path.as_deref(), Binary::Busctl.command_name()) {
            NotifdReplies::gather(runner)
        } else {
            NotifdReplies::default()
        };
        let hyprland_socket = hyprland_socket_path();
        let hyprctl_status = if hyprland_socket.as_deref().is_some_and(Path::exists)
            && is_on_path(path.as_deref(), Binary::Hyprctl.command_name())
        {
            query_hyprctl_status(runner)
        } else {
            None
        };
        DetectionInputs {
            path,
            running_processes: scan_procfs_process_names(),
            hyprland_socket,
            palette_config_anchor: default_config_path(&["hypr", "colors.conf"]),
            config_paths,
            hyprctl_status,
            hyprland_lua_config: default_config_path(&["hypr", "hyprland.lua"]),
            notifd_replies,
            notifd_unit_file: default_config_path(&["systemd", "user", "notifd.service"]),
            bar_text_tone_script: default_config_path(BAR_TEXT_TONE_SCRIPT_RELATIVE),
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
pub struct Capabilities {
    /// Target binaries found on `$PATH`.
    present_binaries: BTreeSet<Binary>,
    /// Daemons found running by the procfs scan.
    live_daemons: BTreeSet<Daemon>,
    /// Whether the Hyprland IPC socket exists (the compositor is live).
    hyprland_ipc: bool,
    /// Whether a live theme-restyle path (settings portal or dconf backend) is
    /// available (R2.2).
    settings_portal: bool,
    /// The dotfiles repository root resolved behind the deployed config (R8.5), or
    /// `None` when there is no repo behind it. Kept on its own, beside the sources
    /// derived from it, because a repo can be present while its palette source is
    /// incomplete; the Theme page uses it to spot a `settings.ini` that is no longer
    /// linked to its repo copy (task 11.12).
    repo_root: Option<PathBuf>,
    /// The dotfiles palette source, or `None` when there is no repo behind the
    /// config (R3.2, R8.5).
    palette_source: Option<PaletteSource>,
    /// The dotfiles' `scripts/hypr-toggle-laptop-display`, or `None` when there is
    /// no repo behind the config or the script is missing or not executable — the
    /// gate of the Display page's laptop-panel switch (task 11.8; R4.2, R8.5).
    laptop_toggle_script: Option<PathBuf>,
    /// The eww bar's `bar-text-tone` script at its XDG path, or `None` when it is
    /// missing, not executable or not addressable as UTF-8 — the gate of the
    /// wallpaper follow-up reload (task 11.14).
    bar_text_tone_script: Option<PathBuf>,
    /// Backing config paths that could not be read (missing or unreadable, R4.4).
    unreadable_configs: BTreeSet<PathBuf>,
    /// The Hyprland config provider in use, the guard for the Lua-only input and
    /// cursor writes (task 11.2).
    hypr_config_provider: HyprConfigProvider,
    /// Whether the notifd notification daemon runs (and which version) or is
    /// installed, the gate of the Notifications page (task 11.7).
    notifd: NotifdStatus,
}

impl Capabilities {
    /// Runs one detection pass over `inputs` (R4.1).
    ///
    /// This is the pure, re-runnable routine: a manual refresh (R4.3) re-gathers
    /// [`DetectionInputs`] and calls it again. It performs the binary scan, daemon
    /// and Hyprland liveness checks, the config-provider resolution, the
    /// settings-portal probe, palette-source detection, and config-readability
    /// checks, logging absent page-gating items at `info` and unreadable configs at
    /// `warn` (R4.2/R4.4/R7.3), then returns the plain [`Capabilities`]. It never
    /// fails: every probe degrades to "absent".
    pub fn detect(inputs: &DetectionInputs) -> Capabilities {
        let present_binaries = scan_binaries(inputs.path.as_deref());
        log_binary_results(&present_binaries);

        let live_daemons = detect_daemons(&inputs.running_processes);
        if live_daemons.contains(&Daemon::Xsettingsd) {
            // Not a hidden feature but a user-visible note on the Theme page, so it
            // earns an `info` like the live-restyle downgrade below (R7.3).
            tracing::info!(
                "xsettingsd is running: an external GTK-settings writer (usually KDE's \
                 kded6 gtkconfig module) is active and may rewrite settings.ini and the \
                 GTK gsettings keys (analysis §7.7)"
            );
        }

        // Hyprland liveness is the socket's existence on the real filesystem; a
        // `None` anchor (no socket path resolvable) is simply "not live".
        let hyprland_ipc = inputs.hyprland_socket.as_deref().is_some_and(Path::exists);
        tracing::debug!(hyprland_ipc, "Hyprland IPC socket liveness");

        let hypr_config_provider = detect_config_provider(
            hyprland_ipc,
            inputs.hyprctl_status.as_deref(),
            &inputs.hyprland_lua_config,
        );
        if hypr_config_provider != HyprConfigProvider::Lua {
            tracing::info!(
                provider = %hypr_config_provider,
                "Hyprland is not using its Lua config provider; the Input page and the \
                 Hyprland-side cursor write are hidden (analysis §8.6, R4.2)"
            );
        }

        let settings_portal = detect_settings_portal(&inputs.running_processes, &present_binaries);
        if !settings_portal {
            // Not a hidden page, but a user-visible behaviour change (theme edits
            // take effect at next launch rather than live), so it earns an `info`.
            tracing::info!(
                "no settings portal or dconf backend detected; live theme restyle \
                 unavailable — changes take effect at next launch (R2.2)"
            );
        }

        let repo_root = resolve_repo_root(&inputs.palette_config_anchor);
        let palette_source = detect_palette_source(repo_root.as_deref());
        let laptop_toggle_script = detect_laptop_toggle_script(repo_root.as_deref());

        let bar_text_tone_script = detect_bar_text_tone_script(&inputs.bar_text_tone_script);

        let unreadable_configs = detect_unreadable_configs(&inputs.config_paths);

        let notifd =
            NotifdStatus::resolve(&inputs.notifd_replies, inputs.notifd_unit_file.exists());
        log_notifd_status(notifd);

        let capabilities = Capabilities {
            present_binaries,
            live_daemons,
            hyprland_ipc,
            settings_portal,
            repo_root,
            palette_source,
            laptop_toggle_script,
            bar_text_tone_script,
            unreadable_configs,
            hypr_config_provider,
            notifd,
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
    /// [`Capabilities::detect`] it runs no probes and logs nothing. Its config
    /// provider is [`HyprConfigProvider::Hyprlang`], the value that keeps the
    /// Lua-only controls hidden, and notifd is [`NotifdStatus::Absent`].
    pub fn absent() -> Capabilities {
        Capabilities {
            present_binaries: BTreeSet::new(),
            live_daemons: BTreeSet::new(),
            hyprland_ipc: false,
            settings_portal: false,
            repo_root: None,
            palette_source: None,
            laptop_toggle_script: None,
            bar_text_tone_script: None,
            unreadable_configs: BTreeSet::new(),
            hypr_config_provider: HyprConfigProvider::Hyprlang,
            notifd: NotifdStatus::Absent,
        }
    }

    /// Whether `binary` was found on `$PATH`.
    pub fn has_binary(&self, binary: Binary) -> bool {
        self.present_binaries.contains(&binary)
    }

    /// Whether any audio control client is on `$PATH` — WirePlumber's `wpctl` or
    /// PulseAudio's `pactl` (R3.1).
    ///
    /// This is the general "is there some audio client at all" query. Note the v1
    /// Sound page (task 6.2) does **not** gate on this: its device load (`pw-dump`,
    /// falling back to `wpctl status`) and its controls speak only `wpctl`/`pw-dump`,
    /// so it is gated on [`Binary::Wpctl`] specifically — a `pactl`-only host would
    /// otherwise render a dead, inert page, which R4.2 forbids. `pactl`-only support is
    /// out of v1 scope.
    pub fn audio_available(&self) -> bool {
        self.has_binary(Binary::Wpctl) || self.has_binary(Binary::Pactl)
    }

    /// Whether `daemon` was found running by the procfs scan (task 4.4 gates a
    /// reload on this).
    pub fn is_daemon_live(&self, daemon: Daemon) -> bool {
        self.live_daemons.contains(&daemon)
    }

    /// Whether the Hyprland IPC socket exists — i.e. the compositor is live.
    pub fn hyprland_ipc_live(&self) -> bool {
        self.hyprland_ipc
    }

    /// Whether a Hyprland reload can be issued: the `hyprctl` client is installed
    /// *and* the compositor is live (architecture §4). The reload table (task 4.4)
    /// uses this to decide whether to run `hyprctl reload`.
    pub fn hyprland_reloadable(&self) -> bool {
        self.has_binary(Binary::Hyprctl) && self.hyprland_ipc
    }

    /// The Hyprland config provider in use (task 11.2; analysis §8.6).
    ///
    /// Resolved from the running session's `hyprctl status` when the IPC socket
    /// exists, else from whether `~/.config/hypr/hyprland.lua` exists, else
    /// [`HyprConfigProvider::Hyprlang`]. The Input page and the Hyprland-side cursor
    /// write require [`HyprConfigProvider::Lua`] and are hidden otherwise.
    pub fn hypr_config_provider(&self) -> HyprConfigProvider {
        self.hypr_config_provider
    }

    /// Whether the notifd notification daemon runs, and which version, or is only
    /// installed (task 11.7; analysis §8.2).
    ///
    /// The Notifications page is offered when it runs or is installed
    /// ([`NotifdStatus::is_present`]); the `GetState`/`Reload` methods and the
    /// file-backed notification rows need a running daemon ≥ 1.1
    /// ([`NotifdStatus::supports_state_and_reload`]).
    pub fn notifd(&self) -> NotifdStatus {
        self.notifd
    }

    /// Whether a live theme-restyle path is available — the GTK settings portal is
    /// running or a dconf GSettings backend is present (R2.2). Gates whether the
    /// Theme page may claim a live restyle versus "takes effect at next launch".
    pub fn settings_portal_available(&self) -> bool {
        self.settings_portal
    }

    /// The dotfiles repository root resolved behind the deployed config symlink, or
    /// `None` when there is no repo behind the config (R8.5).
    ///
    /// The palette source and the laptop-display toggle script are derived from this
    /// root; it is exposed on its own for consumers that only need to know where the
    /// repo is — the Theme page compares each live `settings.ini` with its repo copy
    /// under it to report a file that is no longer linked (task 11.12, R5.6). It is
    /// equal to [`PaletteSource::repo_root`] of [`Self::palette_source`] whenever that is
    /// `Some`; it can be `Some` while the palette source is `None` (an incomplete repo).
    pub fn dotfiles_repo_root(&self) -> Option<&Path> {
        self.repo_root.as_deref()
    }

    /// The detected dotfiles palette source, or `None` when the palette switcher
    /// must be hidden (R3.2, R8.5).
    pub fn palette_source(&self) -> Option<&PaletteSource> {
        self.palette_source.as_ref()
    }

    /// The dotfiles' laptop-display toggle script, or `None` when the Display page's
    /// laptop-panel switch must be hidden (task 11.8; R4.2, R8.5).
    ///
    /// Resolved from the same repo root as [`Self::palette_source`] and present only
    /// when `<repo>/scripts/hypr-toggle-laptop-display` is an executable file. The
    /// app runs it instead of re-implementing it, because the script owns every step
    /// of a toggle — the live `hl.monitor` call, the hotplug watcher's
    /// `/tmp/hypr-laptop-display-forced` override and the eww bar relaunch
    /// (analysis §7.3, §8.1).
    pub fn laptop_toggle_script(&self) -> Option<&Path> {
        self.laptop_toggle_script.as_deref()
    }

    /// The eww bar's text-tone script, or `None` when the wallpaper follow-up must be
    /// skipped (task 11.14; architecture §6, analysis §7.8).
    ///
    /// Present only when `$XDG_CONFIG_HOME/eww/scripts/bar-text-tone` (falling back
    /// to `~/.config/…`) is an executable file with a UTF-8 path. After a wallpaper
    /// write the reload table runs it with `--apply`, which re-scores the bar's text
    /// colour against the new image and pushes it with `eww update`; the run is
    /// additionally gated on eww running, because there is no bar to update
    /// otherwise.
    pub fn bar_text_tone_script(&self) -> Option<&Path> {
        self.bar_text_tone_script.as_deref()
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
    pub fn config_readable(&self, path: &Path) -> bool {
        !self.unreadable_configs.contains(path)
    }

    /// The backing config paths that could not be read (missing or unreadable),
    /// for the UI to hide the affected controls (R4.4).
    pub fn unreadable_configs(&self) -> &BTreeSet<PathBuf> {
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
            dotfiles_repo = self.repo_root.is_some(),
            palette_source = self.palette_source.is_some(),
            laptop_toggle_script = self.laptop_toggle_script.is_some(),
            bar_text_tone_script = self.bar_text_tone_script.is_some(),
            unreadable_configs = self.unreadable_configs.len(),
            hypr_config_provider = %self.hypr_config_provider,
            notifd = %self.notifd,
            "capabilities detection complete"
        );
    }
}

// Available to in-crate unit tests via `cfg(test)` and to the integration suites
// in `tests/` (separate crates, where `cfg(test)` does not reach) via the
// `testing` feature — never in a release build.
#[cfg(any(test, feature = "testing"))]
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
    /// [`Capabilities::detect`] path instead. The config provider is fixed to
    /// [`HyprConfigProvider::Lua`], the target dotfiles' provider; a test of the
    /// hyprlang guard switches it with [`Self::with_hypr_config_provider_for_tests`].
    /// notifd is [`NotifdStatus::Absent`]; [`Self::with_notifd_for_tests`] sets it.
    pub fn for_tests(
        binaries: &[Binary],
        live_daemons: &[Daemon],
        hyprland_ipc: bool,
    ) -> Capabilities {
        Capabilities {
            present_binaries: binaries.iter().copied().collect(),
            live_daemons: live_daemons.iter().copied().collect(),
            hyprland_ipc,
            settings_portal: false,
            repo_root: None,
            palette_source: None,
            laptop_toggle_script: None,
            bar_text_tone_script: None,
            unreadable_configs: BTreeSet::new(),
            hypr_config_provider: HyprConfigProvider::Lua,
            notifd: NotifdStatus::Absent,
        }
    }

    /// Returns `self` with the given notifd status, for tests of the Notifications
    /// page gate (task 11.7) — builder-style, like
    /// [`Self::with_hypr_config_provider_for_tests`].
    pub fn with_notifd_for_tests(mut self, notifd: NotifdStatus) -> Capabilities {
        self.notifd = notifd;
        self
    }

    /// Returns `self` with the given bar text-tone script marked present, for tests of
    /// the wallpaper follow-up reload (task 11.14) — builder-style, like
    /// [`Self::with_notifd_for_tests`]. The path is taken as given; a test of the
    /// presence check itself runs [`Capabilities::detect`] instead.
    pub fn with_bar_text_tone_script_for_tests(mut self, script: PathBuf) -> Capabilities {
        self.bar_text_tone_script = Some(script);
        self
    }

    /// Returns `self` with the given Hyprland config provider, for tests of the
    /// provider guard (task 11.2) — builder-style, like
    /// [`Self::with_palette_source_for_tests`].
    pub fn with_hypr_config_provider_for_tests(
        mut self,
        provider: HyprConfigProvider,
    ) -> Capabilities {
        self.hypr_config_provider = provider;
        self
    }

    /// Returns `self` with the dotfiles palette source marked present, for tests that
    /// need the palette-gated path — such as the Theme category's `palette_source`
    /// arm (task 5.1) — without building a real symlinked-repo filesystem fixture.
    ///
    /// The paths are placeholders: a test that needs the *real* detection logic
    /// exercises [`Capabilities::detect`] against a temp-dir fixture instead. It is
    /// builder-style so it chains onto [`Self::for_tests`], keeping that constructor's
    /// signature (and its "absent" defaults) unchanged.
    pub fn with_palette_source_for_tests(mut self) -> Capabilities {
        // Keep the invariant of `dotfiles_repo_root`: the repo root and the palette
        // source's root are the same path whenever the source is present.
        self.repo_root = Some(PathBuf::from("/test/dotfiles"));
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

    for &binary in Binary::ALL {
        if is_on_path(Some(path), binary.command_name()) {
            present.insert(binary);
        }
    }
    present
}

/// Whether an executable named `name` is in any directory of the `$PATH` string
/// `path` (see [`scan_binaries`] for the empty-entry rule). `None` (no `$PATH`)
/// finds nothing.
fn is_on_path(path: Option<&str>, name: &str) -> bool {
    path.is_some_and(|path| {
        path.split(':')
            .filter(|dir| !dir.is_empty())
            .any(|dir| is_executable_file(&Path::new(dir).join(name)))
    })
}

/// Resolves the Hyprland config provider (task 11.2; analysis §7.1, §8.6).
///
/// The order is the one the compositor itself implies:
///
/// 1. When the IPC socket is live and the captured `hyprctl status` reply carries
///    a recognisable `configProvider:` line, that report wins — it is what the
///    running session actually loaded.
/// 2. Otherwise (no session, `hyprctl` missing or failing, or a reply without the
///    line), the provider is [`HyprConfigProvider::Lua`] when `lua_config` exists:
///    Hyprland ≥ 0.55 loads `hyprland.lua` whenever it is present.
/// 3. Otherwise [`HyprConfigProvider::Hyprlang`].
///
/// `Path::exists` follows symlinks, so a deployed `hyprland.lua` symlink into the
/// dotfiles repo counts and a dangling one does not. Nothing here can fail.
fn detect_config_provider(
    hyprland_ipc: bool,
    hyprctl_status: Option<&str>,
    lua_config: &Path,
) -> HyprConfigProvider {
    if hyprland_ipc {
        if let Some(provider) = hyprctl_status.and_then(parse_config_provider) {
            tracing::debug!(%provider, "config provider reported by `hyprctl status`");
            return provider;
        }
    }
    let provider = if lua_config.exists() {
        HyprConfigProvider::Lua
    } else {
        HyprConfigProvider::Hyprlang
    };
    tracing::debug!(
        %provider,
        lua_config = %lua_config.display(),
        "config provider inferred from whether the Lua config exists"
    );
    provider
}

/// Parses the `configProvider: <name>` line of a `hyprctl status` reply.
///
/// The reply is a few `key: value` lines (`configProvider: lua`, `backend: drm`
/// on Hyprland 0.56.2). Returns `None` when the line is absent or names a
/// provider this app does not know, so [`detect_config_provider`] falls back to
/// the file check rather than guessing.
fn parse_config_provider(status: &str) -> Option<HyprConfigProvider> {
    let value = status
        .lines()
        .find_map(|line| line.trim().strip_prefix(CONFIG_PROVIDER_PREFIX))?
        .trim();
    if value.eq_ignore_ascii_case("lua") {
        Some(HyprConfigProvider::Lua)
    } else if value.eq_ignore_ascii_case("hyprlang") {
        Some(HyprConfigProvider::Hyprlang)
    } else {
        tracing::debug!(
            value,
            "`hyprctl status` names an unknown config provider; ignoring it"
        );
        None
    }
}

/// Runs `hyprctl status` through `runner` and returns its stdout, or `None` when
/// the command could not run or reported failure (logged at `debug`; detection then
/// falls back to the Lua-config file check, so this is never an error).
fn query_hyprctl_status(runner: &dyn CommandRunner) -> Option<String> {
    let command = Command::new(Binary::Hyprctl.command_name()).arg("status");
    match runner.run(&command) {
        Ok(output) if output.success() => {
            Some(String::from_utf8_lossy(output.stdout()).into_owned())
        }
        Ok(output) => {
            tracing::debug!(exit_code = ?output.code(), "`hyprctl status` reported failure");
            None
        }
        Err(error) => {
            tracing::debug!(%error, "`hyprctl status` could not be run");
            None
        }
    }
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

/// Logs the notifd status: an absent daemon at `info` as a hidden item (R4.2), a
/// stopped or unversioned one at `info` because the page then offers less, and a
/// running versioned one at `debug`.
fn log_notifd_status(status: NotifdStatus) {
    match status {
        NotifdStatus::Absent => tracing::info!(
            "notifd not found (its D-Bus name has no owner and no notifd.service unit is \
             installed, or the session bus could not be queried); the Notifications page is \
             hidden (R4.2)"
        ),
        NotifdStatus::Installed => tracing::info!(
            "notifd is installed but not running; it starts with the next notification, and \
             the Notifications page reports it as not running until then"
        ),
        NotifdStatus::Running { version: None } => tracing::info!(
            "notifd is running but did not report a version; treating it as older than 1.1"
        ),
        NotifdStatus::Running {
            version: Some(version),
        } => tracing::debug!(%version, "notifd is running"),
    }
}

/// Detects which [`Daemon`]s are running from an already-scanned list of process names
/// (the walk itself is [`scan_procfs_process_names`]).
fn detect_daemons(running: &[String]) -> BTreeSet<Daemon> {
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

/// Resolves the dotfiles repository root behind the config `anchor` (R8.5).
///
/// The anchor must be a **symlink** — the deployment method for this dotfiles setup
/// (analysis §1). A plain file means there is no repo behind it; when it is a
/// symlink, it is canonicalized to the real repo file and the root is derived by
/// stripping the known [`REPO_ANCHOR_RELATIVE`] tail, after verifying the resolved
/// path really ends in it. Any filesystem failure (a missing anchor, a dangling
/// symlink, an unexpected resolved layout) degrades to `None`.
///
/// Two repo-only controls hang off this root and are hidden like a missing app when
/// it is absent: the palette switcher ([`detect_palette_source`]) and the
/// laptop-display toggle ([`detect_laptop_toggle_script`]). Every reason for a
/// missing root is therefore logged here, once, at `info` — the "hidden item"
/// convention for the rest of detection (R4.2) — naming both controls.
fn resolve_repo_root(anchor: &Path) -> Option<PathBuf> {
    // `symlink_metadata` does not follow the link, so it tells us whether the
    // anchor *itself* is a symlink — the signal that a dotfiles repo is deployed
    // behind it. A plain file (or a missing/unreadable anchor) means no repo.
    match std::fs::symlink_metadata(anchor) {
        Ok(metadata) if metadata.file_type().is_symlink() => {}
        Ok(_) => {
            tracing::info!(
                anchor = %anchor.display(),
                "palette config is not a symlink into a dotfiles repo; the palette switcher \
                 and the laptop-display toggle are hidden (R3.2/R4.2/R8.5)"
            );
            return None;
        }
        Err(error) => {
            tracing::info!(
                anchor = %anchor.display(),
                %error,
                "palette config anchor is absent or unreadable; the palette switcher and \
                 the laptop-display toggle are hidden (R3.2/R4.2/R8.5)"
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
                "palette config symlink could not be resolved (dangling?); the palette \
                 switcher and the laptop-display toggle are hidden (R3.2/R4.2/R8.5)"
            );
            return None;
        }
    };

    // Derive the repo root by stripping the known repo-relative tail. Verifying the
    // tail matches (rather than blindly dropping three components) guards against a
    // symlink that points somewhere with an unexpected shape.
    let relative: PathBuf = REPO_ANCHOR_RELATIVE.iter().collect();
    let repo_root = canonical
        .ends_with(&relative)
        .then(|| canonical.ancestors().nth(REPO_ANCHOR_RELATIVE.len()))
        .flatten()
        .map(Path::to_path_buf);
    if repo_root.is_none() {
        tracing::info!(
            resolved = %canonical.display(),
            "palette config resolved outside the expected repo layout; the palette \
             switcher and the laptop-display toggle are hidden (R3.2/R4.2/R8.5)"
        );
    }
    repo_root
}

/// Detects the dotfiles palette source under `repo_root` (R3.2, R8.5).
///
/// The source is present only when both `colors/` and `scripts/generate-colors`
/// exist under the root resolved by [`resolve_repo_root`]; an incomplete repo hides
/// the palette switcher exactly like a missing app, logged at `info` (R4.2). A
/// `None` root was already logged by [`resolve_repo_root`].
fn detect_palette_source(repo_root: Option<&Path>) -> Option<PaletteSource> {
    let repo_root = repo_root?;
    let colors_dir = repo_root.join("colors");
    let generate_colors = repo_root.join("scripts").join("generate-colors");
    if colors_dir.is_dir() && generate_colors.is_file() {
        tracing::debug!(repo_root = %repo_root.display(), "palette source detected");
        Some(PaletteSource {
            repo_root: repo_root.to_path_buf(),
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

/// Detects the dotfiles' laptop-display toggle script under `repo_root` (task
/// 11.8; R4.2, R8.5; analysis §7.3).
///
/// The Display page's laptop-panel switch runs this script rather than
/// re-implementing it, so the switch exists only when the script does: the path
/// [`LAPTOP_TOGGLE_SCRIPT_RELATIVE`] under the root resolved by
/// [`resolve_repo_root`] must be an executable regular file. Anything else hides the
/// switch like a missing app, logged at `info` (R4.2); the rest of the Display page
/// is unaffected. A `None` root was already logged by [`resolve_repo_root`].
fn detect_laptop_toggle_script(repo_root: Option<&Path>) -> Option<PathBuf> {
    let repo_root = repo_root?;
    let script: PathBuf = std::iter::once(repo_root)
        .chain(LAPTOP_TOGGLE_SCRIPT_RELATIVE.iter().map(Path::new))
        .collect();
    // A command's program is a `String`, so a path that is not valid UTF-8 could only
    // be run after a lossy conversion — that is, a different path. Treat it as not
    // runnable, so detection never reports a script the Display page cannot run.
    if script.to_str().is_none() {
        tracing::info!(
            script = %script.display(),
            "the dotfiles laptop-display toggle script path is not valid UTF-8, so it \
             cannot be run; the laptop-display toggle is hidden (R4.2)"
        );
        None
    } else if is_executable_file(&script) {
        tracing::debug!(script = %script.display(), "laptop-display toggle script detected");
        Some(script)
    } else {
        tracing::info!(
            script = %script.display(),
            "the dotfiles laptop-display toggle script is missing or not executable; the \
             laptop-display toggle is hidden (R4.2)"
        );
        None
    }
}

/// Decides whether the eww bar's text-tone script at its live XDG path can be run
/// (task 11.14; architecture §6, analysis §7.8).
///
/// Present only as an executable regular file (a symlink is followed, since the
/// dotfiles link the whole `eww/scripts` directory into the repo) whose path is
/// valid UTF-8 — a command's program is a `String`, so a lossy conversion would name
/// a different file. Anything else skips the wallpaper follow-up, logged at `info`:
/// it is a best-effort refinement of the bar, not a control, so nothing on a page
/// is hidden by its absence.
fn detect_bar_text_tone_script(script: &Path) -> Option<PathBuf> {
    // Presence first, so a missing script under a non-UTF-8 config directory is
    // reported as missing — the common reason — rather than as a path problem.
    if !is_executable_file(script) {
        tracing::info!(
            script = %script.display(),
            "the eww bar-text-tone script is missing or not executable; the bar text \
             colour is not refreshed after a wallpaper change"
        );
        None
    } else if script.to_str().is_none() {
        tracing::info!(
            script = %script.display(),
            "the eww bar-text-tone script path is not valid UTF-8, so it cannot be run; \
             the bar text colour is not refreshed after a wallpaper change"
        );
        None
    } else {
        tracing::debug!(script = %script.display(), "eww bar-text-tone script detected");
        Some(script.to_path_buf())
    }
}

/// Checks each config path for readability, returning the ones that failed (R4.4).
///
/// A path that cannot be read — missing, permission-revoked, or otherwise
/// inaccessible — is logged at `warn` and returned so the UI hides its controls.
/// Only the path and OS error are logged, never the file's contents (R7.3). The
/// full bytes are read (config files are tiny) but discarded; structural
/// parseability is validated later by the page's own parser.
fn detect_unreadable_configs(paths: &[PathBuf]) -> BTreeSet<PathBuf> {
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

/// The live XDG path of a config file, given its components below the config
/// directory (R8.5) — used for the palette anchor (`hypr/colors.conf`) and the Lua
/// config (`hypr/hyprland.lua`).
///
/// Prefers `$XDG_CONFIG_HOME/<relative>`, falling back to
/// `$HOME/.config/<relative>`. When neither variable is set the returned bare file
/// name is relative to the working directory and simply misses (the palette source
/// degrades to absent, the Lua config reads as absent), so this never has to fail.
fn default_config_path(relative: &[&str]) -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|config_home| !config_home.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".config")));
    match base {
        Some(base) => relative.iter().fold(base, |path, part| path.join(part)),
        None => relative.last().map_or_else(PathBuf::new, PathBuf::from),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::command::{CommandOutput, MockCommandRunner};
    use std::fs;
    use std::path::Path;

    /// Detection inputs with everything absent — a blank slate a test tweaks. The
    /// palette anchor and the Lua config point at nonexistent paths so the palette
    /// source is absent and the config provider falls back to hyprlang by default.
    fn base_inputs() -> DetectionInputs {
        DetectionInputs {
            path: None,
            running_processes: Vec::new(),
            hyprland_socket: None,
            palette_config_anchor: PathBuf::from("/nonexistent/settings4000/hypr/colors.conf"),
            config_paths: Vec::new(),
            hyprctl_status: None,
            hyprland_lua_config: PathBuf::from("/nonexistent/settings4000/hypr/hyprland.lua"),
            notifd_replies: NotifdReplies::default(),
            notifd_unit_file: PathBuf::from(
                "/nonexistent/settings4000/systemd/user/notifd.service",
            ),
            bar_text_tone_script: PathBuf::from(
                "/nonexistent/settings4000/eww/scripts/bar-text-tone",
            ),
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
    fn path_scan_searches_every_path_directory() {
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
    fn daemon_liveness_follows_the_injected_process_list() {
        // Accept criterion (liveness injectable): a supplied process list flips
        // daemon liveness without a live daemon.
        let mut inputs = base_inputs();
        inputs.running_processes = vec!["hyprpaper".to_string(), "eww".to_string()];
        let caps = Capabilities::detect(&inputs);

        assert!(caps.is_daemon_live(Daemon::Hyprpaper));
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
    fn a_running_settings_backend_signals_the_portal() {
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
    fn hyprland_reloadable_needs_the_client_and_a_live_socket() {
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

    #[test]
    fn xsettingsd_detection_is_logged_at_info() {
        // Both its presence (logged) and its absence (silent) are checked.
        // Task 11.12 (analysis §7.7): xsettingsd is the trace of KDE's kded6 gtkconfig
        // module managing GTK settings. It is scanned like the other daemons and its
        // presence logged at info, so the Theme page note has a logged cause.
        let mut inputs = base_inputs();
        inputs.running_processes = vec!["xsettingsd".to_string(), "eww".to_string()];
        let mut caps = None;
        let events = crate::testing::capture_logs(|| caps = Some(Capabilities::detect(&inputs)));
        let caps = caps.expect("detect ran");
        assert!(caps.is_daemon_live(Daemon::Xsettingsd));
        assert!(
            events
                .iter()
                .any(|event| event.level == tracing::Level::INFO
                    && event.message.contains("xsettingsd is running")),
            "a running xsettingsd must be logged at info, got {events:?}"
        );

        // Not running: not live, and nothing is said about it.
        let mut inputs = base_inputs();
        inputs.running_processes = vec!["eww".to_string()];
        let events = crate::testing::capture_logs(|| {
            let caps = Capabilities::detect(&inputs);
            assert!(!caps.is_daemon_live(Daemon::Xsettingsd));
        });
        assert!(
            !events
                .iter()
                .any(|event| event.message.contains("xsettingsd")),
            "an absent xsettingsd must not be logged, got {events:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_repo_root_does_not_need_a_complete_palette_source() {
        // Task 11.12: the Theme page compares settings.ini with its repo copy under
        // the detected root, which must not depend on the palette source being
        // complete (a repo without colors/ or the generator still has its configs).
        let root = tempfile::tempdir().expect("temp dir");
        let (anchor, repo) = build_repo_with_symlinked_anchor(root.path(), false, false);
        let mut inputs = base_inputs();
        inputs.palette_config_anchor = anchor;
        let caps = Capabilities::detect(&inputs);

        assert!(caps.palette_source().is_none());
        let expected_root = fs::canonicalize(&repo).expect("canonicalize the repo root");
        assert_eq!(caps.dotfiles_repo_root(), Some(expected_root.as_path()));

        // No symlink behind the anchor: no repo root at all.
        let caps = Capabilities::detect(&base_inputs());
        assert_eq!(caps.dotfiles_repo_root(), None);
        assert_eq!(Capabilities::absent().dotfiles_repo_root(), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_complete_linked_repo_yields_the_palette_source() {
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
    fn an_unexpected_symlink_target_hides_the_palette_source() {
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

    #[cfg(unix)]
    #[test]
    fn an_executable_repo_toggle_script_is_detected() {
        // Task 11.8 (R4.2/R8.5): the laptop-display toggle script is found under the
        // same repo root as the palette source and is present only as an executable
        // file. The palette source's own completeness does not matter to it.
        let root = tempfile::tempdir().expect("temp dir");
        let (anchor, repo) = build_repo_with_symlinked_anchor(root.path(), false, false);
        let scripts = repo.join("scripts");
        fs::create_dir_all(&scripts).expect("create scripts dir");
        write_executable(&scripts, "hypr-toggle-laptop-display");

        let mut inputs = base_inputs();
        inputs.palette_config_anchor = anchor;
        let caps = Capabilities::detect(&inputs);

        let expected = fs::canonicalize(&repo)
            .expect("canonicalize the repo root")
            .join("scripts")
            .join("hypr-toggle-laptop-display");
        assert_eq!(caps.laptop_toggle_script(), Some(expected.as_path()));
        assert!(
            caps.palette_source().is_none(),
            "the incomplete palette source is hidden independently"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_unusable_toggle_script_hides_the_laptop_toggle() {
        // Unusable means missing or not executable.
        use std::os::unix::fs::PermissionsExt;

        // A complete palette repo without the script: the toggle is hidden and the
        // reason is logged at `info` as a hidden item (R4.2).
        let root = tempfile::tempdir().expect("temp dir");
        let (anchor, repo) = build_repo_with_symlinked_anchor(root.path(), true, true);
        let mut inputs = base_inputs();
        inputs.palette_config_anchor = anchor;

        let mut caps = None;
        let events = crate::testing::capture_logs(|| caps = Some(Capabilities::detect(&inputs)));
        let caps = caps.expect("detection ran");
        assert!(caps.laptop_toggle_script().is_none());
        assert!(caps.palette_source().is_some());
        assert!(
            events
                .iter()
                .any(|event| event.level == tracing::Level::INFO
                    && event.message.contains("laptop-display toggle is hidden")),
            "a missing toggle script must be logged at info, got {events:?}"
        );

        // Present but not executable: still hidden, since the app could not run it.
        let script = repo.join("scripts").join("hypr-toggle-laptop-display");
        fs::write(&script, b"#!/bin/sh\n").expect("write the script");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o644))
            .expect("clear the executable bits");
        assert!(
            Capabilities::detect(&inputs)
                .laptop_toggle_script()
                .is_none(),
            "a non-executable script must hide the toggle"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_toggle_script_path_hides_the_laptop_toggle() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        // An executable script under a repo root whose path is not valid UTF-8 could
        // only be run through a lossy conversion, so it is reported absent and the
        // reason is logged at `info` with the other hide reasons (R4.2).
        let root = tempfile::tempdir().expect("temp dir");
        let repo = root.path().join(OsStr::from_bytes(b"dot\xfffiles"));
        let scripts = repo.join("scripts");
        fs::create_dir_all(&scripts).expect("create a non-UTF-8 repo dir");
        write_executable(&scripts, "hypr-toggle-laptop-display");
        assert!(is_executable_file(
            &scripts.join("hypr-toggle-laptop-display")
        ));

        let mut script = None;
        let events =
            crate::testing::capture_logs(|| script = detect_laptop_toggle_script(Some(&repo)));
        assert!(script.is_none(), "a non-UTF-8 path must not be reported");
        assert!(
            events
                .iter()
                .any(|event| event.level == tracing::Level::INFO
                    && event.message.contains("not valid UTF-8")
                    && event.message.contains("laptop-display toggle is hidden")),
            "the reason must be logged at info, got {events:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_executable_bar_text_tone_script_is_detected() {
        // Task 11.14: the script is looked up at its live XDG path, independently of
        // any dotfiles repo (none is behind `base_inputs`), and reported as given.
        let root = tempfile::tempdir().expect("temp dir");
        let scripts = root.path().join("eww").join("scripts");
        fs::create_dir_all(&scripts).expect("create the eww scripts dir");
        let script = write_executable(&scripts, "bar-text-tone");

        let mut inputs = base_inputs();
        inputs.bar_text_tone_script = script.clone();
        let caps = Capabilities::detect(&inputs);
        assert_eq!(caps.bar_text_tone_script(), Some(script.as_path()));
        assert!(caps.dotfiles_repo_root().is_none(), "no repo was needed");
    }

    #[cfg(unix)]
    #[test]
    fn an_unusable_bar_text_tone_script_is_absent() {
        // Missing, not executable, or a directory: absent; the missing case is logged.
        use std::os::unix::fs::PermissionsExt;

        // Missing: absent, with the skipped follow-up logged at `info`.
        let root = tempfile::tempdir().expect("temp dir");
        let script = root.path().join("bar-text-tone");
        let mut inputs = base_inputs();
        inputs.bar_text_tone_script = script.clone();
        let mut caps = None;
        let events = crate::testing::capture_logs(|| caps = Some(Capabilities::detect(&inputs)));
        assert!(
            caps.expect("detection ran")
                .bar_text_tone_script()
                .is_none()
        );
        assert!(
            events
                .iter()
                .any(|event| event.level == tracing::Level::INFO
                    && event.message.contains("bar-text-tone script is missing")),
            "a missing script must be logged at info, got {events:?}"
        );

        // Present but not executable: still absent, since it could not be run.
        fs::write(&script, b"#!/bin/sh\n").expect("write the script");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o644))
            .expect("clear the executable bits");
        assert!(
            Capabilities::detect(&inputs)
                .bar_text_tone_script()
                .is_none()
        );

        // A directory of that name is not a script either.
        let dir = root.path().join("dir-named-bar-text-tone");
        fs::create_dir(&dir).expect("create a directory");
        inputs.bar_text_tone_script = dir;
        assert!(
            Capabilities::detect(&inputs)
                .bar_text_tone_script()
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_bar_text_tone_script_path_is_absent() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let root = tempfile::tempdir().expect("temp dir");
        let dir = root.path().join(OsStr::from_bytes(b"conf\xffig"));
        fs::create_dir_all(&dir).expect("create a non-UTF-8 dir");
        let script = write_executable(&dir, "bar-text-tone");
        assert!(is_executable_file(&script));

        let mut detected = None;
        let events =
            crate::testing::capture_logs(|| detected = detect_bar_text_tone_script(&script));
        assert!(detected.is_none(), "a non-UTF-8 path must not be reported");
        assert!(
            events
                .iter()
                .any(|event| event.level == tracing::Level::INFO
                    && event.message.contains("not valid UTF-8")),
            "the reason must be logged at info, got {events:?}"
        );

        // A script that does not exist there is reported as missing, not as a path
        // problem.
        let missing = dir.join("absent-bar-text-tone");
        let events =
            crate::testing::capture_logs(|| detected = detect_bar_text_tone_script(&missing));
        assert!(detected.is_none());
        assert!(
            events
                .iter()
                .any(|event| event.level == tracing::Level::INFO
                    && event.message.contains("missing or not executable")),
            "a missing script is logged as missing, got {events:?}"
        );
        assert!(
            events
                .iter()
                .all(|event| !event.message.contains("not valid UTF-8")),
            "{events:?}"
        );
    }

    #[test]
    fn no_repo_behind_the_config_hides_the_laptop_toggle() {
        // Without a repo root there is nowhere to find the script (R8.5); the shared
        // root resolution logs once at `info`, naming both repo-backed controls.
        let mut caps = None;
        let events = crate::testing::capture_logs(|| {
            caps = Some(Capabilities::detect(&base_inputs()));
        });
        assert!(
            caps.expect("detection ran")
                .laptop_toggle_script()
                .is_none()
        );
        assert!(
            events
                .iter()
                .any(|event| event.level == tracing::Level::INFO
                    && event.message.contains("palette switcher")
                    && event.message.contains("laptop-display toggle")),
            "the missing repo root must be logged at info naming both controls, got {events:?}"
        );
    }

    #[test]
    fn config_readability_is_reported() {
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
        let path = dir.path().join("hypridle.conf");
        fs::write(&path, b"general {\n}\n").expect("write a config");
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
    fn detection_is_deterministic_across_reruns() {
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
        // the machine running the test may or may not have any given tool. The mock
        // runner keeps the test from spawning a real `hyprctl`, `busctl` or
        // `systemctl`; whatever it was asked to run can only be the config-provider
        // query and the notifd probes.
        let runner = MockCommandRunner::new();
        let inputs = DetectionInputs::from_system(&runner, Vec::new());
        let _ = Capabilities::detect(&inputs);
        let allowed = [
            Command::new("hyprctl").arg("status"),
            crate::core::notifd::name_has_owner_command(),
            crate::core::notifd::server_information_command(),
            crate::core::notifd::unit_is_active_command(),
        ];
        for command in runner.recorded() {
            assert!(allowed.contains(&command), "unexpected probe {command}");
        }
    }

    // --- Hyprland config provider (task 11.2) ---

    /// The `hyprctl status` reply of a Lua session (verbatim from Hyprland 0.56.2).
    const STATUS_LUA: &str = "configProvider: lua\nbackend: drm\n\n";
    /// The reply of a session on the legacy provider.
    const STATUS_HYPRLANG: &str = "configProvider: hyprlang\nbackend: drm\n";
    /// A reply without the provider line (e.g. a build that does not report it).
    const STATUS_WITHOUT_PROVIDER: &str = "backend: drm\n";

    /// Inputs with a live stand-in IPC socket (so `hyprctl status` is consulted),
    /// the given captured reply, and the given Lua-config path.
    fn session_inputs(socket: &Path, status: Option<&str>, lua_config: PathBuf) -> DetectionInputs {
        let mut inputs = base_inputs();
        inputs.hyprland_socket = Some(socket.to_path_buf());
        inputs.hyprctl_status = status.map(str::to_string);
        inputs.hyprland_lua_config = lua_config;
        inputs
    }

    #[test]
    fn a_live_session_reports_its_config_provider() {
        let run = tempfile::tempdir().expect("temp dir");
        let socket = run.path().join(".socket.sock");
        fs::write(&socket, b"").expect("stand-in socket");
        let missing_lua = run.path().join("hyprland.lua");

        // The session's report wins even with no `hyprland.lua` on disk.
        let caps = Capabilities::detect(&session_inputs(
            &socket,
            Some(STATUS_LUA),
            missing_lua.clone(),
        ));
        assert_eq!(caps.hypr_config_provider(), HyprConfigProvider::Lua);

        // ...and a hyprlang report wins even when a `hyprland.lua` exists (the session
        // did not load it, so writing the Lua files would have no effect).
        let lua = run.path().join("present.lua");
        fs::write(&lua, b"hl.config({})\n").expect("write a Lua config");
        let caps = Capabilities::detect(&session_inputs(&socket, Some(STATUS_HYPRLANG), lua));
        assert_eq!(caps.hypr_config_provider(), HyprConfigProvider::Hyprlang);
    }

    #[test]
    fn a_reply_without_a_provider_falls_back_to_the_lua_file() {
        let run = tempfile::tempdir().expect("temp dir");
        let socket = run.path().join(".socket.sock");
        fs::write(&socket, b"").expect("stand-in socket");
        let lua = run.path().join("hyprland.lua");

        let without_file = session_inputs(&socket, Some(STATUS_WITHOUT_PROVIDER), lua.clone());
        assert_eq!(
            Capabilities::detect(&without_file).hypr_config_provider(),
            HyprConfigProvider::Hyprlang,
            "no report and no hyprland.lua -> hyprlang"
        );

        fs::write(&lua, b"hl.config({})\n").expect("write a Lua config");
        let with_file = session_inputs(&socket, Some(STATUS_WITHOUT_PROVIDER), lua.clone());
        assert_eq!(
            Capabilities::detect(&with_file).hypr_config_provider(),
            HyprConfigProvider::Lua,
            "no report but hyprland.lua exists -> lua"
        );

        // A failed/ungathered query (`None`) behaves the same way.
        let ungathered = session_inputs(&socket, None, lua);
        assert_eq!(
            Capabilities::detect(&ungathered).hypr_config_provider(),
            HyprConfigProvider::Lua
        );
    }

    #[test]
    fn a_status_reply_is_ignored_without_a_live_socket() {
        // A stale captured reply must not outvote the filesystem when no compositor
        // is live: only the file check applies.
        let mut inputs = base_inputs();
        inputs.hyprctl_status = Some(STATUS_LUA.to_string());
        assert_eq!(
            Capabilities::detect(&inputs).hypr_config_provider(),
            HyprConfigProvider::Hyprlang,
            "no socket and no hyprland.lua -> hyprlang, whatever the reply said"
        );
    }

    #[test]
    fn the_fixture_tree_hyprland_lua_makes_an_offline_host_lua() {
        // With no running session, the deployed fixture `hyprland.lua` (a symlink into
        // the fixture repo, task 11.1) decides the provider; without it the host reads
        // as hyprlang.
        let fx = crate::testing::FixtureDotfiles::install();
        let mut inputs = base_inputs();
        inputs.hyprland_lua_config = fx.config_path("hypr/hyprland.lua");
        assert_eq!(
            Capabilities::detect(&inputs).hypr_config_provider(),
            HyprConfigProvider::Lua
        );

        fs::remove_file(fx.config_path("hypr/hyprland.lua")).expect("remove the deployed link");
        assert_eq!(
            Capabilities::detect(&inputs).hypr_config_provider(),
            HyprConfigProvider::Hyprlang
        );
    }

    #[test]
    fn the_provider_line_is_parsed_leniently() {
        // Case, surrounding whitespace and other status lines do not matter.
        assert_eq!(
            parse_config_provider("  configProvider:   Lua  \n"),
            Some(HyprConfigProvider::Lua)
        );
        assert_eq!(
            parse_config_provider("backend: drm\nconfigProvider: hyprlang"),
            Some(HyprConfigProvider::Hyprlang)
        );
    }

    #[test]
    fn an_unknown_or_missing_provider_parses_as_none() {
        assert_eq!(parse_config_provider("configProvider: toml\n"), None);
        assert_eq!(parse_config_provider(STATUS_WITHOUT_PROVIDER), None);
        assert_eq!(parse_config_provider(""), None);
    }

    #[test]
    fn hyprctl_status_is_queried_through_the_runner() {
        let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake_with_streams(
            0, STATUS_LUA, "",
        ))]);
        assert_eq!(query_hyprctl_status(&runner).as_deref(), Some(STATUS_LUA));
        assert_eq!(
            runner.recorded(),
            vec![Command::new("hyprctl").arg("status")]
        );

        // A failing or unrunnable query degrades to `None`, never an error.
        let failing = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake(1))]);
        assert_eq!(query_hyprctl_status(&failing), None);
        let unrunnable =
            MockCommandRunner::with_outcomes([Err(crate::system::command::CommandError::Spawn(
                std::io::Error::from(std::io::ErrorKind::NotFound),
            ))]);
        assert_eq!(query_hyprctl_status(&unrunnable), None);
    }

    #[test]
    fn the_absent_placeholder_keeps_the_lua_only_controls_hidden() {
        assert_eq!(
            Capabilities::absent().hypr_config_provider(),
            HyprConfigProvider::Hyprlang
        );
    }

    // --- notifd (task 11.7) ---

    /// Detection inputs carrying the given notifd probe replies (stdout as the mock
    /// runner would have captured it) and the given unit-file path.
    fn notifd_inputs(
        owner: Option<&str>,
        info: Option<&str>,
        active: Option<&str>,
        unit_file: PathBuf,
    ) -> DetectionInputs {
        let mut inputs = base_inputs();
        inputs.notifd_replies = NotifdReplies {
            name_has_owner: owner.map(str::to_string),
            server_information: info.map(str::to_string),
            unit_active: active.map(str::to_string),
        };
        inputs.notifd_unit_file = unit_file;
        inputs
    }

    /// Gathers the notifd replies from a mock runner scripted with `outcomes`, the
    /// way `from_system` does, and returns them with the commands it issued.
    fn gather_from(
        outcomes: Vec<Result<CommandOutput, crate::system::command::CommandError>>,
    ) -> (NotifdReplies, Vec<Command>) {
        let runner = MockCommandRunner::with_outcomes(outcomes);
        let replies = NotifdReplies::gather(&runner);
        (replies, runner.recorded())
    }

    #[test]
    fn a_running_1_1_notifd_is_detected_with_its_version() {
        let (replies, issued) = gather_from(vec![
            Ok(CommandOutput::fake_with_streams(0, "b true\n", "")),
            Ok(CommandOutput::fake_with_streams(
                0,
                "ssss \"notifd\" \"dotfiles\" \"1.1\" \"1.3\"\n",
                "",
            )),
        ]);
        assert_eq!(issued.len(), 2, "NameHasOwner then GetServerInformation");
        let mut inputs = base_inputs();
        inputs.notifd_replies = replies;
        let caps = Capabilities::detect(&inputs);
        assert_eq!(
            caps.notifd(),
            NotifdStatus::Running {
                version: crate::core::notifd::NotifdVersion::parse("1.1")
            }
        );
        assert!(caps.notifd().is_present());
        assert!(caps.notifd().supports_state_and_reload());
    }

    #[test]
    fn a_running_1_0_notifd_is_present_without_state_and_reload() {
        let caps = Capabilities::detect(&notifd_inputs(
            Some("b true\n"),
            Some("ssss \"notifd\" \"dotfiles\" \"1.0\" \"1.3\"\n"),
            None,
            PathBuf::from("/nonexistent/notifd.service"),
        ));
        assert_eq!(
            caps.notifd().version(),
            crate::core::notifd::NotifdVersion::parse("1.0")
        );
        assert!(caps.notifd().is_present());
        assert!(!caps.notifd().supports_state_and_reload());
    }

    #[test]
    fn an_installed_but_stopped_notifd_is_installed() {
        // Unit file present, `is-active` reports inactive (exit 3), name not owned.
        let dir = tempfile::tempdir().expect("temp dir");
        let unit = dir.path().join("notifd.service");
        fs::write(&unit, b"[Service]\n").expect("write a unit file");
        let (replies, issued) = gather_from(vec![
            Ok(CommandOutput::fake_with_streams(0, "b false\n", "")),
            Ok(CommandOutput::fake_with_streams(3, "inactive\n", "")),
        ]);
        assert_eq!(
            issued,
            vec![
                crate::core::notifd::name_has_owner_command(),
                crate::core::notifd::unit_is_active_command()
            ]
        );
        let mut inputs = base_inputs();
        inputs.notifd_replies = replies;
        inputs.notifd_unit_file = unit.clone();
        let caps = Capabilities::detect(&inputs);
        assert_eq!(caps.notifd(), NotifdStatus::Installed);
        assert!(caps.notifd().is_present());
        assert_eq!(caps.notifd().version(), None);

        // The same replies without the unit file: nothing says notifd is installed.
        fs::remove_file(&unit).expect("remove the unit file");
        assert_eq!(Capabilities::detect(&inputs).notifd(), NotifdStatus::Absent);
    }

    #[test]
    fn the_fixture_tree_unit_file_marks_notifd_installed() {
        // The deployed fixture links config/systemd/user/notifd.service like the
        // dotfiles' installer does.
        let fx = crate::testing::FixtureDotfiles::install();
        let caps = Capabilities::detect(&notifd_inputs(
            Some("b false\n"),
            None,
            Some("inactive\n"),
            fx.config_path("systemd/user/notifd.service"),
        ));
        assert_eq!(caps.notifd(), NotifdStatus::Installed);
    }

    #[test]
    fn an_absent_or_unqueryable_notifd_is_absent() {
        // Nothing gathered (no busctl on PATH) — the base inputs.
        assert_eq!(
            Capabilities::detect(&base_inputs()).notifd(),
            NotifdStatus::Absent
        );
        // Bus answered "no owner", systemd does not know the unit, no unit file.
        let caps = Capabilities::detect(&notifd_inputs(
            Some("b false\n"),
            None,
            Some("inactive\n"),
            PathBuf::from("/nonexistent/notifd.service"),
        ));
        assert_eq!(caps.notifd(), NotifdStatus::Absent);
        assert!(!caps.notifd().is_present());
    }

    #[test]
    fn a_malformed_notifd_reply_degrades_instead_of_guessing() {
        // A garbled NameHasOwner reply: the bus cannot be trusted, so Absent — even
        // with a unit file and an active unit.
        let dir = tempfile::tempdir().expect("temp dir");
        let unit = dir.path().join("notifd.service");
        fs::write(&unit, b"").expect("write a unit file");
        let garbled_owner = notifd_inputs(Some("b maybe\n"), None, Some("active\n"), unit);
        assert_eq!(
            Capabilities::detect(&garbled_owner).notifd(),
            NotifdStatus::Absent
        );

        // A garbled GetServerInformation reply: running, version unknown (pre-1.1).
        let garbled_info = notifd_inputs(
            Some("b true\n"),
            Some("ssss \"notifd\" \"dotfiles\""),
            None,
            PathBuf::from("/nonexistent/notifd.service"),
        );
        let caps = Capabilities::detect(&garbled_info);
        assert_eq!(caps.notifd(), NotifdStatus::Running { version: None });
        assert!(!caps.notifd().supports_state_and_reload());
    }

    #[test]
    fn detection_never_scans_for_a_notification_daemon_binary() {
        // notifd is found by its bus name only: a binary of the daemon it replaced may
        // still be installed, so no notification-daemon binary is in the scan list.
        for binary in Binary::ALL {
            assert!(
                !binary.command_name().starts_with("sway"),
                "{binary:?} must not be scanned"
            );
        }
        assert_eq!(Capabilities::absent().notifd(), NotifdStatus::Absent);
    }

    #[test]
    fn detection_logs_every_degradation_at_its_level() {
        // Accept criterion (logging): a hidden item (absent binary, absent palette
        // source) is logged at `info`; an unreadable config is logged at `warn`.
        // Captured via a scoped tracing subscriber so no global state is touched.
        use std::sync::{Arc, Mutex};
        use tracing::Level;
        use tracing::subscriber::with_default;
        use tracing_subscriber::Layer;
        use tracing_subscriber::layer::SubscriberExt;

        /// One captured event: its level, its message, and its other fields rendered
        /// as `name=value` pairs (kept apart so a field value can never satisfy a
        /// message assertion).
        type Captured = (Level, String, String);

        #[derive(Clone, Default)]
        struct LogCapture {
            events: Arc<Mutex<Vec<Captured>>>,
        }

        struct MessageVisitor<'a> {
            message: &'a mut String,
            fields: &'a mut String,
        }
        impl tracing::field::Visit for MessageVisitor<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                use std::fmt::Write as _;
                if field.name() == "message" {
                    let _ = write!(self.message, "{value:?}");
                } else {
                    let _ = write!(self.fields, "{}={value:?} ", field.name());
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
                let mut fields = String::new();
                event.record(&mut MessageVisitor {
                    message: &mut message,
                    fields: &mut fields,
                });
                self.events
                    .lock()
                    .expect("log capture mutex should not be poisoned")
                    .push((*event.metadata().level(), message, fields));
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
                .any(|(level, message, _)| *level == Level::INFO && message.contains("palette")),
            "an absent palette source must be logged at info"
        );
        assert!(
            events
                .iter()
                .any(|(level, message, _)| *level == Level::INFO && message.contains("PATH")),
            "an absent binary must be logged at info"
        );
        assert!(
            events
                .iter()
                .any(|(level, message, _)| *level == Level::WARN && message.contains("unreadable")),
            "an unreadable config must be logged at warn"
        );
        // Task 11.2: no `hyprland.lua` and no session -> hyprlang, which hides the
        // Lua-only controls (an `info` hidden item), and the summary names it.
        assert!(
            events
                .iter()
                .any(|(level, message, _)| *level == Level::INFO
                    && message.contains("Input page")
                    && message.contains("cursor write")),
            "a non-Lua provider must log the hidden Input page and cursor write at info"
        );
        assert!(
            events
                .iter()
                .any(|(level, message, fields)| *level == Level::INFO
                    && message.contains("detection complete")
                    && fields.contains("hypr_config_provider=hyprlang")),
            "the detection summary must name the config provider"
        );
        // Task 11.7: nothing gathered -> notifd absent, logged as a hidden page, and
        // the summary carries the status.
        assert!(
            events
                .iter()
                .any(|(level, message, _)| *level == Level::INFO
                    && message.contains("notifd not found")
                    && message.contains("Notifications page is hidden")),
            "an absent notifd must be logged at info as a hidden page"
        );
        assert!(
            events
                .iter()
                .any(|(level, message, fields)| *level == Level::INFO
                    && message.contains("detection complete")
                    && fields.contains("notifd=absent")),
            "the detection summary must name the notifd status"
        );
    }

    #[test]
    fn process_matching_is_exact() {
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
