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
//! # notifd: no palette step, one `Reload()` for the icon theme
//!
//! The notification daemon, notifd (task 11.7), draws its popups and center as eww
//! widgets styled from the generated `_colors.scss`, so a palette switch reaches it
//! through `eww reload`; the palette chain has no notification step. notifd's own
//! `Reload()` D-Bus method re-reads `~/.config/notifd/config` and the icon theme
//! (from `gtk-3.0/settings.ini`) without dropping popups, timers, inhibitors or
//! history (analysis §8.2), so it is the [`ReloadAction::NotifdReload`] step: a
//! `settings.ini` write that changed the icon theme asks for it (task 11.14), and
//! so does a write of the notifd config file (task 11.15). [`plan_reloads`]
//! deduplicates, so one Apply calls it once however many writes asked. It needs a
//! running daemon of version 1.1 or later, the first with `Reload()`; an older one
//! is skipped and the Theme page says that notification icons follow at the next
//! notifd start. The app never restarts the unit (that would drop the popups on
//! screen) and never sends SIGHUP (which stops the daemon). The D-Bus call itself is
//! built in [`crate::core::notifd`].
//!
//! # The wallpaper reload and its bar follow-up
//!
//! A `hyprpaper.conf` change is applied live with **one** IPC request, `hyprctl
//! hyprpaper wallpaper ,<absolute path>[,<fit>]` — there is no `preload` step:
//! hyprctl on the target stack (hyprpaper 0.8.4) knows only the `wallpaper` and
//! `listactive` requests, so a `preload` request fails and, run first, kept the
//! wallpaper from ever being set (task 11.14). Which fit modes are passed is
//! explained on [`ReloadAction::HyprpaperWallpaper`]. After it, when the eww bar's
//! `~/.config/eww/scripts/bar-text-tone` script exists and eww runs, the script runs
//! with `--apply` so the bar re-picks its text colour from the new image
//! ([`ReloadAction::BarTextTone`]; architecture §6, analysis §7.8).
//!
//! # Reload order
//!
//! [`plan_reloads`] sorts the actions into one fixed order (the private
//! `ReloadAction::order_key`): the Hyprland reload, the `gsettings` keys,
//! `hyprctl setcursor`, the hyprpaper wallpaper, the hypridle restart, `eww reload`,
//! the bar text tone, kitty's SIGUSR1, and notifd's `Reload()`. The bar text tone
//! follows `eww reload`: the bar's tone is an eww `defpoll` (`bar_tone`, polled
//! hourly from the same script), which `eww reload` re-initialises to its `:initial`
//! value and re-polls (analysis §7.8). A tone pushed before the reload would be
//! thrown away; pushing it afterwards with `--apply` guarantees the result
//! regardless of poll timing. It also follows the hyprpaper request so the bar's
//! colour changes with the new image rather than before it (the script reads the
//! image from `hyprpaper.conf`, already rewritten by the write phase). notifd's
//! `Reload()` depends on nothing but the written `settings.ini`, so it goes last,
//! after the steps the user sees on the desktop.
//!
//! # Kept in sync with `scripts/apply-theme`
//!
//! For a palette (color-scheme) change the reload set mirrors the dotfiles'
//! canonical `scripts/apply-theme` wrapper (analysis §6.1, §8.1) exactly:
//! `hyprctl reload config-only`, then `eww reload`, then a SIGUSR1 to kitty. The
//! app runs `generate-colors` and issues these reloads itself rather than calling
//! the script (architecture §6), so this table and that script must be kept in
//! step: if the script's reload chain changes, [`BackingFile::Palette`]'s mapping
//! changes with it.
//!
//! # Capability gating (R4.2/R5.5)
//!
//! An action is emitted only when its target component is present *and*, for a
//! daemon, detection found it running — "reload only the components that changed
//! and are running". [`ReloadAction::is_available`] encodes this against
//! [`Capabilities`]; [`plan_reloads`] applies it. A reload for an absent or stopped
//! component is silently dropped, never attempted.
//!
//! # Live option changes go through `hyprctl eval` (task 11.2)
//!
//! Hyprland's Lua config provider (Hyprland ≥ 0.55, the provider the target
//! dotfiles run, analysis §7.1) rejects `hyprctl keyword …` with `keyword can't
//! work with non-legacy parsers. Use eval.` — and, worse, still exits 0 while doing
//! so. A live option change is therefore sent as one Lua call through `hyprctl
//! eval`, built and run by [`HyprlandEval`]; success is the literal reply `ok` on
//! stdout, so a zero exit alone is never taken as success. The app issues no
//! `hyprctl keyword` anywhere. `hyprctl reload`, `setcursor`, `monitors -j`,
//! `getoption` and the hyprpaper IPC are requests rather than keywords and work on
//! both providers, so they stay as they are.
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
use std::path::Path;

use nix::sys::signal::Signal;

use crate::core::detect::{Binary, Capabilities, Daemon};
use crate::core::notifd::{NotifdMethod, control_command};
use crate::parsers::lua_literal::{LuaLiteral, LuaLiteralError, is_identifier};
use crate::system::command::{Command, CommandError, CommandRunner, STDERR_EXCERPT_LIMIT};
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

/// The argument that makes the eww bar's `bar-text-tone` script push its result with
/// `eww update bar_tone=…` instead of only printing it (analysis §7.8).
const BAR_TEXT_TONE_APPLY: &str = "--apply";

/// The fit modes hyprpaper's IPC renders differently from its default, `cover`
/// (see [`ReloadAction::HyprpaperWallpaper`]).
const LIVE_FIT_MODES: &[&str] = &["contain", "tile"];

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
pub enum BackingFile {
    /// `config/hypr/monitors.conf` — a display change; the only file whose change
    /// takes the full `hyprctl reload`, because the monitor layout must be
    /// re-applied (tasks 6.1, 11.11).
    MonitorsConf,
    /// `config/hypr/input.lua` — the app-owned `hl.config({ input = { … } })` table
    /// that `hyprland.lua` loads with `dofile` (analysis §7.1, §8.6); re-read with
    /// `hyprctl reload config-only` (tasks 6.6, 11.4).
    InputLua,
    /// `config/hypr/hyprland.lua` — the Lua Hyprland config, of which the app edits
    /// only the two cursor `hl.env("XCURSOR_THEME", …)` / `hl.env("XCURSOR_SIZE", …)`
    /// lines (task 11.5, R3.4); `hyprctl reload config-only` re-runs the file
    /// (task 11.11). The live cursor change itself is applied by the cursor reload
    /// emitted for the GTK/env files.
    HyprlandLua,
    /// `config/hypr/hypridle.conf` — idle timeouts / lock command; hypridle is
    /// restarted to pick them up (task 6.8).
    HypridleConf,
    /// `config/hypr/hyprlock.conf` — the lock screen. **No reload**: hyprlock reads
    /// its config only at launch, so a change takes effect at the next lock
    /// (intentional, architecture §6, task 6.5).
    HyprlockConf,
    /// `config/hypr/hyprpaper.conf` — the wallpaper; applied live with one
    /// `hyprctl hyprpaper wallpaper` request (no `preload`, which the installed
    /// hyprctl does not know), followed by the eww bar's text-tone script when it is
    /// present (tasks 6.5, 11.14).
    HyprpaperConf,
    /// The GTK 3/4 `settings.ini` files — GTK/icon/cursor theme; applied live with
    /// `gsettings set` (+ `hyprctl setcursor` for the cursor), task 6.4. An icon-theme
    /// change also asks notifd to `Reload()`, because notifd reads the icon theme
    /// from `gtk-3.0/settings.ini` only when it starts or reloads (task 11.14).
    GtkSettings,
    /// `config/uwsm/env` — the cursor `XCURSOR_*` env copy; the cursor is applied
    /// live with `gsettings set cursor-*` + `hyprctl setcursor` (task 6.4).
    UwsmEnv,
    /// `~/.icons/default/index.theme` — the XCursor fallback theme, whose
    /// `[Icon Theme]` `Inherits=` names the cursor theme for programs that ignore
    /// `XCURSOR_THEME` (task 11.5, analysis §8.4). Nothing re-reads it on demand
    /// (libXcursor reads it when a program loads its cursors), so like `uwsm/env` it
    /// carries only the shared live cursor reload.
    CursorFallbackTheme,
    /// `~/.config/notifd/config` — notifd's seven tunables (task 11.15); re-read
    /// with notifd's `Reload()` D-Bus method, which keeps on-screen popups, timers and
    /// the history (analysis §8.2). Never a unit restart.
    NotifdConfig,
    /// A palette (color-scheme) switch: `colors/<scheme>` is edited and
    /// `generate-colors` rewrites the color partials. Triggers the broad
    /// apply-theme reload chain, `hyprctl reload config-only` first (analysis §6.1,
    /// §8.1).
    Palette,
}

impl BackingFile {
    /// Every backing file, in declaration order.
    ///
    /// Lets a caller or test cover the whole table without keeping its own list of
    /// variants; a new variant must be added here as well.
    pub const ALL: &'static [BackingFile] = &[
        BackingFile::MonitorsConf,
        BackingFile::InputLua,
        BackingFile::HyprlandLua,
        BackingFile::HypridleConf,
        BackingFile::HyprlockConf,
        BackingFile::HyprpaperConf,
        BackingFile::GtkSettings,
        BackingFile::UwsmEnv,
        BackingFile::CursorFallbackTheme,
        BackingFile::NotifdConfig,
        BackingFile::Palette,
    ];

    /// The reload actions this file's change requires, in canonical order, before
    /// capability gating.
    ///
    /// `params` supplies the runtime values the parameterized actions need (the
    /// wallpaper path, the cursor theme/size, the GTK/icon theme names). The Apply
    /// pipeline (task 4.5) fills a field only for a value that actually changed, so
    /// e.g. a change that touched only the icon theme yields only the icon-theme
    /// `gsettings set` and notifd's `Reload()`. A parameterized file whose value is
    /// missing from `params` yields no action for it (with a warning) rather than an
    /// ill-formed command.
    ///
    /// `capabilities` is consulted only to *locate* a follow-up script detection
    /// found — the eww bar's text-tone script, whose path the action must carry;
    /// whether a component is running is still decided by
    /// [`ReloadAction::is_available`] in [`plan_reloads`].
    pub fn reload_actions(
        self,
        params: &ReloadParams,
        capabilities: &Capabilities,
    ) -> Vec<ReloadAction> {
        match self {
            // An input or cursor change only needs Hyprland to re-run its config,
            // not to re-apply the monitor layout, so it takes the lighter
            // `config-only` form — the one `scripts/apply-theme` uses (architecture
            // §6, analysis §8.1, tasks 11.4 and 11.11). hyprland.lua carries only the
            // two cursor `hl.env` lines the app edits; the live cursor apply itself
            // rides on the GTK/env files' actions.
            BackingFile::InputLua | BackingFile::HyprlandLua => {
                vec![ReloadAction::HyprctlReload(HyprlandReloadScope::ConfigOnly)]
            }
            // The only full Hyprland reload: a monitors.conf change needs the
            // monitor rules re-applied, which `config-only` deliberately skips.
            BackingFile::MonitorsConf => {
                vec![ReloadAction::HyprctlReload(HyprlandReloadScope::Full)]
            }
            BackingFile::HypridleConf => vec![ReloadAction::HypridleRestart],
            // notifd re-reads its file in place on `Reload()`; an icon-theme change in
            // the same Apply asks for the same call, and `plan_reloads` issues it once.
            BackingFile::NotifdConfig => vec![ReloadAction::NotifdReload],
            // hyprlock intentionally has no reload (see the variant docs).
            BackingFile::HyprlockConf => Vec::new(),
            BackingFile::HyprpaperConf => match &params.wallpaper {
                Some(path) => {
                    let mut actions = vec![ReloadAction::HyprpaperWallpaper {
                        path: path.clone(),
                        fit: params.fit.clone(),
                    }];
                    actions.extend(bar_text_tone_action(capabilities));
                    actions
                }
                None => {
                    tracing::warn!(
                        "hyprpaper.conf changed but no wallpaper path was provided; \
                         skipping the hyprpaper reload"
                    );
                    Vec::new()
                }
            },
            // The GTK settings.ini files carry the GTK theme, icon theme, and cursor;
            // uwsm/env and the XCursor fallback theme carry only the cursor copy.
            BackingFile::GtkSettings => theme_and_cursor_actions(params, true),
            BackingFile::UwsmEnv | BackingFile::CursorFallbackTheme => {
                theme_and_cursor_actions(params, false)
            }
            // A palette switch reloads every component whose colors were regenerated,
            // with the same commands and order as `scripts/apply-theme` (analysis
            // §6.1, §8.1, §8.7): the regenerated colors.lua is picked up by a
            // `config-only` reload, as the script does. There is no notification
            // step: notifd's popups are eww widgets, recolored by `eww reload`.
            BackingFile::Palette => vec![
                ReloadAction::HyprctlReload(HyprlandReloadScope::ConfigOnly),
                ReloadAction::EwwReload,
                ReloadAction::KittyColors,
            ],
        }
    }
}

/// The eww bar's text-tone follow-up of a wallpaper change, or `None` when detection
/// found no runnable script (task 11.14; architecture §6, analysis §7.8).
///
/// The script decides the bar's text colour from the wallpaper image and `--apply`
/// pushes it to the running bar. Its absence is not a problem the user must hear
/// about — detection already logged it at `info` — so it is only noted at `debug`
/// here.
fn bar_text_tone_action(capabilities: &Capabilities) -> Option<ReloadAction> {
    let Some(script) = capabilities.bar_text_tone_script() else {
        tracing::debug!(
            "no eww bar-text-tone script was detected; the bar text colour is not \
             refreshed after this wallpaper change"
        );
        return None;
    };
    // Detection reports only UTF-8 paths (a command's program is a `String`), so
    // this conversion is never lossy; a non-UTF-8 path is skipped like an absent one.
    let script = script.to_str()?;
    Some(ReloadAction::BarTextTone {
        script: script.to_string(),
    })
}

/// Builds the theme/cursor reload actions from `params`.
///
/// When `include_gtk_icon` is set (the `settings.ini` files, which hold those keys)
/// the GTK and icon theme `gsettings set` calls are emitted for whichever changed,
/// and an icon-theme change also emits notifd's `Reload()`: notifd looks up
/// notification icons in the theme named by `gtk-3.0/settings.ini`, which it reads
/// only at start and on `Reload()` (analysis §8.2). A GTK-theme or cursor change
/// does not affect notifd and does not emit it.
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
            actions.push(ReloadAction::NotifdReload);
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
pub struct ReloadParams {
    /// The new wallpaper path, for the hyprpaper reload (task 6.5).
    pub wallpaper: Option<String>,
    /// The new wallpaper fit mode, applied live in the same hyprpaper reload (task
    /// 6.5). hyprpaper's `wallpaper` IPC takes the fit as an optional third
    /// comma-field (`monitor,path,fit_mode`), so without this a fit change would be
    /// written to `hyprpaper.conf` but never take effect until hyprpaper restarts.
    /// Set only when a fit is configured; which values actually reach the IPC is
    /// decided by [`ReloadAction::HyprpaperWallpaper`].
    pub fit: Option<String>,
    /// The new cursor theme and size, for `hyprctl setcursor` and the `gsettings`
    /// cursor keys (task 6.4).
    pub cursor: Option<CursorValue>,
    /// The new GTK theme name, for `gsettings set … gtk-theme` (task 6.4).
    pub gtk_theme: Option<String>,
    /// The new icon theme name, for `gsettings set … icon-theme` (task 6.4).
    pub icon_theme: Option<String>,
}

/// A cursor theme selection: the theme name and its pixel size.
///
/// Both are written together to every cursor copy the app owns and reloaded
/// together via `hyprctl setcursor <theme> <size>` and the `gsettings` cursor keys
/// (R3.4), so they travel as one value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorValue {
    /// The cursor theme name (e.g. `Nordic-cursors`).
    pub theme: String,
    /// The cursor size in pixels (e.g. `16`).
    pub size: u32,
}

/// How much of its configuration Hyprland re-applies on a `hyprctl reload`
/// (architecture §6; analysis §8.1).
///
/// Hyprland has two reload forms. `hyprctl reload config-only` re-runs the config
/// and applies the options it sets (input, cursor, colors) but skips the
/// compositor's own monitor re-apply; the `hl.monitor()` calls inside the config
/// still execute. It is the cheaper form, and the dotfiles' own
/// `scripts/apply-theme` uses it. The plain `hyprctl reload` additionally has the
/// compositor re-apply the monitor rules, which a `monitors.conf` change needs.
///
/// Neither form is what keeps the laptop panel off while docked: because the
/// config's `hl.monitor()` calls run on both, the panel used to come back on even
/// under `config-only`. The dotfiles' Lua monitor loader now emits the panel rule
/// as disabled whenever an external output is enabled (unless the user forced the
/// panel on), which makes both forms docked-safe (analysis §8.1).
///
/// Every Hyprland-side write takes [`ConfigOnly`](Self::ConfigOnly) — `input.lua`,
/// the cursor lines of `hyprland.lua` and the palette switch — except
/// `monitors.conf`, which takes [`Full`](Self::Full) (architecture §6, task 11.11).
/// When one Apply needs both forms, [`plan_reloads`] issues only the full reload,
/// because it does everything the lighter one would.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HyprlandReloadScope {
    /// `hyprctl reload config-only`: re-run the config (its `hl.monitor()` calls
    /// included) without the compositor's own monitor re-apply.
    ConfigOnly,
    /// `hyprctl reload`: re-run the config and have the compositor re-apply the
    /// monitor rules.
    Full,
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
pub enum ReloadAction {
    /// `hyprctl reload` or `hyprctl reload config-only` — Hyprland re-runs its
    /// config (`hyprland.lua` and every file it loads); the
    /// [`HyprlandReloadScope`] picks the form.
    HyprctlReload(HyprlandReloadScope),
    /// `eww reload` — recompile and reload the eww widgets: the bars and the
    /// notification popups and center notifd draws through eww.
    EwwReload,
    /// SIGUSR1 to every running kitty, which re-reads its config (v1 has no kitty
    /// remote-control reload; analysis §6.1).
    KittyColors,
    /// Set the wallpaper via hyprpaper with exactly one request, `hyprctl hyprpaper
    /// wallpaper ,<path>[,<fit>]` (tasks 6.5, 11.14). The empty first field is the
    /// monitor: empty means every output.
    ///
    /// There is deliberately no `hyprctl hyprpaper preload` first: the hyprctl of
    /// the target stack (hyprpaper 0.8.4) knows only the `wallpaper` and
    /// `listactive` requests, answers `preload` with "invalid hyprpaper request",
    /// and the `wallpaper` request loads the image itself.
    ///
    /// The fit field is sent only for `contain` and `tile`. Over IPC, hyprctl's
    /// fit parser recognises only `contain`, `tile` and `fit`/`stretch`, and the
    /// daemon renders the last two as `cover`, its default when the field is left
    /// out — so `contain` and `tile` are the only modes that look different live.
    /// `cover`, `fill` (which the config file accepts and the app writes there) and
    /// any other value are therefore sent without the field: the live result is the
    /// same `cover` it would be with a passed-through value, without ever sending a
    /// token whose handling the IPC does not define. The file keeps the chosen
    /// mode, so a `fill` wallpaper is shown stretched from the next hyprpaper start
    /// on.
    HyprpaperWallpaper {
        /// The absolute image path to display (hyprpaper's IPC rejects a path that
        /// does not start with `/`; the wallpaper model expands `~/` before this).
        path: String,
        /// The configured fit mode, or `None` when the file sets none; see the
        /// variant docs for which values reach the IPC.
        fit: Option<String>,
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
    /// `<script> --apply` — the eww bar's `bar-text-tone` script re-scores the bar's
    /// text colour against the wallpaper behind it and pushes the result with `eww
    /// update` (task 11.14; architecture §6, analysis §7.8). Runs after a wallpaper
    /// change, only while eww runs (an `eww update` without a daemon has no bar to
    /// update). Best-effort and non-fatal like every reload (R5.5).
    BarTextTone {
        /// The detected script path
        /// ([`Capabilities::bar_text_tone_script`], always UTF-8).
        script: String,
    },
    /// notifd's `Reload()` D-Bus method: `busctl --user --auto-start=no call
    /// se.dotfiles.Notifd /se/dotfiles/Notifd se.dotfiles.Notifd Reload` (task
    /// 11.14; R5.3; analysis §8.2). notifd re-reads its config file and the icon
    /// theme in place, keeping popups, timers, inhibitors and history. Only a
    /// running daemon of version 1.1 or later has the method; never replaced by a
    /// unit restart (which drops the popups on screen) or SIGHUP (which stops it).
    /// One variant serves every write that needs it — the icon theme in
    /// `settings.ini` and the notifd config file (task 11.15) — so
    /// [`plan_reloads`]' deduplication issues it once per Apply.
    NotifdReload,
}

impl ReloadAction {
    /// Whether this action's target component is present and (for a daemon) running,
    /// so the reload should be issued (R4.2/R5.5).
    ///
    /// Hyprland-family actions (`hyprctl reload`/`setcursor`, and `hyprctl
    /// hyprpaper`) require [`Capabilities::hyprland_reloadable`] — the `hyprctl`
    /// client plus a live IPC socket; the hyprpaper action additionally requires the
    /// hyprpaper daemon to be running. The daemon reloads (`eww`, kitty, hypridle)
    /// require that daemon to be live. `gsettings set` requires the
    /// `gsettings` binary. The bar text tone requires the detected script (the very
    /// path the action carries) and a running eww. notifd's `Reload()` requires a
    /// running notifd of version 1.1 or later
    /// ([`NotifdStatus::supports_state_and_reload`](crate::core::notifd::NotifdStatus::supports_state_and_reload));
    /// an older or stopped daemon picks the icon theme up at its next start. A
    /// component that is absent or stopped yields `false`, so [`plan_reloads`] drops
    /// the action.
    pub fn is_available(&self, capabilities: &Capabilities) -> bool {
        match self {
            ReloadAction::HyprctlReload(_) | ReloadAction::HyprctlSetCursor { .. } => {
                capabilities.hyprland_reloadable()
            }
            ReloadAction::HyprpaperWallpaper { .. } => {
                capabilities.hyprland_reloadable() && capabilities.is_daemon_live(Daemon::Hyprpaper)
            }
            ReloadAction::EwwReload => capabilities.is_daemon_live(Daemon::Eww),
            ReloadAction::KittyColors => capabilities.is_daemon_live(Daemon::Kitty),
            ReloadAction::HypridleRestart => capabilities.is_daemon_live(Daemon::Hypridle),
            ReloadAction::GsettingsSet { .. } => capabilities.has_binary(Binary::Gsettings),
            ReloadAction::BarTextTone { script } => {
                capabilities.bar_text_tone_script() == Some(Path::new(script))
                    && capabilities.is_daemon_live(Daemon::Eww)
            }
            ReloadAction::NotifdReload => capabilities.notifd().supports_state_and_reload(),
        }
    }

    /// A total ordering key placing actions in a deterministic reload order.
    ///
    /// Used by [`plan_reloads`] to sort the merged action list so a combined change
    /// always reloads in the same sequence, and so a palette change's chain comes
    /// out in the apply-theme order (`hyprctl reload config-only` → `eww reload` →
    /// kitty; analysis §6.1). The second element sub-orders the several
    /// `gsettings set` keys deterministically. Why the two follow-ups of task 11.14
    /// sit where they do is explained under "Reload order" in the module docs: the
    /// bar text tone after the hyprpaper request *and* after `eww reload` (which
    /// re-initialises the bar's `bar_tone` poll to its `:initial` value and re-polls
    /// it, so the `--apply` guarantees the result regardless of poll timing),
    /// notifd's `Reload()` last.
    fn order_key(&self) -> (u8, u8) {
        match self {
            // Both forms share the Hyprland slot so they stay adjacent in the sorted
            // plan; the fold in [`plan_reloads`] that drops `config-only` beside a
            // full reload does not depend on their relative order.
            ReloadAction::HyprctlReload(HyprlandReloadScope::Full) => (0, 0),
            ReloadAction::HyprctlReload(HyprlandReloadScope::ConfigOnly) => (0, 1),
            ReloadAction::GsettingsSet { key, .. } => (1, gsettings_key_rank(key)),
            ReloadAction::HyprctlSetCursor { .. } => (2, 0),
            ReloadAction::HyprpaperWallpaper { .. } => (3, 0),
            ReloadAction::HypridleRestart => (4, 0),
            ReloadAction::EwwReload => (5, 0),
            ReloadAction::BarTextTone { .. } => (6, 0),
            ReloadAction::KittyColors => (7, 0),
            ReloadAction::NotifdReload => (8, 0),
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
    pub fn execute(
        &self,
        runner: &dyn CommandRunner,
        signaller: &dyn ProcessSignaller,
    ) -> Result<(), ReloadError> {
        match self {
            ReloadAction::HyprctlReload(HyprlandReloadScope::Full) => {
                run_and_check(runner, Command::new("hyprctl").arg("reload"))
            }
            ReloadAction::HyprctlReload(HyprlandReloadScope::ConfigOnly) => run_and_check(
                runner,
                Command::new("hyprctl").arg("reload").arg("config-only"),
            ),
            ReloadAction::EwwReload => run_and_check(runner, Command::new("eww").arg("reload")),
            ReloadAction::HyprpaperWallpaper { path, fit } => run_and_check(
                runner,
                Command::new("hyprctl")
                    .arg("hyprpaper")
                    .arg("wallpaper")
                    .arg(hyprpaper_wallpaper_argument(path, fit.as_deref())),
            ),
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
            ReloadAction::BarTextTone { script } => run_and_check(
                runner,
                Command::new(script.as_str()).arg(BAR_TEXT_TONE_APPLY),
            ),
            ReloadAction::NotifdReload => {
                run_and_check(runner, control_command(NotifdMethod::Reload))
            }
        }
    }
}

/// The single argument of the `hyprctl hyprpaper wallpaper` request:
/// `,<path>,<fit>` for the fit modes that render differently live (`contain`,
/// `tile`), `,<path>` for every other value or none — see
/// [`ReloadAction::HyprpaperWallpaper`] for why. The leading empty field targets
/// every output.
fn hyprpaper_wallpaper_argument(path: &str, fit: Option<&str>) -> String {
    match fit {
        Some(fit) if LIVE_FIT_MODES.contains(&fit) => format!(",{path},{fit}"),
        _ => format!(",{path}"),
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
/// `hyprland.lua` and `index.theme`, or an icon theme and the notifd config file
/// that both ask for notifd's `Reload()`) issues each reload once, in a
/// deterministic order. When both Hyprland reload forms are planned, only the full
/// one is kept (see [`HyprlandReloadScope`]). This is the list the Apply pipeline
/// (task 4.5) executes after writing the files.
pub fn plan_reloads(
    changed: &[BackingFile],
    params: &ReloadParams,
    capabilities: &Capabilities,
) -> Vec<ReloadAction> {
    let requested = changed
        .iter()
        .flat_map(|file| file.reload_actions(params, capabilities))
        .collect();
    gate_order_and_dedup(requested, capabilities)
}

/// The second half of [`plan_reloads`]: drops the actions whose component is absent
/// or stopped, sorts the rest into the canonical order, collapses duplicates, and
/// lets a full Hyprland reload absorb the `config-only` form.
///
/// Kept separate so the deduplication can be tested on a raw action list — for
/// example two requests for notifd's `Reload()` from different files, as an icon
/// theme plus a notifd config write will produce — independently of which
/// [`BackingFile`]s exist today.
fn gate_order_and_dedup(
    requested: Vec<ReloadAction>,
    capabilities: &Capabilities,
) -> Vec<ReloadAction> {
    let mut actions: Vec<ReloadAction> = requested
        .into_iter()
        .filter(|action| {
            let available = action.is_available(capabilities);
            if !available {
                // "Reload only what is running" (R5.3): a skipped reload is normal,
                // not a failure, so it is only traced.
                tracing::debug!(?action, "component absent or not running; reload skipped");
            }
            available
        })
        .collect();
    // A stable sort by the canonical order key groups equal actions together; the
    // subsequent `dedup` (which removes only consecutive equal elements) then
    // collapses them to one.
    actions.sort_by_key(ReloadAction::order_key);
    actions.dedup();
    // A full `hyprctl reload` re-applies everything `config-only` would, so running
    // both would only make Hyprland re-read its config twice.
    if actions.contains(&ReloadAction::HyprctlReload(HyprlandReloadScope::Full)) {
        actions.retain(|action| {
            *action != ReloadAction::HyprctlReload(HyprlandReloadScope::ConfigOnly)
        });
    }
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

/// The reply `hyprctl eval` prints on stdout when the Lua call was accepted
/// (verified on Hyprland 0.56.2, analysis §7.1).
const HYPRCTL_EVAL_OK: &str = "ok";

/// A live Hyprland change rendered as a single Lua call, sent with `hyprctl eval
/// <code>` (task 11.2; architecture §6; analysis §7.1).
///
/// This is the one place the app turns a live option change into Lua source, so
/// every live tweak quotes strings, writes numbers and booleans bare, and applies
/// the forbidden-character rule of [`crate::parsers::lua_literal`] — the same rule
/// the Lua-literal file editor (task 11.3) uses, so a value refused for the file is
/// also refused live. Keys are checked with
/// [`is_identifier`], so neither a key nor a value can inject code into the call.
///
/// The code is passed to `hyprctl` as **one argument** of the argument vector (the
/// quotes in `hyprctl eval 'hl.config({ … })'` are only how a shell user would write
/// it — no shell is involved here). [`Self::run`] treats exactly the stdout reply
/// `ok` as success: a Lua error or an unknown option comes back as `error: …` (exit
/// status 7 on Hyprland 0.56.2), but `hyprctl` also exits 0 for some refusals — the
/// `keyword` rejection quoted in the module docs is one — so the exit status alone is
/// not trustworthy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HyprlandEval {
    /// The Lua source sent to Hyprland, e.g.
    /// `hl.config({ input = { touchpad = { natural_scroll = true } } })`.
    code: String,
}

impl HyprlandEval {
    /// An option change at a dotted table path, rendered as nested tables:
    /// `input.touchpad.natural_scroll` = `true` becomes
    /// `hl.config({ input = { touchpad = { natural_scroll = true } } })`.
    ///
    /// The dotted path is the same addressing the config files use (`input.kb_layout`,
    /// analysis §7.1). `hl.config` merges the given table into the live config, so
    /// only the named option changes. Fails with [`LuaLiteralError::InvalidKey`] when
    /// the path is empty or any segment is not a bare Lua key (for example the
    /// hyprlang spelling `tap-to-click`; the Lua name is `tap_to_click`).
    pub fn config(path: &str, value: &LuaLiteral) -> Result<HyprlandEval, LuaLiteralError> {
        let segments: Vec<&str> = path.split('.').collect();
        if let Some(bad) = segments.iter().find(|segment| !is_identifier(segment)) {
            return Err(LuaLiteralError::InvalidKey((*bad).to_string()));
        }
        // Build the table inside-out: `c = v`, then `b = { c = v }`, and so on.
        // `segments` is never empty (`split` yields at least one item), and every
        // segment was checked above.
        let mut table = String::new();
        for (depth, segment) in segments.iter().rev().enumerate() {
            table = if depth == 0 {
                format!("{segment} = {value}")
            } else {
                format!("{segment} = {{ {table} }}")
            };
        }
        Ok(HyprlandEval {
            code: format!("hl.config({{ {table} }})"),
        })
    }

    /// The rendered Lua source.
    pub fn code(&self) -> &str {
        &self.code
    }

    /// The shell-free command that sends the call: `hyprctl` with the arguments
    /// `eval` and the Lua source as a single argument.
    pub fn command(&self) -> Command {
        Command::new("hyprctl").arg("eval").arg(self.code.as_str())
    }

    /// Sends the call through `runner`, logging the attempt and its outcome (R7.3).
    ///
    /// Success is a zero exit **and** the stdout reply `ok` (surrounding whitespace
    /// ignored). Anything else — a Lua error, an unknown option, a hyprlang session
    /// that does not understand `eval` — is a [`ReloadError::EvalRejected`] carrying
    /// a bounded excerpt of Hyprland's reply; a command that could not run at all is
    /// a [`ReloadError::Command`]. Failures are logged at `error` and returned for
    /// the caller to surface; like every reload they are non-fatal (R5.5).
    pub fn run(&self, runner: &dyn CommandRunner) -> Result<(), ReloadError> {
        let command = self.command();
        match runner.run(&command) {
            Ok(output)
                if output.success()
                    && String::from_utf8_lossy(output.stdout()).trim() == HYPRCTL_EVAL_OK =>
            {
                tracing::info!(command = %command, "hyprctl eval accepted the live change");
                Ok(())
            }
            Ok(output) => {
                let reply = eval_reply_excerpt(output.stdout());
                tracing::error!(
                    command = %command,
                    exit_code = ?output.code(),
                    %reply,
                    "hyprctl eval did not answer `ok`; the live change was not applied (R5.5)"
                );
                Err(ReloadError::EvalRejected {
                    code: output.code(),
                    reply,
                })
            }
            Err(error) => {
                tracing::error!(command = %command, %error, "hyprctl eval could not be run (R5.5)");
                Err(ReloadError::Command(error))
            }
        }
    }
}

/// The leading, trimmed slice of a `hyprctl eval` reply kept for logs and error
/// messages, bounded by [`STDERR_EXCERPT_LIMIT`] like every other captured output
/// the app surfaces. The reply is Hyprland's own diagnostic (e.g. `error: … unknown
/// config key 'input.foo'`), never file contents.
fn eval_reply_excerpt(stdout: &[u8]) -> String {
    let end = stdout.len().min(STDERR_EXCERPT_LIMIT);
    let mut excerpt = String::from_utf8_lossy(&stdout[..end]).trim().to_string();
    // Same convention as the generator-output excerpt in `core/apply.rs`: a trailing
    // `…` tells the reader the reply was cut, not that it ended there.
    if stdout.len() > end {
        excerpt.push('…');
    }
    excerpt
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
/// [`CommandRunner::run`] as a [detached](Command::detached) command: `setsid
/// --fork` forks hypridle into a new session and the `setsid` process itself exits
/// immediately, so `run` reaps `setsid` (no zombie is leaked) while the daemon is
/// reparented to init and survives the app's exit. The detached marker matters:
/// the forked hypridle inherits any capture-pipe write ends, so a
/// *capturing* run would block draining them until the daemon exits — stalling the
/// Apply that triggered the restart for as long as hypridle lives. Detached mode
/// captures nothing, so `run` returns as soon as `setsid` exits. This reuses the
/// existing reaping/timeout machinery rather than a hand-rolled detached spawn. The
/// old and new hypridle may briefly overlap while the terminated one shuts down;
/// that race is benign and this is a best-effort fallback.
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
        Command::new("setsid")
            .arg("--fork")
            .arg(HYPRIDLE_PROCESS)
            .detached(),
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
pub enum ReloadError {
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
    /// A `hyprctl eval` call ran but Hyprland did not answer `ok` (see
    /// [`HyprlandEval::run`]), so the live change was not applied.
    EvalRejected {
        /// The exit code of `hyprctl`, or `None` if it was terminated by a signal.
        /// It may be `0`: `hyprctl` exits 0 for some refusals.
        code: Option<i32>,
        /// A bounded, trimmed excerpt of Hyprland's reply on stdout (empty when it
        /// printed nothing).
        reply: String,
    },
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
            ReloadError::EvalRejected { reply, .. } if reply.is_empty() => {
                write!(f, "Hyprland did not accept the live change (no reply)")
            }
            ReloadError::EvalRejected { reply, .. } => {
                write!(f, "Hyprland did not accept the live change: {reply}")
            }
        }
    }
}

impl std::error::Error for ReloadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ReloadError::Command(error) => Some(error),
            ReloadError::Signal(error) => Some(error),
            ReloadError::NonZeroExit { .. } | ReloadError::EvalRejected { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::notifd::{NotifdStatus, NotifdVersion};
    use crate::system::command::{CommandOutput, MockCommandRunner};
    use crate::system::signal::{MockProcessSignaller, SignalCall};

    /// The bar text-tone script path the test capabilities report.
    const BAR_TEXT_TONE: &str = "/home/u/.config/eww/scripts/bar-text-tone";

    /// A running notifd reporting `version`, as detection resolves it.
    fn notifd_running(version: &str) -> NotifdStatus {
        NotifdStatus::Running {
            version: NotifdVersion::parse(version),
        }
    }

    /// The live components of [`all_capabilities`], for tests that narrow one.
    const ALL_DAEMONS: &[Daemon] = &[
        Daemon::Eww,
        Daemon::Kitty,
        Daemon::Hyprpaper,
        Daemon::Hypridle,
    ];

    /// A capabilities value with every reload target present and running — the
    /// baseline for mapping tests, which a gating test then narrows. notifd runs
    /// version 1.1 (it has `Reload()`) and the eww bar's text-tone script exists.
    fn all_capabilities() -> Capabilities {
        Capabilities::for_tests(&[Binary::Hyprctl, Binary::Gsettings], ALL_DAEMONS, true)
            .with_notifd_for_tests(notifd_running("1.1"))
            .with_bar_text_tone_script_for_tests(BAR_TEXT_TONE.into())
    }

    /// The bar text-tone follow-up for [`all_capabilities`]' script.
    fn bar_text_tone() -> ReloadAction {
        ReloadAction::BarTextTone {
            script: BAR_TEXT_TONE.to_string(),
        }
    }

    /// A short name for an action's kind, without its data, so a test can state a
    /// plan's shape. The match has no `_` arm: a new [`ReloadAction`] variant does not
    /// compile until it is named here, and the order and availability tests below then
    /// fail until its sample is added to [`one_of_each_action`].
    fn kind(action: &ReloadAction) -> &'static str {
        match action {
            ReloadAction::HyprctlReload(HyprlandReloadScope::Full) => "hyprctl reload",
            ReloadAction::HyprctlReload(HyprlandReloadScope::ConfigOnly) => {
                "hyprctl reload config-only"
            }
            ReloadAction::GsettingsSet { key, .. } => match *key {
                GSETTINGS_GTK_THEME => "gsettings gtk-theme",
                GSETTINGS_ICON_THEME => "gsettings icon-theme",
                GSETTINGS_CURSOR_THEME => "gsettings cursor-theme",
                GSETTINGS_CURSOR_SIZE => "gsettings cursor-size",
                _ => "gsettings (other key)",
            },
            ReloadAction::HyprctlSetCursor { .. } => "hyprctl setcursor",
            ReloadAction::HyprpaperWallpaper { .. } => "hyprpaper wallpaper",
            ReloadAction::HypridleRestart => "hypridle restart",
            ReloadAction::EwwReload => "eww reload",
            ReloadAction::BarTextTone { .. } => "bar text tone",
            ReloadAction::KittyColors => "kitty SIGUSR1",
            ReloadAction::NotifdReload => "notifd Reload",
        }
    }

    /// The kinds of `actions`, in order.
    fn kinds(actions: &[ReloadAction]) -> Vec<&'static str> {
        actions.iter().map(kind).collect()
    }

    /// One action of every kind [`kind`] names (each `gsettings` key once), in
    /// declaration order — the input of the order and availability tests.
    fn one_of_each_action() -> Vec<ReloadAction> {
        let gsettings = |key| ReloadAction::GsettingsSet {
            key,
            value: "x".to_string(),
        };
        vec![
            ReloadAction::HyprctlReload(HyprlandReloadScope::Full),
            ReloadAction::HyprctlReload(HyprlandReloadScope::ConfigOnly),
            ReloadAction::EwwReload,
            ReloadAction::KittyColors,
            ReloadAction::HyprpaperWallpaper {
                path: "/w.jpg".to_string(),
                fit: None,
            },
            ReloadAction::HypridleRestart,
            gsettings(GSETTINGS_GTK_THEME),
            gsettings(GSETTINGS_ICON_THEME),
            gsettings(GSETTINGS_CURSOR_THEME),
            gsettings(GSETTINGS_CURSOR_SIZE),
            ReloadAction::HyprctlSetCursor {
                theme: "t".to_string(),
                size: 24,
            },
            bar_text_tone(),
            ReloadAction::NotifdReload,
        ]
    }

    /// Params carrying every parameterized value, so the theme/cursor/wallpaper
    /// files produce their full action sets.
    fn full_params() -> ReloadParams {
        ReloadParams {
            wallpaper: Some("/home/u/Pictures/wall.jpg".to_string()),
            fit: Some("cover".to_string()),
            cursor: Some(CursorValue {
                theme: "Nordic-cursors".to_string(),
                size: 16,
            }),
            gtk_theme: Some("Everforest-Green-Dark".to_string()),
            icon_theme: Some("Everforest-Dark".to_string()),
        }
    }

    // --- File -> actions mapping (accept: each file maps to exactly its actions) ---

    /// The Hyprland reload form a file's change plans, or `None` for a file whose
    /// change needs no Hyprland reload at all.
    fn hyprland_scope(file: BackingFile) -> Option<HyprlandReloadScope> {
        let scopes: Vec<HyprlandReloadScope> = file
            .reload_actions(&full_params(), &all_capabilities())
            .into_iter()
            .filter_map(|action| match action {
                ReloadAction::HyprctlReload(scope) => Some(scope),
                _ => None,
            })
            .collect();
        assert!(
            scopes.len() <= 1,
            "{file:?} must plan at most one Hyprland reload, got {scopes:?}"
        );
        scopes.first().copied()
    }

    #[test]
    fn every_backing_file_maps_to_its_expected_hyprland_reload_scope() {
        // Task 11.11, architecture §6: only monitors.conf takes the full reload;
        // every other Hyprland-side write takes `config-only`; the rest take none.
        // The exhaustive match (no `_` arm) forces a decision at compile time: a new
        // `BackingFile` variant does not build until its expected scope is written
        // here. The residual gap is a variant added to the enum but not to
        // `BackingFile::ALL`, which this loop would then not visit.
        let expected = |file: BackingFile| match file {
            BackingFile::MonitorsConf => Some(HyprlandReloadScope::Full),
            BackingFile::InputLua | BackingFile::HyprlandLua | BackingFile::Palette => {
                Some(HyprlandReloadScope::ConfigOnly)
            }
            BackingFile::HypridleConf
            | BackingFile::HyprlockConf
            | BackingFile::HyprpaperConf
            | BackingFile::GtkSettings
            | BackingFile::UwsmEnv
            | BackingFile::CursorFallbackTheme
            | BackingFile::NotifdConfig => None,
        };
        for &file in BackingFile::ALL {
            assert_eq!(hyprland_scope(file), expected(file), "{file:?}");
        }

        // Task 11.14 extends the same exhaustive table to the whole planned reload of
        // each file, with every component running and every value changed: the
        // wallpaper gains the bar text tone after the single hyprpaper request, and
        // only the settings.ini files ask for notifd's `Reload()` (the icon theme
        // lives there) — never the cursor-only copies — and the notifd config file
        // (task 11.15) asks for nothing but notifd's `Reload()`.
        let expected_plan = |file: BackingFile| -> Vec<&'static str> {
            match file {
                BackingFile::MonitorsConf => vec!["hyprctl reload"],
                BackingFile::InputLua | BackingFile::HyprlandLua => {
                    vec!["hyprctl reload config-only"]
                }
                BackingFile::HypridleConf => vec!["hypridle restart"],
                BackingFile::HyprlockConf => vec![],
                BackingFile::HyprpaperConf => vec!["hyprpaper wallpaper", "bar text tone"],
                BackingFile::GtkSettings => vec![
                    "gsettings gtk-theme",
                    "gsettings icon-theme",
                    "gsettings cursor-theme",
                    "gsettings cursor-size",
                    "hyprctl setcursor",
                    "notifd Reload",
                ],
                BackingFile::UwsmEnv | BackingFile::CursorFallbackTheme => vec![
                    "gsettings cursor-theme",
                    "gsettings cursor-size",
                    "hyprctl setcursor",
                ],
                BackingFile::NotifdConfig => vec!["notifd Reload"],
                BackingFile::Palette => {
                    vec!["hyprctl reload config-only", "eww reload", "kitty SIGUSR1"]
                }
            }
        };
        for &file in BackingFile::ALL {
            assert_eq!(
                kinds(&plan_reloads(&[file], &full_params(), &all_capabilities())),
                expected_plan(file),
                "{file:?}"
            );
        }
    }

    #[test]
    fn every_action_kind_has_its_place_in_the_canonical_order() {
        // Pins `order_key` for every variant (architecture §6, module docs "Reload
        // order"): fed in a scrambled order, the plan comes out with the bar text tone
        // after both the hyprpaper request and `eww reload` (which re-initialises the
        // bar's `bar_tone` poll to its `:initial` value), and notifd's `Reload()` last.
        let mut scrambled = one_of_each_action();
        scrambled.reverse();
        scrambled.rotate_left(5);
        let planned = gate_order_and_dedup(scrambled, &all_capabilities());
        assert_eq!(
            kinds(&planned),
            vec![
                // The full reload absorbs `config-only` when both are present.
                "hyprctl reload",
                "gsettings gtk-theme",
                "gsettings icon-theme",
                "gsettings cursor-theme",
                "gsettings cursor-size",
                "hyprctl setcursor",
                "hyprpaper wallpaper",
                "hypridle restart",
                "eww reload",
                "bar text tone",
                "kitty SIGUSR1",
                "notifd Reload",
            ]
        );
    }

    #[test]
    fn every_action_kind_is_dropped_without_its_component() {
        // Pins `is_available` for every variant: everything runs under
        // `all_capabilities`, nothing under an empty capability set (no binaries, no
        // daemons, no Hyprland socket, no notifd, no script).
        let caps = all_capabilities();
        for action in one_of_each_action() {
            assert!(
                action.is_available(&caps),
                "{} with everything",
                kind(&action)
            );
        }
        let nothing = Capabilities::for_tests(&[], &[], false);
        for action in one_of_each_action() {
            assert!(
                !action.is_available(&nothing),
                "{} with nothing",
                kind(&action)
            );
        }
    }

    #[test]
    fn notifd_reload_needs_a_running_daemon_of_at_least_1_1() {
        // Analysis §8.2: `Reload()` exists from notifd 1.1 on. An older or
        // unversioned daemon, a stopped one and an absent one all skip it — the
        // stopped daemon reads the icon theme when it starts anyway.
        let with = |notifd| Capabilities::for_tests(&[], &[], false).with_notifd_for_tests(notifd);
        let cases = [
            (notifd_running("1.1"), true),
            (notifd_running("2.0"), true),
            (notifd_running("1.0"), false),
            (NotifdStatus::Running { version: None }, false),
            (NotifdStatus::Installed, false),
            (NotifdStatus::Absent, false),
        ];
        for (notifd, expected) in cases {
            assert_eq!(
                ReloadAction::NotifdReload.is_available(&with(notifd)),
                expected,
                "{notifd}"
            );
        }
    }

    #[test]
    fn the_bar_text_tone_needs_its_detected_script_and_a_running_eww() {
        // Task 11.14: skipped when eww is not running (an `eww update` would find no
        // bar), and when detection reported no script or a different one.
        let without_eww = Capabilities::for_tests(&[], &[Daemon::Kitty], false)
            .with_bar_text_tone_script_for_tests(BAR_TEXT_TONE.into());
        assert!(!bar_text_tone().is_available(&without_eww));

        let without_script = Capabilities::for_tests(&[], &[Daemon::Eww], false);
        assert!(!bar_text_tone().is_available(&without_script));

        let other_script = Capabilities::for_tests(&[], &[Daemon::Eww], false)
            .with_bar_text_tone_script_for_tests("/elsewhere/bar-text-tone".into());
        assert!(!bar_text_tone().is_available(&other_script));

        let both = Capabilities::for_tests(&[], &[Daemon::Eww], false)
            .with_bar_text_tone_script_for_tests(BAR_TEXT_TONE.into());
        assert!(bar_text_tone().is_available(&both));
    }

    #[test]
    fn monitors_conf_maps_to_exactly_the_full_reload() {
        // The monitor layout is re-applied only by the full `hyprctl reload`
        // (architecture §6). No params are needed.
        assert_eq!(
            BackingFile::MonitorsConf.reload_actions(&ReloadParams::default(), &all_capabilities()),
            vec![ReloadAction::HyprctlReload(HyprlandReloadScope::Full)]
        );
    }

    #[test]
    fn input_and_cursor_lua_files_map_to_exactly_the_config_only_reload() {
        // Tasks 11.4 and 11.11: an input.lua or hyprland.lua (cursor `hl.env`) change
        // re-runs the config without re-applying the monitor layout, like
        // `scripts/apply-theme` does.
        for file in [BackingFile::InputLua, BackingFile::HyprlandLua] {
            assert_eq!(
                file.reload_actions(&ReloadParams::default(), &all_capabilities()),
                vec![ReloadAction::HyprctlReload(HyprlandReloadScope::ConfigOnly)],
                "{file:?}"
            );
        }
    }

    #[test]
    fn a_full_reload_absorbs_the_config_only_form() {
        // An Apply that wrote both input.lua and monitors.conf reloads Hyprland once,
        // with the full form: it re-applies everything `config-only` would.
        let planned = plan_reloads(
            &[BackingFile::InputLua, BackingFile::MonitorsConf],
            &ReloadParams::default(),
            &all_capabilities(),
        );
        assert_eq!(
            planned,
            vec![ReloadAction::HyprctlReload(HyprlandReloadScope::Full)]
        );

        // The same holds when the palette chain joins a monitors change: the chain's
        // `config-only` step is absorbed, the eww and kitty steps stay in order.
        let planned = plan_reloads(
            &[BackingFile::Palette, BackingFile::MonitorsConf],
            &ReloadParams::default(),
            &all_capabilities(),
        );
        assert_eq!(
            planned,
            vec![
                ReloadAction::HyprctlReload(HyprlandReloadScope::Full),
                ReloadAction::EwwReload,
                ReloadAction::KittyColors,
            ]
        );

        // On its own, the input change keeps the lighter form.
        let planned = plan_reloads(
            &[BackingFile::InputLua],
            &ReloadParams::default(),
            &all_capabilities(),
        );
        assert_eq!(
            planned,
            vec![ReloadAction::HyprctlReload(HyprlandReloadScope::ConfigOnly)]
        );
    }

    #[test]
    fn hyprlock_only_change_yields_no_reload() {
        // Accept criterion: hyprlock reads its config only at launch, so a
        // hyprlock.conf change issues no reload at all (intentional).
        assert!(
            BackingFile::HyprlockConf
                .reload_actions(&ReloadParams::default(), &all_capabilities())
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
            BackingFile::HypridleConf.reload_actions(&params, &all_capabilities()),
            vec![ReloadAction::HypridleRestart],
            "hypridle.conf restarts hypridle, not `hyprctl reload`"
        );
        assert_eq!(
            BackingFile::HyprpaperConf.reload_actions(&params, &all_capabilities()),
            vec![
                ReloadAction::HyprpaperWallpaper {
                    path: "/home/u/Pictures/wall.jpg".to_string(),
                    fit: Some("cover".to_string()),
                },
                bar_text_tone(),
            ],
            "hyprpaper.conf maps to the hyprpaper wallpaper action carrying the fit, \
             then the bar text tone"
        );
        // Without a detected script the follow-up is simply not requested.
        let no_script = Capabilities::for_tests(&[Binary::Hyprctl], ALL_DAEMONS, true);
        assert_eq!(
            kinds(&BackingFile::HyprpaperConf.reload_actions(&params, &no_script)),
            vec!["hyprpaper wallpaper"]
        );
    }

    #[test]
    fn palette_change_maps_to_the_apply_theme_chain_in_order() {
        // Accept criterion: a palette change is the broad apply-theme chain, in the
        // canonical order of `scripts/apply-theme` (analysis §6.1, §8.1, §8.7) —
        // `hyprctl reload config-only` (task 11.11), eww reload, kitty SIGUSR1 — with
        // no notification-daemon step (task 11.7: notifd's popups are eww widgets,
        // recolored by the eww reload).
        assert_eq!(
            BackingFile::Palette.reload_actions(&ReloadParams::default(), &all_capabilities()),
            vec![
                ReloadAction::HyprctlReload(HyprlandReloadScope::ConfigOnly),
                ReloadAction::EwwReload,
                ReloadAction::KittyColors,
            ]
        );
        assert_eq!(
            plan_reloads(
                &[BackingFile::Palette],
                &ReloadParams::default(),
                &all_capabilities()
            ),
            BackingFile::Palette.reload_actions(&ReloadParams::default(), &all_capabilities()),
            "with everything live the planned chain is exactly the three steps, in order"
        );
    }

    #[test]
    fn gtk_settings_maps_to_theme_and_cursor_gsettings_plus_setcursor() {
        // settings.ini carries GTK theme, icon theme, and cursor: all three
        // `gsettings set` calls (in key order) plus `hyprctl setcursor`, and the icon
        // theme also asks notifd to `Reload()` (task 11.14).
        assert_eq!(
            BackingFile::GtkSettings.reload_actions(&full_params(), &all_capabilities()),
            vec![
                ReloadAction::GsettingsSet {
                    key: GSETTINGS_GTK_THEME,
                    value: "Everforest-Green-Dark".to_string()
                },
                ReloadAction::GsettingsSet {
                    key: GSETTINGS_ICON_THEME,
                    value: "Everforest-Dark".to_string()
                },
                ReloadAction::NotifdReload,
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
    fn uwsm_env_and_the_cursor_fallback_theme_map_to_the_cursor_reload_only() {
        // uwsm/env and ~/.icons/default/index.theme hold only the cursor copy, so they
        // never emit the GTK/icon theme gsettings — only the cursor keys and setcursor.
        for file in [BackingFile::UwsmEnv, BackingFile::CursorFallbackTheme] {
            assert_eq!(
                file.reload_actions(&full_params(), &all_capabilities()),
                cursor_reload_actions(),
                "{file:?}"
            );
        }
    }

    /// The cursor reload every cursor copy maps to, for [`full_params`]'s cursor.
    fn cursor_reload_actions() -> Vec<ReloadAction> {
        vec![
            ReloadAction::GsettingsSet {
                key: GSETTINGS_CURSOR_THEME,
                value: "Nordic-cursors".to_string(),
            },
            ReloadAction::GsettingsSet {
                key: GSETTINGS_CURSOR_SIZE,
                value: "16".to_string(),
            },
            ReloadAction::HyprctlSetCursor {
                theme: "Nordic-cursors".to_string(),
                size: 16,
            },
        ]
    }

    #[test]
    fn a_changed_setting_only_emits_actions_for_provided_params() {
        // The pipeline provides a param only for a value that changed; a settings.ini
        // change that touched only the icon theme yields only the icon-theme
        // gsettings and notifd's `Reload()` — no gtk-theme, no cursor.
        let params = ReloadParams {
            icon_theme: Some("Papirus".to_string()),
            ..ReloadParams::default()
        };
        assert_eq!(
            BackingFile::GtkSettings.reload_actions(&params, &all_capabilities()),
            vec![
                ReloadAction::GsettingsSet {
                    key: GSETTINGS_ICON_THEME,
                    value: "Papirus".to_string()
                },
                ReloadAction::NotifdReload,
            ]
        );
    }

    #[test]
    fn an_icon_theme_change_plans_exactly_one_notifd_reload() {
        // Task 11.14 (R5.3, analysis §8.2): an icon-theme-only Apply ends with one
        // `Reload()`; a GTK-theme-only or cursor-only Apply has none, because notifd
        // reads only the icon theme from settings.ini.
        let icon_only = ReloadParams {
            icon_theme: Some("Papirus".to_string()),
            ..ReloadParams::default()
        };
        assert_eq!(
            kinds(&plan_reloads(
                &[BackingFile::GtkSettings],
                &icon_only,
                &all_capabilities()
            )),
            vec!["gsettings icon-theme", "notifd Reload"]
        );

        let gtk_only = ReloadParams {
            gtk_theme: Some("Everforest-Green-Dark".to_string()),
            ..ReloadParams::default()
        };
        assert_eq!(
            kinds(&plan_reloads(
                &[BackingFile::GtkSettings],
                &gtk_only,
                &all_capabilities()
            )),
            vec!["gsettings gtk-theme"]
        );

        let cursor_only = ReloadParams {
            cursor: full_params().cursor,
            ..ReloadParams::default()
        };
        let planned = plan_reloads(
            &[
                BackingFile::GtkSettings,
                BackingFile::UwsmEnv,
                BackingFile::HyprlandLua,
                BackingFile::CursorFallbackTheme,
            ],
            &cursor_only,
            &all_capabilities(),
        );
        assert!(
            !planned.contains(&ReloadAction::NotifdReload),
            "a cursor change does not reload notifd: {planned:?}"
        );
    }

    #[test]
    fn an_icon_theme_and_a_notifd_config_write_plan_one_reload() {
        // Task 11.15 on top of 11.14: a settings.ini write that changed the icon theme
        // and a notifd config write each ask for `Reload()`; one Apply issues it once,
        // last. The config file alone asks for exactly that call.
        let icon_only = ReloadParams {
            icon_theme: Some("Papirus".to_string()),
            ..ReloadParams::default()
        };
        let planned = plan_reloads(
            &[BackingFile::GtkSettings, BackingFile::NotifdConfig],
            &icon_only,
            &all_capabilities(),
        );
        assert_eq!(
            kinds(&planned),
            vec!["gsettings icon-theme", "notifd Reload"]
        );
        assert_eq!(
            plan_reloads(
                &[BackingFile::NotifdConfig],
                &ReloadParams::default(),
                &all_capabilities()
            ),
            vec![ReloadAction::NotifdReload]
        );
    }

    #[test]
    fn a_notifd_config_write_plans_nothing_without_a_reloadable_daemon() {
        // The file-backed rows are hidden for these daemons, so the case is a guard: the
        // gate drops the call rather than restart anything.
        for notifd in [
            notifd_running("1.0"),
            NotifdStatus::Installed,
            NotifdStatus::Absent,
        ] {
            let caps = Capabilities::for_tests(&[], &[], false).with_notifd_for_tests(notifd);
            assert!(
                plan_reloads(
                    &[BackingFile::NotifdConfig],
                    &ReloadParams::default(),
                    &caps
                )
                .is_empty(),
                "{notifd}"
            );
        }
    }

    #[test]
    fn notifd_reload_is_issued_once_however_many_writes_ask_for_it() {
        // Task 11.14: the icon theme and the notifd config file (task 11.15) both ask
        // for `Reload()` in one Apply; the planner issues it once, last.
        let requested = vec![
            ReloadAction::NotifdReload,
            ReloadAction::GsettingsSet {
                key: GSETTINGS_ICON_THEME,
                value: "Papirus".to_string(),
            },
            ReloadAction::NotifdReload,
        ];
        assert_eq!(
            kinds(&gate_order_and_dedup(requested, &all_capabilities())),
            vec!["gsettings icon-theme", "notifd Reload"]
        );
    }

    #[test]
    fn an_older_notifd_gets_no_reload_call() {
        // Analysis §8.2: notifd 1.0 has no `Reload()`; the icon change is still
        // applied through gsettings and notifd picks it up at its next start. The unit
        // is never restarted in its place.
        let caps = Capabilities::for_tests(&[Binary::Hyprctl, Binary::Gsettings], &[], true)
            .with_notifd_for_tests(notifd_running("1.0"));
        let params = ReloadParams {
            icon_theme: Some("Papirus".to_string()),
            ..ReloadParams::default()
        };
        let planned = plan_reloads(&[BackingFile::GtkSettings], &params, &caps);
        assert_eq!(kinds(&planned), vec!["gsettings icon-theme"]);
    }

    #[test]
    fn a_wallpaper_change_runs_the_bar_text_tone_after_hyprpaper_only_with_eww() {
        // Task 11.14 (architecture §6): the bar text tone follows the hyprpaper
        // request, and is skipped when eww is down or the script is absent.
        let planned = plan_reloads(
            &[BackingFile::HyprpaperConf],
            &full_params(),
            &all_capabilities(),
        );
        assert_eq!(
            kinds(&planned),
            vec!["hyprpaper wallpaper", "bar text tone"]
        );

        let eww_down = Capabilities::for_tests(&[Binary::Hyprctl], &[Daemon::Hyprpaper], true)
            .with_bar_text_tone_script_for_tests(BAR_TEXT_TONE.into());
        assert_eq!(
            kinds(&plan_reloads(
                &[BackingFile::HyprpaperConf],
                &full_params(),
                &eww_down
            )),
            vec!["hyprpaper wallpaper"]
        );

        let no_script = Capabilities::for_tests(&[Binary::Hyprctl], ALL_DAEMONS, true);
        assert_eq!(
            kinds(&plan_reloads(
                &[BackingFile::HyprpaperConf],
                &full_params(),
                &no_script
            )),
            vec!["hyprpaper wallpaper"]
        );

        // With a palette switch in the same Apply, the tone is pushed after `eww
        // reload`, which would otherwise re-initialise it to its `:initial` value.
        let planned = plan_reloads(
            &[BackingFile::HyprpaperConf, BackingFile::Palette],
            &full_params(),
            &all_capabilities(),
        );
        assert_eq!(
            kinds(&planned),
            vec![
                "hyprctl reload config-only",
                "hyprpaper wallpaper",
                "eww reload",
                "bar text tone",
                "kitty SIGUSR1",
            ]
        );
    }

    #[test]
    fn hyprpaper_without_a_path_emits_no_action() {
        // A defensive guard: hyprpaper.conf changed but no wallpaper path was
        // supplied (a caller bug) yields no action rather than a malformed command.
        assert!(
            BackingFile::HyprpaperConf
                .reload_actions(&ReloadParams::default(), &all_capabilities())
                .is_empty()
        );
    }

    // --- plan_reloads: dedup + ordering + gating -----------------------------

    #[test]
    fn plan_dedups_and_orders_a_multi_file_cursor_change() {
        // A cursor change writes settings.ini, uwsm/env, hyprland.lua and the
        // XCursor fallback index.theme. The files' actions overlap on the cursor
        // reload and must collapse to one of each, in canonical order: `hyprctl
        // reload config-only` (from hyprland.lua, task 11.11), then the cursor
        // gsettings keys, then setcursor.
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
                BackingFile::HyprlandLua,
                BackingFile::CursorFallbackTheme,
            ],
            &params,
            &all_capabilities(),
        );
        assert_eq!(
            planned,
            vec![
                ReloadAction::HyprctlReload(HyprlandReloadScope::ConfigOnly),
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
            &[Daemon::Hypridle], // eww and kitty are NOT live
            true,
        );
        let planned = plan_reloads(&[BackingFile::Palette], &ReloadParams::default(), &caps);
        assert_eq!(
            planned,
            vec![ReloadAction::HyprctlReload(HyprlandReloadScope::ConfigOnly)],
            "eww reload and kitty SIGUSR1 are dropped when those daemons are not live"
        );
    }

    #[test]
    fn plan_drops_hyprctl_actions_when_hyprland_is_not_reloadable() {
        // Without a live Hyprland IPC socket, every hyprctl-family action is dropped
        // even though the daemons are live.
        let caps = Capabilities::for_tests(
            &[Binary::Hyprctl, Binary::Gsettings],
            &[Daemon::Eww, Daemon::Kitty, Daemon::Hyprpaper],
            false, // no live IPC socket
        );
        // hyprpaper.conf needs `hyprctl hyprpaper`, so it is dropped.
        assert!(
            plan_reloads(&[BackingFile::HyprpaperConf], &full_params(), &caps).is_empty(),
            "hyprpaper reload needs a reloadable Hyprland"
        );
        // A palette change keeps eww/kitty but drops the hyprctl reload.
        assert_eq!(
            plan_reloads(&[BackingFile::Palette], &ReloadParams::default(), &caps),
            vec![ReloadAction::EwwReload, ReloadAction::KittyColors]
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

        ReloadAction::HyprctlReload(HyprlandReloadScope::Full)
            .execute(&runner, &signaller)
            .expect("hyprctl reload succeeds under the default mock");
        ReloadAction::HyprctlReload(HyprlandReloadScope::ConfigOnly)
            .execute(&runner, &signaller)
            .expect("hyprctl reload config-only succeeds under the default mock");
        ReloadAction::EwwReload
            .execute(&runner, &signaller)
            .expect("eww reload succeeds");
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
                Command::new("hyprctl").arg("reload").arg("config-only"),
                Command::new("eww").arg("reload"),
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

    /// Runs a hyprpaper action for `fit` and returns the recorded commands.
    fn hyprpaper_commands(fit: Option<&str>) -> Vec<Command> {
        let runner = MockCommandRunner::new();
        ReloadAction::HyprpaperWallpaper {
            path: "/home/u/Pictures/wall.jpg".to_string(),
            fit: fit.map(str::to_string),
        }
        .execute(&runner, &MockProcessSignaller::new())
        .expect("hyprpaper reload succeeds");
        runner.recorded()
    }

    #[test]
    fn hyprpaper_action_sets_the_wallpaper_in_one_request_with_a_live_fit() {
        // Task 11.14: exactly one `wallpaper` request — no `preload`, which the
        // installed hyprctl rejects — on all outputs (empty monitor field before the
        // comma), with the fit as the third comma-field for the modes that render
        // differently live.
        for fit in ["tile", "contain"] {
            assert_eq!(
                hyprpaper_commands(Some(fit)),
                vec![Command::new("hyprctl").args([
                    "hyprpaper".to_string(),
                    "wallpaper".to_string(),
                    format!(",/home/u/Pictures/wall.jpg,{fit}"),
                ])],
                "{fit}"
            );
        }
    }

    #[test]
    fn hyprpaper_action_omits_every_fit_that_renders_as_cover_live() {
        // Over IPC only `contain` and `tile` differ from the default `cover`; `cover`
        // itself, `fill` (valid in the file, unknown to the IPC), `stretch` and no fit
        // at all are all sent as the two-field form, which hyprpaper shows as cover.
        for fit in [
            None,
            Some("cover"),
            Some("fill"),
            Some("stretch"),
            Some("odd"),
        ] {
            assert_eq!(
                hyprpaper_commands(fit),
                vec![Command::new("hyprctl").args([
                    "hyprpaper",
                    "wallpaper",
                    ",/home/u/Pictures/wall.jpg"
                ])],
                "{fit:?}"
            );
        }
    }

    #[test]
    fn a_failed_hyprpaper_request_is_a_reload_error_after_one_command() {
        // A failing `wallpaper` request is surfaced (R5.5); nothing else was tried.
        let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake(1))]);
        let result = ReloadAction::HyprpaperWallpaper {
            path: "/x.jpg".to_string(),
            fit: Some("tile".to_string()),
        }
        .execute(&runner, &MockProcessSignaller::new());

        assert!(matches!(result, Err(ReloadError::NonZeroExit { .. })));
        assert_eq!(
            runner.recorded(),
            vec![Command::new("hyprctl").args(["hyprpaper", "wallpaper", ",/x.jpg,tile"])]
        );
    }

    // --- Execution: the task 11.14 follow-ups --------------------------------

    #[test]
    fn the_bar_text_tone_runs_its_script_with_apply() {
        let runner = MockCommandRunner::new();
        bar_text_tone()
            .execute(&runner, &MockProcessSignaller::new())
            .expect("the script succeeds under the default mock");
        assert_eq!(
            runner.recorded(),
            vec![Command::new(BAR_TEXT_TONE).arg("--apply")]
        );

        // A failing script is a non-fatal reload error like any other (R5.5).
        let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake(1))]);
        assert!(matches!(
            bar_text_tone().execute(&runner, &MockProcessSignaller::new()),
            Err(ReloadError::NonZeroExit { .. })
        ));
    }

    #[test]
    fn notifd_reload_is_the_busctl_reload_call() {
        // Analysis §8.2: the same `busctl --user --auto-start=no` form as every other
        // notifd call, so it can never D-Bus-activate a daemon; never systemctl.
        let runner = MockCommandRunner::new();
        ReloadAction::NotifdReload
            .execute(&runner, &MockProcessSignaller::new())
            .expect("the call succeeds under the default mock");
        assert_eq!(
            runner.recorded(),
            vec![Command::new("busctl").args([
                "--user",
                "--auto-start=no",
                "call",
                "se.dotfiles.Notifd",
                "/se/dotfiles/Notifd",
                "se.dotfiles.Notifd",
                "Reload",
            ])]
        );

        let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake(1))]);
        assert!(matches!(
            ReloadAction::NotifdReload.execute(&runner, &MockProcessSignaller::new()),
            Err(ReloadError::NonZeroExit { program, .. }) if program == "busctl"
        ));
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
                Command::new("setsid")
                    .args(["--fork", "hypridle"])
                    .detached(),
            ],
            "the inactive unit is not restarted; hypridle is respawned via a detached \
             setsid --fork (detached so the runner never drains pipes the daemon inherits)"
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
                Command::new("setsid")
                    .args(["--fork", "hypridle"])
                    .detached(),
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

        let error = ReloadAction::HyprctlReload(HyprlandReloadScope::Full)
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

    // --- Live option changes via `hyprctl eval` (task 11.2) ---

    #[test]
    fn eval_renders_a_nested_table_path_as_nested_tables() {
        let eval =
            HyprlandEval::config("input.touchpad.natural_scroll", &LuaLiteral::boolean(true))
                .expect("a valid path renders");
        assert_eq!(
            eval.code(),
            "hl.config({ input = { touchpad = { natural_scroll = true } } })"
        );

        let eval = HyprlandEval::config("input.sensitivity", &LuaLiteral::float(0.3).unwrap())
            .expect("a valid path renders");
        assert_eq!(eval.code(), "hl.config({ input = { sensitivity = 0.3 } })");

        // A single segment is a top-level key.
        let eval = HyprlandEval::config("input", &LuaLiteral::integer(1)).expect("renders");
        assert_eq!(eval.code(), "hl.config({ input = 1 })");
    }

    #[test]
    fn eval_quotes_strings_and_writes_numbers_and_booleans_bare() {
        let layouts = LuaLiteral::string("us,se").expect("allowed");
        assert_eq!(
            HyprlandEval::config("input.kb_layout", &layouts)
                .unwrap()
                .code(),
            r#"hl.config({ input = { kb_layout = "us,se" } })"#
        );
        assert_eq!(
            HyprlandEval::config("input.follow_mouse", &LuaLiteral::integer(1))
                .unwrap()
                .code(),
            "hl.config({ input = { follow_mouse = 1 } })"
        );
        assert_eq!(
            HyprlandEval::config("input.touchpad.tap_to_click", &LuaLiteral::boolean(false))
                .unwrap()
                .code(),
            "hl.config({ input = { touchpad = { tap_to_click = false } } })"
        );
    }

    #[test]
    fn eval_refuses_forbidden_string_characters_before_rendering() {
        // The forbidden-character rule lives in `parsers::lua_literal` and is shared
        // with the Lua-literal file editor (task 11.3): such a value can never reach
        // a `hyprctl eval` call, because the literal cannot be built at all.
        for value in ["us\"", "us\\se", "us\nse"] {
            assert!(
                matches!(
                    LuaLiteral::string(value),
                    Err(LuaLiteralError::ForbiddenCharacter { .. })
                ),
                "{value:?} must be refused"
            );
        }
    }

    #[test]
    fn eval_refuses_keys_that_are_not_bare_lua_names() {
        let value = LuaLiteral::boolean(true);
        for path in [
            "",
            "input..natural_scroll",
            "input.touchpad.tap-to-click",
            "input.end",
            "input = {} }) os.exit() --",
        ] {
            assert!(
                matches!(
                    HyprlandEval::config(path, &value),
                    Err(LuaLiteralError::InvalidKey(_))
                ),
                "{path:?} must be refused as a table path"
            );
        }
    }

    #[test]
    fn eval_runs_as_a_single_argument_without_a_shell() {
        let eval =
            HyprlandEval::config("input.kb_layout", &LuaLiteral::string("us,se").unwrap()).unwrap();
        let runner =
            MockCommandRunner::with_outcomes([Ok(CommandOutput::fake_with_streams(0, "ok\n", ""))]);
        eval.run(&runner).expect("an `ok` reply is success");
        assert_eq!(
            runner.recorded(),
            vec![
                Command::new("hyprctl")
                    .args(["eval", r#"hl.config({ input = { kb_layout = "us,se" } })"#,])
            ],
            "the Lua source is one argv entry, never split or shell-quoted"
        );
    }

    #[test]
    fn eval_treats_anything_but_an_ok_reply_as_failure() {
        let eval =
            HyprlandEval::config("input.sensitivity", &LuaLiteral::float(0.3).unwrap()).unwrap();

        // Exit 0 but not `ok` — how `hyprctl` refuses some requests (the Lua-session
        // `keyword` rejection is printed with exit status 0).
        let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake_with_streams(
            0,
            "keyword can't work with non-legacy parsers. Use eval.\n",
            "",
        ))]);
        match eval.run(&runner) {
            Err(ReloadError::EvalRejected { code, reply }) => {
                assert_eq!(code, Some(0));
                assert_eq!(
                    reply,
                    "keyword can't work with non-legacy parsers. Use eval."
                );
            }
            other => panic!("a non-`ok` reply must be EvalRejected, got {other:?}"),
        }

        // A Lua error: non-zero exit with the diagnostic on stdout.
        let runner = MockCommandRunner::with_outcomes([Ok(CommandOutput::fake_with_streams(
            7,
            "error: unknown config key 'input.foo'\n",
            "",
        ))]);
        let error = eval.run(&runner).expect_err("a Lua error is a failure");
        assert!(matches!(
            error,
            ReloadError::EvalRejected { code: Some(7), .. }
        ));
        assert!(error.to_string().contains("unknown config key"));

        // Exit 0 with empty stdout is not success either.
        let runner = MockCommandRunner::new();
        let error = eval.run(&runner).expect_err("an empty reply is not `ok`");
        assert!(matches!(error, ReloadError::EvalRejected { ref reply, .. } if reply.is_empty()));
        assert!(!error.to_string().is_empty());

        // `ok` with a failing exit status is still a failure.
        let runner =
            MockCommandRunner::with_outcomes([Ok(CommandOutput::fake_with_streams(1, "ok", ""))]);
        assert!(eval.run(&runner).is_err());

        // A command that could not be spawned at all.
        let runner = MockCommandRunner::with_outcomes([Err(CommandError::Spawn(io::Error::from(
            io::ErrorKind::NotFound,
        )))]);
        assert!(matches!(eval.run(&runner), Err(ReloadError::Command(_))));
    }

    #[test]
    fn eval_reply_excerpt_is_bounded_and_trimmed() {
        let long = "x".repeat(STDERR_EXCERPT_LIMIT * 2);
        assert_eq!(
            eval_reply_excerpt(long.as_bytes()),
            format!("{}…", "x".repeat(STDERR_EXCERPT_LIMIT))
        );
        assert_eq!(eval_reply_excerpt(b"  error: bad\n"), "error: bad");
    }
}
