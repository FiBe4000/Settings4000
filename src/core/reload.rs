//! The reload command table: which live-reloads a changed backing file requires,
//! and how each one runs (task 4.4; architecture §6; R5.3, R5.5, R7.3, R6.2).
//!
//! # What this module is
//!
//! After the Apply pipeline (task 4.5) writes the changed config files, it must
//! tell the affected components to re-read them. This module owns two things:
//!
//! 1. The **table** mapping each [`BackingFile`] the app writes to the ordered set
//!    of [`ReloadAction`]s that file's change requires, and [`plan_reloads`], which
//!    turns the set of changed files into a single deduplicated, ordered,
//!    capability-gated action list.
//! 2. The **executor** — [`ReloadAction::execute`] — which runs one action through
//!    the side-effect seams: subprocess reloads via
//!    [`CommandRunner`](crate::system::command::CommandRunner) (no shell, arg
//!    vectors only) and the two signal-based reloads via
//!    [`ProcessSignaller`](crate::system::signal::ProcessSignaller).
//!
//! It lives in `core/` because the mapping is pure domain logic: it depends only on
//! the [`Capabilities`] value and the change set, so it is headlessly testable
//! (R6.2) and the layering guard in `tests/module_boundaries.rs` forbids any
//! `gtk`/`relm4` import. Side effects are reached only through the two `system/`
//! traits.
//!
//! # Kept in sync with `scripts/apply-theme`
//!
//! For a palette (color-scheme) change the reload set mirrors the dotfiles'
//! canonical `scripts/apply-theme` wrapper (analysis §6.1): `hyprctl reload`, then
//! `eww reload`, then `swaync-client -rs`, then a SIGUSR1 to kitty. The app runs
//! `generate-colors` and issues these reloads itself rather than calling the
//! script (architecture §6), so this table and that script must be kept in step:
//! if the script's reload chain changes, [`BackingFile::Palette`]'s mapping changes
//! with it.
//!
//! # Capability gating (R4.2/R5.5)
//!
//! An action is emitted only when its target component is present *and*, for a
//! daemon, detection found it running — "reload only the components that changed
//! and are running". [`ReloadAction::is_available`] encodes this against
//! [`Capabilities`]; [`plan_reloads`] applies it. A reload for an absent or stopped
//! component is silently dropped, never attempted.
//!
//! # Reload failures are non-fatal (R5.5)
//!
//! [`ReloadAction::execute`] logs each reload and its result (R7.3) and returns a
//! [`ReloadError`] on failure, but a failed reload never rolls back a successful
//! file write. The decision to keep going after a failed reload (surfacing it as a
//! non-fatal toast) belongs to the Apply pipeline (task 4.5); this module only
//! defines the actions and how a single one runs.

use std::fmt;
use std::io;

use nix::sys::signal::Signal;

use crate::core::detect::{Binary, Capabilities, Daemon};
use crate::system::command::{Command, CommandError, CommandRunner};
use crate::system::signal::ProcessSignaller;

/// The GSettings schema every theme/cursor key the app writes lives under.
const GSETTINGS_SCHEMA: &str = "org.gnome.desktop.interface";
/// GSettings key for the GTK theme name (R3.3).
const GSETTINGS_GTK_THEME: &str = "gtk-theme";
/// GSettings key for the icon theme name (R3.4).
const GSETTINGS_ICON_THEME: &str = "icon-theme";
/// GSettings key for the cursor theme name (R3.4).
const GSETTINGS_CURSOR_THEME: &str = "cursor-theme";
/// GSettings key for the cursor size (R3.4).
const GSETTINGS_CURSOR_SIZE: &str = "cursor-size";

/// The executable name (argv[0] basename) of the kitty terminal, signalled for a
/// color reload.
const KITTY_PROCESS: &str = "kitty";
/// The executable name of the hypridle daemon, signalled and respawned by the
/// restart fallback.
const HYPRIDLE_PROCESS: &str = "hypridle";
/// The systemd user unit name tried first when restarting hypridle.
const HYPRIDLE_UNIT: &str = "hypridle";

/// A config file the app writes, identified by the reload its change requires
/// (architecture §6 / analysis §6.6).
///
/// The Apply pipeline (task 4.5) maps each changed setting to the file that backs
/// it and feeds the resulting set to [`plan_reloads`]. A file is named here by the
/// reload concern it drives rather than by an exact path, so files that require the
/// identical reload share one variant: [`GtkSettings`](Self::GtkSettings) covers
/// both `gtk-3.0/settings.ini` and `gtk-4.0/settings.ini`, and
/// [`Palette`](Self::Palette) stands for the palette switch (`colors/<scheme>` +
/// the generated color partials `generate-colors` rewrites) rather than a single
/// file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum BackingFile {
    /// `config/hypr/monitors.conf` — a display change; Hyprland re-reads it on
    /// `hyprctl reload` (task 6.1).
    MonitorsConf,
    /// `config/hypr/input.conf` — the `source=`d `input {}` block; `hyprctl reload`
    /// (task 6.6).
    InputConf,
    /// `config/hypr/hyprland.conf` — the cursor `env =` lines the app still owns
    /// here; `hyprctl reload` re-reads the file (task 6.4). The live cursor change
    /// itself is applied by the cursor reload emitted for the GTK/env files.
    HyprlandConf,
    /// `config/hypr/hypridle.conf` — idle timeouts / lock command; hypridle is
    /// restarted to pick them up (task 6.8).
    HypridleConf,
    /// `config/hypr/hyprlock.conf` — the lock screen. **No reload**: hyprlock reads
    /// its config only at launch, so a change takes effect at the next lock
    /// (intentional, architecture §6, task 6.5).
    HyprlockConf,
    /// `config/hypr/hyprpaper.conf` — the wallpaper; applied live with
    /// `hyprctl hyprpaper preload`/`wallpaper` (task 6.5).
    HyprpaperConf,
    /// `config/swaync/config.json` — notification settings; `swaync-client -rs`
    /// (task 6.7).
    SwayncConfig,
    /// The GTK 3/4 `settings.ini` files — GTK/icon/cursor theme; applied live with
    /// `gsettings set` (+ `hyprctl setcursor` for the cursor), task 6.4.
    GtkSettings,
    /// `config/uwsm/env` — the cursor `XCURSOR_*` env copy; the cursor is applied
    /// live with `gsettings set cursor-*` + `hyprctl setcursor` (task 6.4).
    UwsmEnv,
    /// A palette (color-scheme) switch: `colors/<scheme>` is edited and
    /// `generate-colors` rewrites the color partials. Triggers the broad
    /// apply-theme reload chain (analysis §6.1).
    Palette,
}

impl BackingFile {
    /// The reload actions this file's change requires, in canonical order, before
    /// capability gating.
    ///
    /// `params` supplies the runtime values the parameterized actions need (the
    /// wallpaper path, the cursor theme/size, the GTK/icon theme names). The Apply
    /// pipeline (task 4.5) fills a field only for a value that actually changed, so
    /// e.g. a change that touched only the icon theme yields only the icon-theme
    /// `gsettings set`. A parameterized file whose value is missing from `params`
    /// yields no action for it (with a warning) rather than an ill-formed command.
    pub(crate) fn reload_actions(self, params: &ReloadParams) -> Vec<ReloadAction> {
        match self {
            // A plain Hyprland config re-read. hyprland.conf is here (not with the
            // cursor reload) because architecture §6 maps the file to `hyprctl
            // reload`; the live cursor apply rides on the GTK/env files' actions.
            BackingFile::MonitorsConf | BackingFile::InputConf | BackingFile::HyprlandConf => {
                vec![ReloadAction::HyprctlReload]
            }
            BackingFile::HypridleConf => vec![ReloadAction::HypridleRestart],
            // hyprlock intentionally has no reload (see the variant docs).
            BackingFile::HyprlockConf => Vec::new(),
            BackingFile::SwayncConfig => vec![ReloadAction::SwayncReload],
            BackingFile::HyprpaperConf => match &params.wallpaper {
                Some(path) => vec![ReloadAction::HyprpaperWallpaper { path: path.clone() }],
                None => {
                    tracing::warn!(
                        "hyprpaper.conf changed but no wallpaper path was provided; \
                         skipping the hyprpaper reload"
                    );
                    Vec::new()
                }
            },
            // The GTK settings.ini files carry the GTK theme, icon theme, and cursor;
            // uwsm/env carries only the cursor copy.
            BackingFile::GtkSettings => theme_and_cursor_actions(params, true),
            BackingFile::UwsmEnv => theme_and_cursor_actions(params, false),
            // A palette switch reloads every component whose colors were regenerated,
            // in the apply-theme order (analysis §6.1).
            BackingFile::Palette => vec![
                ReloadAction::HyprctlReload,
                ReloadAction::EwwReload,
                ReloadAction::SwayncReload,
                ReloadAction::KittyColors,
            ],
        }
    }
}

/// Builds the theme/cursor reload actions from `params`.
///
/// When `include_gtk_icon` is set (the `settings.ini` files, which hold those keys)
/// the GTK and icon theme `gsettings set` calls are emitted for whichever changed.
/// The cursor part — `gsettings set cursor-theme`/`cursor-size` plus
/// `hyprctl setcursor` — is emitted whenever a cursor value is present, for both the
/// `settings.ini` files and `uwsm/env`, so a cursor change reloads identically
/// regardless of which of its several copies triggered it (R3.4, analysis §6.2).
fn theme_and_cursor_actions(params: &ReloadParams, include_gtk_icon: bool) -> Vec<ReloadAction> {
    let mut actions = Vec::new();
    if include_gtk_icon {
        if let Some(theme) = &params.gtk_theme {
            actions.push(ReloadAction::GsettingsSet {
                key: GSETTINGS_GTK_THEME,
                value: theme.clone(),
            });
        }
        if let Some(theme) = &params.icon_theme {
            actions.push(ReloadAction::GsettingsSet {
                key: GSETTINGS_ICON_THEME,
                value: theme.clone(),
            });
        }
    }
    if let Some(cursor) = &params.cursor {
        actions.push(ReloadAction::GsettingsSet {
            key: GSETTINGS_CURSOR_THEME,
            value: cursor.theme.clone(),
        });
        actions.push(ReloadAction::GsettingsSet {
            key: GSETTINGS_CURSOR_SIZE,
            value: cursor.size.to_string(),
        });
        actions.push(ReloadAction::HyprctlSetCursor {
            theme: cursor.theme.clone(),
            size: cursor.size,
        });
    }
    actions
}

/// Runtime values the parameterized reload actions need, supplied by the Apply
/// pipeline (task 4.5) from the staged values.
///
/// Each field is `Some` only when the corresponding setting actually changed in the
/// current Apply, so the table emits an action only for what changed (see
/// [`BackingFile::reload_actions`]). The default (all `None`) is the correct state
/// for a change that touches no parameterized file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReloadParams {
    /// The new wallpaper path, for the hyprpaper reload (task 6.5).
    pub(crate) wallpaper: Option<String>,
    /// The new cursor theme and size, for `hyprctl setcursor` and the `gsettings`
    /// cursor keys (task 6.4).
    pub(crate) cursor: Option<CursorValue>,
    /// The new GTK theme name, for `gsettings set … gtk-theme` (task 6.4).
    pub(crate) gtk_theme: Option<String>,
    /// The new icon theme name, for `gsettings set … icon-theme` (task 6.4).
    pub(crate) icon_theme: Option<String>,
}

/// A cursor theme selection: the theme name and its pixel size.
///
/// Both are written together to every cursor copy the app owns and reloaded
/// together via `hyprctl setcursor <theme> <size>` and the `gsettings` cursor keys
/// (R3.4), so they travel as one value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CursorValue {
    /// The cursor theme name (e.g. `Nordic-cursors`).
    pub(crate) theme: String,
    /// The cursor size in pixels (e.g. `16`).
    pub(crate) size: u32,
}

/// A single distinct live-reload the app can issue (architecture §6 /
/// `scripts/apply-theme`, analysis §6.1).
///
/// Each variant carries exactly the data its command(s) need; the concrete,
/// shell-free argument vectors are built in [`Self::execute`]. Two variants are not
/// subprocesses: [`KittyColors`](Self::KittyColors) delivers a signal, and
/// [`HypridleRestart`](Self::HypridleRestart) is a systemctl call with a
/// signal-plus-respawn fallback — both routed through the seams in
/// [`Self::execute`] rather than a plain command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReloadAction {
    /// `hyprctl reload` — Hyprland re-reads `hyprland.conf` and every `source=`d
    /// file.
    HyprctlReload,
    /// `eww reload` — recompile and reload the eww bars.
    EwwReload,
    /// `swaync-client -rs` — reload swaync's config and CSS.
    SwayncReload,
    /// SIGUSR1 to every running kitty, which re-reads its config (v1 has no kitty
    /// remote-control reload; analysis §6.1).
    KittyColors,
    /// Set the wallpaper via hyprpaper: `hyprctl hyprpaper preload <path>` then
    /// `hyprctl hyprpaper wallpaper ,<path>` (empty monitor field = all outputs).
    HyprpaperWallpaper {
        /// The image path to preload and display.
        path: String,
    },
    /// Restart hypridle so it re-reads its config: restart its systemd user unit
    /// when one is active, else SIGTERM + `setsid --fork` respawn (architecture §6;
    /// see [`restart_hypridle`]).
    HypridleRestart,
    /// `gsettings set org.gnome.desktop.interface <key> <value>` for one interface
    /// key (GTK theme, icon theme, or a cursor key), R3.3/R3.4.
    GsettingsSet {
        /// The interface key (one of the `GSETTINGS_*` constants).
        key: &'static str,
        /// The value to set.
        value: String,
    },
    /// `hyprctl setcursor <theme> <size>` — apply the cursor live on Hyprland
    /// (R3.4).
    HyprctlSetCursor {
        /// The cursor theme name.
        theme: String,
        /// The cursor size in pixels.
        size: u32,
    },
}

impl ReloadAction {
    /// Whether this action's target component is present and (for a daemon) running,
    /// so the reload should be issued (R4.2/R5.5).
    ///
    /// Hyprland-family actions (`hyprctl reload`/`setcursor`, and `hyprctl
    /// hyprpaper`) require [`Capabilities::hyprland_reloadable`] — the `hyprctl`
    /// client plus a live IPC socket; the hyprpaper action additionally requires the
    /// hyprpaper daemon to be running. The daemon reloads (`eww`, `swaync`, kitty,
    /// hypridle) require that daemon to be live. `gsettings set` requires the
    /// `gsettings` binary. A component that is absent or stopped yields `false`, so
    /// [`plan_reloads`] drops the action.
    pub(crate) fn is_available(&self, capabilities: &Capabilities) -> bool {
        match self {
            ReloadAction::HyprctlReload | ReloadAction::HyprctlSetCursor { .. } => {
                capabilities.hyprland_reloadable()
            }
            ReloadAction::HyprpaperWallpaper { .. } => {
                capabilities.hyprland_reloadable() && capabilities.is_daemon_live(Daemon::Hyprpaper)
            }
            ReloadAction::EwwReload => capabilities.is_daemon_live(Daemon::Eww),
            ReloadAction::SwayncReload => capabilities.is_daemon_live(Daemon::Swaync),
            ReloadAction::KittyColors => capabilities.is_daemon_live(Daemon::Kitty),
            ReloadAction::HypridleRestart => capabilities.is_daemon_live(Daemon::Hypridle),
            ReloadAction::GsettingsSet { .. } => capabilities.has_binary(Binary::Gsettings),
        }
    }

    /// A total ordering key placing actions in a deterministic reload order.
    ///
    /// Used by [`plan_reloads`] to sort the merged action list so a combined change
    /// always reloads in the same sequence, and so a palette change's chain comes
    /// out in the apply-theme order (`hyprctl reload` → `eww reload` →
    /// `swaync-client -rs` → kitty; analysis §6.1). The second element sub-orders the
    /// several `gsettings set` keys deterministically.
    fn order_key(&self) -> (u8, u8) {
        match self {
            ReloadAction::HyprctlReload => (0, 0),
            ReloadAction::GsettingsSet { key, .. } => (1, gsettings_key_rank(key)),
            ReloadAction::HyprctlSetCursor { .. } => (2, 0),
            ReloadAction::HyprpaperWallpaper { .. } => (3, 0),
            ReloadAction::HypridleRestart => (4, 0),
            ReloadAction::EwwReload => (5, 0),
            ReloadAction::SwayncReload => (6, 0),
            ReloadAction::KittyColors => (7, 0),
        }
    }

    /// Runs this reload through the side-effect seams, logging it and its result
    /// (R7.3), and returns a [`ReloadError`] on failure.
    ///
    /// Subprocess reloads go through `runner` (no shell); the two signal-based
    /// reloads go through `signaller`. A failure is logged at `error` and returned
    /// (R5.5) but is not fatal here — the Apply pipeline (task 4.5) decides whether
    /// to continue and how to surface it. The file write it accompanies always
    /// stands.
    pub(crate) fn execute(
        &self,
        runner: &dyn CommandRunner,
        signaller: &dyn ProcessSignaller,
    ) -> Result<(), ReloadError> {
        match self {
            ReloadAction::HyprctlReload => {
                run_and_check(runner, Command::new("hyprctl").arg("reload"))
            }
            ReloadAction::EwwReload => run_and_check(runner, Command::new("eww").arg("reload")),
            ReloadAction::SwayncReload => {
                run_and_check(runner, Command::new("swaync-client").arg("-rs"))
            }
            ReloadAction::HyprpaperWallpaper { path } => {
                // Preload the image, then set it on all outputs (empty monitor
                // field). Preload must succeed first, so short-circuit on its
                // failure rather than trying to display an unloaded image.
                run_and_check(
                    runner,
                    Command::new("hyprctl")
                        .arg("hyprpaper")
                        .arg("preload")
                        .arg(path.as_str()),
                )?;
                run_and_check(
                    runner,
                    Command::new("hyprctl")
                        .arg("hyprpaper")
                        .arg("wallpaper")
                        .arg(format!(",{path}")),
                )
            }
            ReloadAction::GsettingsSet { key, value } => run_and_check(
                runner,
                Command::new("gsettings")
                    .arg("set")
                    .arg(GSETTINGS_SCHEMA)
                    .arg(*key)
                    .arg(value.as_str()),
            ),
            ReloadAction::HyprctlSetCursor { theme, size } => run_and_check(
                runner,
                Command::new("hyprctl")
                    .arg("setcursor")
                    .arg(theme.as_str())
                    .arg(size.to_string()),
            ),
            ReloadAction::KittyColors => reload_kitty(signaller),
            ReloadAction::HypridleRestart => restart_hypridle(runner, signaller),
        }
    }
}

/// The canonical sub-order of the `gsettings set` interface keys, so a multi-key
/// theme change reloads them in a stable order.
fn gsettings_key_rank(key: &str) -> u8 {
    match key {
        GSETTINGS_GTK_THEME => 0,
        GSETTINGS_ICON_THEME => 1,
        GSETTINGS_CURSOR_THEME => 2,
        GSETTINGS_CURSOR_SIZE => 3,
        _ => 4,
    }
}

/// Plans the reloads for a set of changed backing files (architecture §6 step 4).
///
/// Collects each file's [`BackingFile::reload_actions`], drops any whose component
/// is absent or not running ([`ReloadAction::is_available`]), then sorts by
/// [`ReloadAction::order_key`] and removes duplicates so a change touching several
/// files that share a reload (e.g. a cursor written to `settings.ini`, `uwsm/env`,
/// and `hyprland.conf`) issues each reload once, in a deterministic order. This is
/// the list the Apply pipeline (task 4.5) executes after writing the files.
pub(crate) fn plan_reloads(
    changed: &[BackingFile],
    params: &ReloadParams,
    capabilities: &Capabilities,
) -> Vec<ReloadAction> {
    let mut actions: Vec<ReloadAction> = changed
        .iter()
        .flat_map(|file| file.reload_actions(params))
        .filter(|action| action.is_available(capabilities))
        .collect();
    // A stable sort by the canonical order key groups equal actions together; the
    // subsequent `dedup` (which removes only consecutive equal elements) then
    // collapses them to one.
    actions.sort_by_key(ReloadAction::order_key);
    actions.dedup();
    actions
}

/// Runs one subprocess reload command and turns its result into a [`ReloadError`]
/// on failure, logging both the attempt and the outcome (R7.3).
///
/// A completed command that exited non-zero is a [`ReloadError::NonZeroExit`]; a
/// command that could not be run at all (spawn failure / timeout) is a
/// [`ReloadError::Command`]. Both are logged at `error` (R5.5); success is logged at
/// `info`. This layers a reload-level record on top of the `CommandRunner`'s own
/// per-invocation log.
fn run_and_check(runner: &dyn CommandRunner, command: Command) -> Result<(), ReloadError> {
    match runner.run(&command) {
        Ok(output) if output.success() => {
            tracing::info!(command = %command, "reload command succeeded");
            Ok(())
        }
        Ok(output) => {
            tracing::error!(
                command = %command,
                exit_code = ?output.code(),
                "reload command reported failure (R5.5)"
            );
            Err(ReloadError::NonZeroExit {
                program: command.program().to_string(),
                code: output.code(),
            })
        }
        Err(error) => {
            tracing::error!(command = %command, %error, "reload command could not be run (R5.5)");
            Err(ReloadError::Command(error))
        }
    }
}

/// Reloads kitty by delivering SIGUSR1 to every running kitty (architecture §6).
///
/// kitty re-reads its config on SIGUSR1, so this is how a palette change reaches it
/// in v1. An empty PID set is not a failure — it means kitty is no longer running (a
/// race against detection), logged and treated as a no-op. A genuine inability to
/// enumerate processes is a [`ReloadError::Signal`].
fn reload_kitty(signaller: &dyn ProcessSignaller) -> Result<(), ReloadError> {
    match signaller.signal_all(KITTY_PROCESS, Signal::SIGUSR1) {
        Ok(pids) if pids.is_empty() => {
            tracing::warn!("no running kitty found to signal; skipping the kitty color reload");
            Ok(())
        }
        Ok(pids) => {
            tracing::info!(?pids, "sent SIGUSR1 to kitty for a color reload");
            Ok(())
        }
        Err(error) => {
            tracing::error!(%error, "failed to signal kitty for a reload (R5.5)");
            Err(ReloadError::Signal(error))
        }
    }
}

/// Restarts hypridle so it re-reads its config (architecture §6).
///
/// If a systemd user unit is *actively* managing hypridle, restarts it through
/// systemd; otherwise — the target dotfiles case, where hypridle is an `exec-once`
/// autostart rather than a unit — it terminates the running hypridle with SIGTERM
/// and respawns a fresh one that picks up the new config.
///
/// The activeness check is deliberately `systemctl --user is-active`, **not**
/// `try-restart`: `try-restart` exits 0 even when the unit exists but is inactive
/// (systemd issue #34192), so a distro that ships an inactive `hypridle.service`
/// while the real hypridle runs via `exec-once` would take the systemd arm, do
/// nothing, and never reach the fallback — silently dropping the config change.
/// `is-active` reflects whether systemd is genuinely managing the process, so the
/// two paths are chosen correctly.
///
/// The respawn is `setsid --fork <hypridle>` run through the normal
/// [`CommandRunner::run`]: `setsid --fork` forks hypridle into a new session and the
/// `setsid` process itself exits immediately, so `run` reaps `setsid` (no zombie is
/// leaked) while the daemon is reparented to init and survives the app's exit. This
/// reuses the existing reaping/timeout machinery rather than a hand-rolled detached
/// spawn. The old and new hypridle may briefly overlap while the terminated one
/// shuts down; that race is benign and this is a best-effort fallback.
fn restart_hypridle(
    runner: &dyn CommandRunner,
    signaller: &dyn ProcessSignaller,
) -> Result<(), ReloadError> {
    let is_active = Command::new("systemctl")
        .arg("--user")
        .arg("is-active")
        .arg("--quiet")
        .arg(HYPRIDLE_UNIT);
    // A `systemctl` that could not run at all (absent, timed out) is treated as "no
    // active unit", falling through to the kill + respawn path.
    if runner.run(&is_active).is_ok_and(|output| output.success()) {
        tracing::info!("restarting hypridle via its active systemd user unit");
        return run_and_check(
            runner,
            Command::new("systemctl")
                .arg("--user")
                .arg("restart")
                .arg(HYPRIDLE_UNIT),
        );
    }

    tracing::debug!(
        "no active hypridle systemd unit; falling back to kill + `setsid --fork` respawn"
    );
    let killed = signaller
        .signal_all(HYPRIDLE_PROCESS, Signal::SIGTERM)
        .map_err(ReloadError::Signal)?;
    tracing::info!(?killed, "sent SIGTERM to hypridle before respawn");
    run_and_check(
        runner,
        Command::new("setsid").arg("--fork").arg(HYPRIDLE_PROCESS),
    )?;
    tracing::info!("respawned hypridle detached via `setsid --fork`");
    Ok(())
}

/// Why a [`ReloadAction`] failed to run (R5.5).
///
/// Non-fatal to the Apply pipeline (task 4.5): the accompanying file write stands
/// and the pipeline surfaces this as a toast. Does not derive `Clone`/`PartialEq`
/// because [`ReloadError::Command`] and [`ReloadError::Signal`] wrap error types that
/// implement neither; callers render the message or match the variant.
#[derive(Debug)]
pub(crate) enum ReloadError {
    /// A reload command could not be run at all (spawn failure or timeout).
    Command(CommandError),
    /// A reload command ran but exited non-zero.
    NonZeroExit {
        /// The program that failed.
        program: String,
        /// Its exit code, or `None` if it was terminated by a signal.
        code: Option<i32>,
    },
    /// A signal-based reload could not enumerate its target processes.
    Signal(io::Error),
}

impl fmt::Display for ReloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReloadError::Command(error) => write!(f, "reload command failed to run: {error}"),
            ReloadError::NonZeroExit { program, code } => match code {
                Some(code) => write!(f, "reload command `{program}` exited with status {code}"),
                None => write!(f, "reload command `{program}` was terminated by a signal"),
            },
            ReloadError::Signal(error) => {
                write!(f, "failed to signal a process for reload: {error}")
            }
        }
    }
}

impl std::error::Error for ReloadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ReloadError::Command(error) => Some(error),
            ReloadError::Signal(error) => Some(error),
            ReloadError::NonZeroExit { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::command::{CommandOutput, MockCommandRunner};
    use crate::system::signal::{MockProcessSignaller, SignalCall};

    /// A capabilities value with every reload target present and running — the
    /// baseline for mapping tests, which a gating test then narrows.
    fn all_capabilities() -> Capabilities {
        Capabilities::for_tests(
            &[Binary::Hyprctl, Binary::Gsettings],
            &[
                Daemon::Eww,
                Daemon::Swaync,
                Daemon::Kitty,
                Daemon::Hyprpaper,
                Daemon::Hypridle,
            ],
            true,
        )
    }

    /// Params carrying every parameterized value, so the theme/cursor/wallpaper
    /// files produce their full action sets.
    fn full_params() -> ReloadParams {
        ReloadParams {
            wallpaper: Some("/home/u/Pictures/wall.jpg".to_string()),
            cursor: Some(CursorValue {
                theme: "Nordic-cursors".to_string(),
                size: 16,
            }),
            gtk_theme: Some("Everforest-Green-Dark".to_string()),
            icon_theme: Some("Everforest-Dark".to_string()),
        }
    }

    // --- File -> actions mapping (accept: each file maps to exactly its actions) ---

    #[test]
    fn hypr_config_files_map_to_hyprctl_reload() {
        // monitors.conf / input.conf / hyprland.conf all re-read via `hyprctl
        // reload` (architecture §6). No params are needed.
        let params = ReloadParams::default();
        for file in [
            BackingFile::MonitorsConf,
            BackingFile::InputConf,
            BackingFile::HyprlandConf,
        ] {
            assert_eq!(
                file.reload_actions(&params),
                vec![ReloadAction::HyprctlReload],
                "{file:?} must map to exactly `hyprctl reload`"
            );
        }
    }

    #[test]
    fn hyprlock_only_change_yields_no_reload() {
        // Accept criterion: hyprlock reads its config only at launch, so a
        // hyprlock.conf change issues no reload at all (intentional).
        assert!(
            BackingFile::HyprlockConf
                .reload_actions(&ReloadParams::default())
                .is_empty(),
            "hyprlock.conf must yield no reload"
        );
        // And through the full gated planner, a hyprlock-only change plans nothing.
        assert!(
            plan_reloads(
                &[BackingFile::HyprlockConf],
                &ReloadParams::default(),
                &all_capabilities()
            )
            .is_empty(),
            "a hyprlock-only Apply must run no reload commands"
        );
    }

    #[test]
    fn distinct_hypr_files_map_to_distinct_reloads() {
        let params = full_params();
        assert_eq!(
            BackingFile::HypridleConf.reload_actions(&params),
            vec![ReloadAction::HypridleRestart],
            "hypridle.conf restarts hypridle, not `hyprctl reload`"
        );
        assert_eq!(
            BackingFile::HyprpaperConf.reload_actions(&params),
            vec![ReloadAction::HyprpaperWallpaper {
                path: "/home/u/Pictures/wall.jpg".to_string()
            }],
            "hyprpaper.conf maps to the hyprpaper wallpaper action"
        );
        assert_eq!(
            BackingFile::SwayncConfig.reload_actions(&params),
            vec![ReloadAction::SwayncReload]
        );
    }

    #[test]
    fn palette_change_maps_to_the_apply_theme_chain_in_order() {
        // Accept criterion: a palette change is the broad apply-theme chain, in the
        // canonical order (analysis §6.1) — hyprctl reload, eww reload, swaync -rs,
        // kitty SIGUSR1.
        assert_eq!(
            BackingFile::Palette.reload_actions(&ReloadParams::default()),
            vec![
                ReloadAction::HyprctlReload,
                ReloadAction::EwwReload,
                ReloadAction::SwayncReload,
                ReloadAction::KittyColors,
            ]
        );
    }

    #[test]
    fn gtk_settings_maps_to_theme_and_cursor_gsettings_plus_setcursor() {
        // settings.ini carries GTK theme, icon theme, and cursor: all three
        // `gsettings set` calls (in key order) plus `hyprctl setcursor`.
        assert_eq!(
            BackingFile::GtkSettings.reload_actions(&full_params()),
            vec![
                ReloadAction::GsettingsSet {
                    key: GSETTINGS_GTK_THEME,
                    value: "Everforest-Green-Dark".to_string()
                },
                ReloadAction::GsettingsSet {
                    key: GSETTINGS_ICON_THEME,
                    value: "Everforest-Dark".to_string()
                },
                ReloadAction::GsettingsSet {
                    key: GSETTINGS_CURSOR_THEME,
                    value: "Nordic-cursors".to_string()
                },
                ReloadAction::GsettingsSet {
                    key: GSETTINGS_CURSOR_SIZE,
                    value: "16".to_string()
                },
                ReloadAction::HyprctlSetCursor {
                    theme: "Nordic-cursors".to_string(),
                    size: 16
                },
            ]
        );
    }

    #[test]
    fn uwsm_env_maps_to_the_cursor_reload_only() {
        // uwsm/env holds only the cursor copy, so it never emits the GTK/icon theme
        // gsettings — only the cursor keys and setcursor.
        assert_eq!(
            BackingFile::UwsmEnv.reload_actions(&full_params()),
            vec![
                ReloadAction::GsettingsSet {
                    key: GSETTINGS_CURSOR_THEME,
                    value: "Nordic-cursors".to_string()
                },
                ReloadAction::GsettingsSet {
                    key: GSETTINGS_CURSOR_SIZE,
                    value: "16".to_string()
                },
                ReloadAction::HyprctlSetCursor {
                    theme: "Nordic-cursors".to_string(),
                    size: 16
                },
            ]
        );
    }

    #[test]
    fn a_changed_setting_only_emits_actions_for_provided_params() {
        // The pipeline provides a param only for a value that changed; a settings.ini
        // change that touched only the icon theme yields only the icon-theme
        // gsettings — no gtk-theme, no cursor.
        let params = ReloadParams {
            icon_theme: Some("Papirus".to_string()),
            ..ReloadParams::default()
        };
        assert_eq!(
            BackingFile::GtkSettings.reload_actions(&params),
            vec![ReloadAction::GsettingsSet {
                key: GSETTINGS_ICON_THEME,
                value: "Papirus".to_string()
            }]
        );
    }

    #[test]
    fn hyprpaper_without_a_path_emits_no_action() {
        // A defensive guard: hyprpaper.conf changed but no wallpaper path was
        // supplied (a caller bug) yields no action rather than a malformed command.
        assert!(
            BackingFile::HyprpaperConf
                .reload_actions(&ReloadParams::default())
                .is_empty()
        );
    }

    // --- plan_reloads: dedup + ordering + gating -----------------------------

    #[test]
    fn plan_dedups_and_orders_a_multi_file_cursor_change() {
        // A cursor change writes settings.ini, uwsm/env, and hyprland.conf. The
        // three files' actions overlap on the cursor reload and must collapse to one
        // of each, in canonical order: hyprctl reload (from hyprland.conf), then the
        // cursor gsettings keys, then setcursor.
        let params = ReloadParams {
            cursor: Some(CursorValue {
                theme: "Nordic-cursors".to_string(),
                size: 16,
            }),
            ..ReloadParams::default()
        };
        let planned = plan_reloads(
            &[
                BackingFile::GtkSettings,
                BackingFile::UwsmEnv,
                BackingFile::HyprlandConf,
            ],
            &params,
            &all_capabilities(),
        );
        assert_eq!(
            planned,
            vec![
                ReloadAction::HyprctlReload,
                ReloadAction::GsettingsSet {
                    key: GSETTINGS_CURSOR_THEME,
                    value: "Nordic-cursors".to_string()
                },
                ReloadAction::GsettingsSet {
                    key: GSETTINGS_CURSOR_SIZE,
                    value: "16".to_string()
                },
                ReloadAction::HyprctlSetCursor {
                    theme: "Nordic-cursors".to_string(),
                    size: 16
                },
            ]
        );
    }

    #[test]
    fn plan_drops_actions_for_absent_or_stopped_components() {
        // Accept criterion: an action is dropped when its component is absent or not
        // running. With hyprctl present but eww/kitty stopped, a palette change plans
        // only the reloads whose components are live.
        let caps = Capabilities::for_tests(
            &[Binary::Hyprctl],
            &[Daemon::Swaync], // eww and kitty are NOT live
            true,
        );
        let planned = plan_reloads(&[BackingFile::Palette], &ReloadParams::default(), &caps);
        assert_eq!(
            planned,
            vec![ReloadAction::HyprctlReload, ReloadAction::SwayncReload],
            "eww reload and kitty SIGUSR1 are dropped when those daemons are not live"
        );
    }

    #[test]
    fn plan_drops_hyprctl_actions_when_hyprland_is_not_reloadable() {
        // Without a live Hyprland IPC socket, every hyprctl-family action is dropped
        // even though the daemons are live.
        let caps = Capabilities::for_tests(
            &[Binary::Hyprctl, Binary::Gsettings],
            &[
                Daemon::Eww,
                Daemon::Swaync,
                Daemon::Kitty,
                Daemon::Hyprpaper,
            ],
            false, // no live IPC socket
        );
        // hyprpaper.conf needs `hyprctl hyprpaper`, so it is dropped.
        assert!(
            plan_reloads(&[BackingFile::HyprpaperConf], &full_params(), &caps).is_empty(),
            "hyprpaper reload needs a reloadable Hyprland"
        );
        // A palette change keeps eww/swaync/kitty but drops the hyprctl reload.
        assert_eq!(
            plan_reloads(&[BackingFile::Palette], &ReloadParams::default(), &caps),
            vec![
                ReloadAction::EwwReload,
                ReloadAction::SwayncReload,
                ReloadAction::KittyColors,
            ]
        );
    }

    #[test]
    fn plan_drops_gsettings_when_the_binary_is_absent() {
        // Without gsettings, the theme/cursor gsettings calls are dropped; the
        // hyprctl setcursor survives (it needs only a reloadable Hyprland).
        let caps = Capabilities::for_tests(&[Binary::Hyprctl], &[], true);
        let planned = plan_reloads(&[BackingFile::GtkSettings], &full_params(), &caps);
        assert_eq!(
            planned,
            vec![ReloadAction::HyprctlSetCursor {
                theme: "Nordic-cursors".to_string(),
                size: 16
            }],
            "gsettings set is dropped when gsettings is absent, setcursor remains"
        );
    }

    #[test]
    fn plan_drops_hyprpaper_when_the_daemon_is_dead() {
        // The hyprpaper reload needs BOTH a reloadable Hyprland AND the hyprpaper
        // daemon running. With Hyprland reloadable but hyprpaper not live, a
        // hyprpaper.conf change plans nothing.
        let caps = Capabilities::for_tests(&[Binary::Hyprctl], &[], true);
        assert!(
            plan_reloads(&[BackingFile::HyprpaperConf], &full_params(), &caps).is_empty(),
            "hyprpaper reload is dropped when the hyprpaper daemon is not live"
        );
    }

    #[test]
    fn plan_drops_hypridle_when_the_daemon_is_absent() {
        // A hypridle.conf change plans no restart when hypridle is not running.
        let caps = Capabilities::for_tests(&[Binary::Hyprctl], &[Daemon::Eww], true);
        assert!(
            plan_reloads(
                &[BackingFile::HypridleConf],
                &ReloadParams::default(),
                &caps
            )
            .is_empty(),
            "hypridle restart is dropped when hypridle is not live"
        );
    }

    // --- Execution: exact arg vectors via the mock recorder -----------------

    #[test]
    fn subprocess_actions_run_their_exact_arg_vectors() {
        // Accept criterion: assert the exact command arg-vectors via the mock
        // recorder, no shell, no interpolation.
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new();

        ReloadAction::HyprctlReload
            .execute(&runner, &signaller)
            .expect("hyprctl reload succeeds under the default mock");
        ReloadAction::EwwReload
            .execute(&runner, &signaller)
            .expect("eww reload succeeds");
        ReloadAction::SwayncReload
            .execute(&runner, &signaller)
            .expect("swaync reload succeeds");
        ReloadAction::GsettingsSet {
            key: GSETTINGS_GTK_THEME,
            value: "Everforest-Green-Dark".to_string(),
        }
        .execute(&runner, &signaller)
        .expect("gsettings set succeeds");
        ReloadAction::HyprctlSetCursor {
            theme: "Nordic-cursors".to_string(),
            size: 16,
        }
        .execute(&runner, &signaller)
        .expect("setcursor succeeds");

        assert_eq!(
            runner.recorded(),
            vec![
                Command::new("hyprctl").arg("reload"),
                Command::new("eww").arg("reload"),
                Command::new("swaync-client").arg("-rs"),
                Command::new("gsettings").args([
                    "set",
                    "org.gnome.desktop.interface",
                    "gtk-theme",
                    "Everforest-Green-Dark",
                ]),
                Command::new("hyprctl").args(["setcursor", "Nordic-cursors", "16"]),
            ]
        );
    }

    #[test]
    fn hyprpaper_action_preloads_then_sets_the_wallpaper() {
        // The single hyprpaper action expands to two ordered commands: preload the
        // path, then set it on all outputs (empty monitor field before the comma).
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new();

        ReloadAction::HyprpaperWallpaper {
            path: "/home/u/Pictures/wall.jpg".to_string(),
        }
        .execute(&runner, &signaller)
        .expect("hyprpaper reload succeeds");

        assert_eq!(
            runner.recorded(),
            vec![
                Command::new("hyprctl").args(["hyprpaper", "preload", "/home/u/Pictures/wall.jpg"]),
                Command::new("hyprctl").args([
                    "hyprpaper",
                    "wallpaper",
                    ",/home/u/Pictures/wall.jpg"
                ]),
            ]
        );
    }

    #[test]
    fn hyprpaper_wallpaper_is_not_set_when_preload_fails() {
        // If preload fails, the wallpaper command must not run (short-circuit), and
        // the failure is surfaced as a ReloadError.
        let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake(1))]);
        let signaller = MockProcessSignaller::new();

        let result = ReloadAction::HyprpaperWallpaper {
            path: "/x.jpg".to_string(),
        }
        .execute(&runner, &signaller);

        assert!(matches!(result, Err(ReloadError::NonZeroExit { .. })));
        assert_eq!(
            runner.recorded(),
            vec![Command::new("hyprctl").args(["hyprpaper", "preload", "/x.jpg"])],
            "the wallpaper command must be skipped after a failed preload"
        );
    }

    // --- Execution: the kitty signal seam ------------------------------------

    #[test]
    fn kitty_reload_signals_sigusr1_to_the_running_kitty_pids() {
        // Accept criterion: the kitty SIGUSR1 seam records the intended PIDs, with no
        // real signal delivered.
        let runner = MockCommandRunner::new();
        let signaller =
            MockProcessSignaller::with_running([(KITTY_PROCESS.to_string(), vec![4242, 4243])]);

        ReloadAction::KittyColors
            .execute(&runner, &signaller)
            .expect("kitty reload succeeds");

        assert_eq!(
            signaller.calls(),
            vec![SignalCall {
                process_name: KITTY_PROCESS.to_string(),
                signal: Signal::SIGUSR1,
                pids: vec![4242, 4243],
            }]
        );
        // The kitty reload issues no subprocess command.
        assert!(runner.recorded().is_empty());
    }

    #[test]
    fn kitty_reload_with_no_running_kitty_is_a_no_op_success() {
        // A race where kitty exited before the reload is not a failure.
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::new(); // no kitty running

        ReloadAction::KittyColors
            .execute(&runner, &signaller)
            .expect("a missing kitty is a no-op, not an error");
        assert_eq!(
            signaller.calls(),
            vec![SignalCall {
                process_name: KITTY_PROCESS.to_string(),
                signal: Signal::SIGUSR1,
                pids: Vec::new(),
            }]
        );
    }

    #[test]
    fn kitty_reload_surfaces_a_signal_enumeration_failure() {
        // When process enumeration itself fails (not merely "no kitty running"), the
        // kitty reload surfaces a ReloadError::Signal (R5.5).
        let runner = MockCommandRunner::new();
        let signaller = MockProcessSignaller::failing(io::ErrorKind::PermissionDenied);

        let error = ReloadAction::KittyColors
            .execute(&runner, &signaller)
            .expect_err("a signal-enumeration failure must be surfaced");
        assert!(matches!(error, ReloadError::Signal(_)));
    }

    // --- Execution: hypridle restart, both paths -----------------------------

    #[test]
    fn hypridle_restart_uses_systemctl_when_the_unit_is_active() {
        // When `systemctl --user is-active hypridle` reports active, hypridle is
        // managed by systemd: it is restarted through the unit, with no kill or
        // respawn. The default mock returns success for every command, so is-active
        // reads as active.
        let runner = MockCommandRunner::new();
        let signaller =
            MockProcessSignaller::with_running([(HYPRIDLE_PROCESS.to_string(), vec![9])]);

        ReloadAction::HypridleRestart
            .execute(&runner, &signaller)
            .expect("the systemd restart path succeeds");

        assert_eq!(
            runner.recorded(),
            vec![
                Command::new("systemctl").args(["--user", "is-active", "--quiet", "hypridle"]),
                Command::new("systemctl").args(["--user", "restart", "hypridle"]),
            ],
            "an active unit is restarted via systemctl, not killed"
        );
        assert!(
            signaller.calls().is_empty(),
            "no SIGTERM when the unit is active"
        );
    }

    #[test]
    fn hypridle_restart_falls_back_to_kill_and_respawn_when_the_unit_is_inactive() {
        // When `is-active` reports non-zero (no active unit — the target dotfiles
        // case, where hypridle runs via exec-once), the fallback kills the running
        // hypridle with SIGTERM and respawns it with `setsid --fork`. This is exactly
        // the case `try-restart` would have mishandled (exit 0 on an inactive unit).
        let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake(3))]);
        let signaller =
            MockProcessSignaller::with_running([(HYPRIDLE_PROCESS.to_string(), vec![77])]);

        ReloadAction::HypridleRestart
            .execute(&runner, &signaller)
            .expect("the kill + respawn fallback succeeds");

        assert_eq!(
            runner.recorded(),
            vec![
                Command::new("systemctl").args(["--user", "is-active", "--quiet", "hypridle"]),
                Command::new("setsid").args(["--fork", "hypridle"]),
            ],
            "the inactive unit is not restarted; hypridle is respawned via setsid --fork"
        );
        assert_eq!(
            signaller.calls(),
            vec![SignalCall {
                process_name: HYPRIDLE_PROCESS.to_string(),
                signal: Signal::SIGTERM,
                pids: vec![77],
            }],
            "the running hypridle is terminated with SIGTERM before respawn"
        );
    }

    #[test]
    fn hypridle_restart_falls_back_when_systemctl_is_absent() {
        // A `systemctl` that cannot be spawned at all (not installed) is treated as
        // "no active unit", so the kill + respawn fallback still runs.
        let runner = MockCommandRunner::with_outcomes([Err(CommandError::Spawn(io::Error::from(
            io::ErrorKind::NotFound,
        )))]);
        let signaller =
            MockProcessSignaller::with_running([(HYPRIDLE_PROCESS.to_string(), vec![55])]);

        ReloadAction::HypridleRestart
            .execute(&runner, &signaller)
            .expect("the fallback runs when systemctl is absent");

        assert_eq!(
            runner.recorded(),
            vec![
                Command::new("systemctl").args(["--user", "is-active", "--quiet", "hypridle"]),
                Command::new("setsid").args(["--fork", "hypridle"]),
            ]
        );
        assert_eq!(
            signaller.calls(),
            vec![SignalCall {
                process_name: HYPRIDLE_PROCESS.to_string(),
                signal: Signal::SIGTERM,
                pids: vec![55],
            }]
        );
    }

    #[test]
    fn hypridle_restart_surfaces_a_kill_signal_failure() {
        // If enumerating hypridle to terminate it fails, the fallback surfaces a
        // ReloadError::Signal and never reaches the respawn.
        let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake(3))]);
        let signaller = MockProcessSignaller::failing(io::ErrorKind::PermissionDenied);

        let error = ReloadAction::HypridleRestart
            .execute(&runner, &signaller)
            .expect_err("a signal-enumeration failure must be surfaced");
        assert!(matches!(error, ReloadError::Signal(_)));
        assert_eq!(
            runner.recorded(),
            vec![Command::new("systemctl").args(["--user", "is-active", "--quiet", "hypridle"])],
            "the respawn must not run after the kill fails"
        );
    }

    // --- Execution: failure surfacing (R5.5) ---------------------------------

    #[test]
    fn a_non_zero_reload_exit_is_a_reload_error() {
        let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake(2))]);
        let signaller = MockProcessSignaller::new();

        let error = ReloadAction::HyprctlReload
            .execute(&runner, &signaller)
            .expect_err("a non-zero exit must be reported");
        match error {
            ReloadError::NonZeroExit { program, code } => {
                assert_eq!(program, "hyprctl");
                assert_eq!(code, Some(2));
            }
            other => panic!("expected NonZeroExit, got {other:?}"),
        }
    }

    #[test]
    fn a_spawn_failure_reload_is_a_reload_error() {
        let runner = MockCommandRunner::with_outcomes([Err(CommandError::Spawn(io::Error::from(
            io::ErrorKind::NotFound,
        )))]);
        let signaller = MockProcessSignaller::new();

        let error = ReloadAction::EwwReload
            .execute(&runner, &signaller)
            .expect_err("a spawn failure must be reported");
        assert!(matches!(error, ReloadError::Command(_)));
        // The error renders a human-readable message for the UI toast.
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn reload_error_messages_are_human_readable() {
        let cases: Vec<ReloadError> = vec![
            ReloadError::Command(CommandError::Timeout {
                limit: std::time::Duration::from_secs(5),
            }),
            ReloadError::NonZeroExit {
                program: "hyprctl".to_string(),
                code: Some(1),
            },
            ReloadError::NonZeroExit {
                program: "eww".to_string(),
                code: None,
            },
            ReloadError::Signal(io::Error::from(io::ErrorKind::PermissionDenied)),
        ];
        for error in cases {
            assert!(
                !error.to_string().is_empty(),
                "every ReloadError must render a message, got empty for {error:?}"
            );
        }
    }
}
