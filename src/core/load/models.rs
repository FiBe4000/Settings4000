//! The bespoke-model half of the startup load (tasks 6.1, 6.3–6.6, 6.8, 11.4, 11.15;
//! R4.2, R4.4, R8.5): building each page model or write helper the shared store does not
//! cover, or deciding — from the detected [`Capabilities`] — that its page, section, or
//! control is hidden, logged at `info` with the reason.
//!
//! Every builder runs on the startup worker, so its filesystem scans and command probes
//! stay off the main thread and inside the R8.1 cold-start budget (architecture §8). See
//! the parent module for how this half fits into the whole load.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use super::{config_home, input_lua_path};
use crate::core::detect::{Binary, Capabilities, Daemon, HyprConfigProvider};
use crate::core::display::DisplayModel;
use crate::core::home_path::home_dir;
use crate::core::input::{InputModel, default_xkb_registry};
use crate::core::notifd_config::NotifdConfigModel;
use crate::core::power::PowerModel;
use crate::core::theme::{
    PaletteModel, ThemeRoots, ThemesModel, ThemesPaths, WallpaperModel, WallpaperPaths,
};
use crate::system::command::SystemCommandRunner;

/// Builds the Power & Idle page's helper on the worker thread (task 6.8; R4.2).
///
/// Only builds when hypridle is present — the Power & Idle category edits `hypridle.conf`
/// and restarts hypridle on Apply (task 5.1's category gate), so without hypridle there is
/// nothing to configure and the category is hidden entirely (R4.2). It records the live
/// `hypridle.conf` path (R8.5) for the Apply-time timeout/lock-command write. Building it
/// here, off the main thread, keeps startup inside the R8.1 budget (architecture §8).
pub fn load_power(capabilities: &Capabilities) -> Option<PowerModel> {
    if !capabilities.has_binary(Binary::Hypridle) {
        tracing::info!("hypridle not found; the Power & Idle page is hidden (R4.2)");
        return None;
    }
    let hypridle_conf = config_home().join("hypr").join("hypridle.conf");
    Some(PowerModel::load(hypridle_conf))
}

/// Builds the Notifications page's helper for its staged notifd settings, or decides
/// that only the runtime controls are shown (task 11.15; R4.2, R4.4).
///
/// Both conditions are required, and a failing one is logged at `info` with its reason
/// (never the file's contents, R7.3):
///
/// - notifd runs at version 1.1 or later — older daemons have neither the file nor
///   `Reload()`, and a stopped one could not apply a change (analysis §8.2);
/// - the file at `notifd_config` is readable, UTF-8, and free of characters notifd
///   would split or strip differently from the app (see [`crate::core::notifd_config`]).
///
/// The Notifications page itself is gated separately (notifd installed or running,
/// task 11.7), so `None` here leaves the runtime controls alone on the page. Split from
/// [`load`](super::load) so the decision is testable against a fixture file.
pub fn load_notifd_config_from(
    capabilities: &Capabilities,
    notifd_config: PathBuf,
) -> Option<NotifdConfigModel> {
    match NotifdConfigModel::offer(capabilities.notifd(), notifd_config.clone()) {
        Ok(model) => Some(model),
        Err(reason) => {
            tracing::info!(
                path = %notifd_config.display(),
                %reason,
                "notifd's settings file cannot back the Notifications page's staged rows; \
                 only the runtime controls are shown (R4.2)"
            );
            None
        }
    }
}

/// Builds the Input page's helper on the worker thread for the real system (tasks
/// 6.6, 11.2, 11.4; R4.2) — [`load_input_from`] with the live `input.lua` path and
/// the XKB registry resolved by [`default_xkb_registry`].
pub fn load_input(capabilities: &Capabilities) -> Option<InputModel> {
    load_input_from(
        capabilities,
        input_lua_path(&config_home()),
        &default_xkb_registry(),
    )
}

/// Builds the Input page's helper, or decides that the page is hidden (tasks 6.6,
/// 11.2, 11.4; R4.2).
///
/// Three conditions, each logged at `info` when it hides the page:
///
/// - `hyprctl` is present — an Input change applies via `hyprctl reload config-only`,
///   so without it there is nothing to apply with (task 5.1);
/// - the compositor runs its Lua config provider — the app's input settings exist
///   only in `input.lua`, so on a hyprlang host the page is hidden like a missing app
///   rather than writing a file the compositor ignores (analysis §8.6);
/// - `input.lua` exists and the Lua-literal editor understands every line of it
///   ([`check_input_lua`](crate::core::input::check_input_lua)). A computed value or
///   any other code in the file might override the literals the page would show, so
///   the page hides instead of guessing (architecture §10). The logged reason names
///   line numbers only, never file contents (R7.3).
///
/// When all hold it reads the XKB layout registry at `xkb_registry` for the
/// keyboard-layout add-list; a missing/unparseable registry degrades to an empty list
/// (R4.4). Reading the (large) registry here, on the startup worker, keeps it off the
/// main thread and inside the R8.1 cold-start budget (architecture §8). The
/// `input.lua` settings themselves are staged in the store (loaded by
/// [`load_input_lua`](super::load_input_lua)); this helper only supplies the
/// candidates and the Apply-time write. Split from [`load_input`] so the decision is
/// testable against a fixture file instead of the real `~/.config`.
pub fn load_input_from(
    capabilities: &Capabilities,
    input_lua: PathBuf,
    xkb_registry: &Path,
) -> Option<InputModel> {
    if !capabilities.has_binary(Binary::Hyprctl) {
        tracing::info!("hyprctl not found; the Input page is hidden (R4.2)");
        return None;
    }
    if capabilities.hypr_config_provider() != HyprConfigProvider::Lua {
        tracing::info!(
            provider = %capabilities.hypr_config_provider(),
            "Hyprland does not run its Lua config provider; the Input page is hidden \
             (analysis §8.6, R4.2)"
        );
        return None;
    }
    match InputModel::load(input_lua.clone(), xkb_registry) {
        Ok(model) => Some(model),
        Err(reason) => {
            tracing::info!(
                path = %input_lua.display(),
                %reason,
                "input.lua cannot back the Input page; the page is hidden (architecture \
                 §10, R4.2)"
            );
            None
        }
    }
}

/// Builds the Theme page's wallpaper / lock-background model on the worker thread (task
/// 6.5; R4.2, R4.4).
///
/// Only builds when hyprpaper is present — the wallpaper is applied live with
/// `hyprctl hyprpaper`, so without hyprpaper the wallpaper section is hidden and logged
/// at `info` (R4.2). Whether the lock-screen override is offered is gated separately on
/// hyprlock (analysis §6.2): its absence hides only that one control, not the whole
/// section. Reading the two config files here, on the startup worker, keeps them off the
/// main thread and inside the R8.1 cold-start budget (architecture §8).
pub fn load_wallpaper(capabilities: &Capabilities) -> Option<WallpaperModel> {
    if !capabilities.has_binary(Binary::Hyprpaper) {
        tracing::info!("hyprpaper not found; the wallpaper controls are hidden (R4.2)");
        return None;
    }
    let lock_available = capabilities.has_binary(Binary::Hyprlock);
    if !lock_available {
        tracing::info!(
            "hyprlock not found; the lock-screen override control is hidden (R4.2/R4.4)"
        );
    }
    Some(WallpaperModel::load(wallpaper_paths(), lock_available))
}

/// The live XDG paths of the two config files a wallpaper / lock-background change
/// writes (R8.5), and the home directory their `~/…` image paths expand against (task
/// 11.10).
fn wallpaper_paths() -> WallpaperPaths {
    let config = config_home();
    WallpaperPaths {
        hyprpaper_conf: config.join("hypr").join("hyprpaper.conf"),
        hyprlock_conf: config.join("hypr").join("hyprlock.conf"),
        home: home_dir(),
    }
}

/// Builds the Theme page's palette-scheme model on the worker thread (task 6.3).
///
/// Only builds when detection found the dotfiles palette source (R3.2/R8.5): its
/// [`PaletteSource`](crate::core::detect::PaletteSource) supplies the `colors/`
/// directory to scan and the `scripts/generate-colors` path the Apply pipeline
/// runs. When it is absent — no dotfiles repo behind the config, or an incomplete one —
/// this returns `None` and the Theme palette section is hidden (R4.2/R4.4). The active
/// scheme is read from the deployed generated `~/.config/hypr/colors.conf` header (task
/// 3.7, R3.2), at its live XDG path. Running the scan here keeps its file reads
/// off the main thread and inside the R8.1 cold-start budget (architecture §8).
pub fn load_palette(capabilities: &Capabilities) -> Option<PaletteModel> {
    let source = capabilities.palette_source()?;
    let active_scheme_source = config_home().join("hypr").join("colors.conf");
    Some(PaletteModel::load(
        source.colors_dir(),
        &active_scheme_source,
        source.generate_colors().to_path_buf(),
    ))
}

/// Builds the Theme page's GTK/icon/cursor theme model on the worker thread (task
/// 6.4; R3.3, R3.4, R4.2, R2.2).
///
/// Only builds when `gsettings` is present — the whole GTK/icon/cursor feature applies
/// its changes with `gsettings set`, so without it the controls are hidden and logged
/// at `info` (R4.2). When present, it scans for installed themes under the XDG theme
/// roots (`~/.themes`, the data dirs, `/usr/share/...`), reads the backing config
/// (both `settings.ini`, `hyprland.lua`, `uwsm/env`, `~/.icons/default/index.theme`),
/// gates the live-restyle claim on
/// the settings portal (R2.2), and passes the app's own `GTK_THEME` environment value
/// so an override disables the GTK-theme drop-down (R3.3). It also hands the model the
/// detected dotfiles repo root and whether `xsettingsd` runs, for the Theme page's
/// two "another program may rewrite GTK settings" notes (task 11.12), and the notifd
/// status, for the note that a notifd too old to reload shows a new icon theme only
/// after its next start (task 11.14). Running the
/// scans here, on the startup worker, keeps its filesystem scans off the main
/// thread and inside the R8.1 cold-start budget (architecture §8).
pub fn load_themes(capabilities: &Capabilities) -> Option<ThemesModel> {
    if !capabilities.has_binary(Binary::Gsettings) {
        tracing::info!("gsettings not found; the GTK/icon/cursor theme controls are hidden (R4.2)");
        return None;
    }
    Some(
        ThemesModel::load(
            &theme_roots(),
            themes_paths(capabilities),
            capabilities.settings_portal_available(),
            std::env::var("GTK_THEME").ok(),
        )
        .with_xsettingsd_running(capabilities.is_daemon_live(Daemon::Xsettingsd))
        .with_notifd_status(capabilities.notifd()),
    )
}

/// The XDG roots scanned for installed GTK, icon, and cursor themes (R3.3/R3.4), read
/// from this process's environment.
///
/// A thin wrapper: [`ThemeRootsEnv::from_process`] reads the variables and
/// [`theme_roots_from`] — a pure function, so tests pin the order without touching the
/// process environment — builds the list. See there for the order and why it matters.
pub fn theme_roots() -> ThemeRoots {
    theme_roots_from(&ThemeRootsEnv::from_process())
}

/// The environment values the theme-root lookup depends on (task 10.6).
///
/// Collected in one place so [`theme_roots_from`] stays a pure function of its input:
/// production fills it from the process environment ([`Self::from_process`]), tests
/// build it by hand. Every field is the raw variable value; empty and relative values
/// are dealt with by [`theme_roots_from`], not here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThemeRootsEnv {
    /// The home directory (`$HOME`, see [`home_dir`]), or `None` when unknown — then
    /// the home-based roots (`~/.themes`, `~/.icons`, the default data home) are left
    /// out rather than resolved against the working directory.
    pub home: Option<PathBuf>,
    /// `$XDG_DATA_HOME` as set, or `None` when unset.
    pub xdg_data_home: Option<OsString>,
    /// `$XDG_DATA_DIRS` as set (a `:`-separated list), or `None` when unset.
    pub xdg_data_dirs: Option<OsString>,
    /// `$GTK_DATA_PREFIX` as set, or `None` when unset — GTK's override of the prefix
    /// its built-in theme directory lives under.
    pub gtk_data_prefix: Option<OsString>,
}

impl ThemeRootsEnv {
    /// Reads the four values from this process's environment.
    ///
    /// The app inherits the session environment that GTK apps started from the same
    /// session also see, so the roots it computes are the ones those apps search.
    pub fn from_process() -> Self {
        ThemeRootsEnv {
            home: home_dir(),
            xdg_data_home: std::env::var_os("XDG_DATA_HOME"),
            xdg_data_dirs: std::env::var_os("XDG_DATA_DIRS"),
            gtk_data_prefix: std::env::var_os("GTK_DATA_PREFIX"),
        }
    }
}

/// `$XDG_DATA_DIRS` when it is unset or empty — the default of the XDG Base Directory
/// Specification, and the literal GLib's `g_build_system_data_dirs` falls back to.
const DEFAULT_XDG_DATA_DIRS: &str = "/usr/local/share/:/usr/share/";

/// The prefix of GTK's built-in theme directory (`<prefix>/share/themes`) when
/// `$GTK_DATA_PREFIX` is unset: the install prefix GTK was compiled with, which is
/// `/usr` for a distribution-packaged GTK. The app cannot ask the library for it, so
/// this is the one assumed value in the lookup; with the default `$XDG_DATA_DIRS` the
/// same directory is already searched through `/usr/share`.
const GTK_DEFAULT_DATA_PREFIX: &str = "/usr";

/// Builds the theme roots from the environment values `env` (R3.3/R3.4, task 10.6).
///
/// **GTK theme roots** are listed in exactly the order GTK 4 searches for a theme
/// name, because the order is load-bearing: the GTK 4 theme-link step
/// (`resolve_gtk4_theme` in `core::theme`, task 11.16) links the first root that has
/// the theme, so for a theme installed in two places the app must pick the copy GTK
/// itself would load, or `~/.config/gtk-4.0` would point GTK 4 apps at a different
/// copy than the one GTK 3 apps load for the same `gtk-theme-name`. The order, from
/// `_gtk_css_find_theme` in GTK 4.24.1's `gtk/gtkcssprovider.c` (also stated in that
/// file's `GtkCssProvider` class documentation; GTK 3.24 searches the same way), is:
///
/// 1. `$XDG_DATA_HOME/themes` — `g_get_user_data_dir()`, which is `$XDG_DATA_HOME`
///    when set and non-empty, else `$HOME/.local/share`;
/// 2. `$HOME/.themes` — still searched, though GTK 4 logs it as deprecated;
/// 3. `<dir>/themes` for each `$XDG_DATA_DIRS` entry in order —
///    `g_get_system_data_dirs()`, which falls back to `/usr/local/share/:/usr/share/`
///    when the variable is unset or empty;
/// 4. GTK's built-in theme directory `$GTK_DATA_PREFIX/share/themes`, or
///    `<compiled prefix>/share/themes` when unset — taken to be `/usr`, the
///    prefix of a distribution-packaged GTK.
///
/// (The GLib side was read in GLib 2.90.1's `glib/gutils.c`; both sources match the
/// libraries installed on the target system, GTK 4.24.1 and GLib 2.90.1.)
///
/// Four deliberate departures, none of which changes the order of real roots:
///
/// - An empty or relative `$XDG_DATA_HOME`, and empty or relative `$XDG_DATA_DIRS`
///   entries (`/usr/share::share`), are skipped, as the XDG Base Directory
///   Specification asks ("a relative path … should be considered invalid and
///   ignored"). GLib keeps them and GTK then probes paths relative to each app's own
///   working directory — nothing a settings app can reproduce. An `$XDG_DATA_HOME`
///   skipped this way falls back to `$HOME/.local/share`, like an unset one.
/// - A root equal to an earlier one is listed once: with the default
///   `$XDG_DATA_DIRS`, the built-in `/usr/share/themes` is already step 3's last root,
///   and a later duplicate could never win.
/// - Without a known home directory (unset, empty or relative `$HOME`) the
///   home-based roots are left out instead of being resolved against the working
///   directory (GLib would ask the password database).
/// - An empty or relative `$GTK_DATA_PREFIX` counts as unset, so the built-in root
///   stays `/usr/share/themes`. GTK's `_gtk_get_theme_dir` checks only whether the
///   variable is set, so for an empty value it would probe the relative
///   `share/themes` against each app's working directory — the same unreproducible
///   case as a relative XDG value above.
///
/// Only the GTK 4 `gtk-4.0/` subdirectory decides which root wins in the link step;
/// GTK additionally probes version-suffixed `gtk-4.<N>/` directories in each root,
/// which the app deliberately does not (task 9.30, recorded under task 10.6).
///
/// **Icon and cursor theme roots** stay `~/.icons`, `$XDG_DATA_HOME/icons` and
/// `/usr/share/icons` (R3.4). They feed only the name scan, which merges names from
/// all roots, so their order picks nothing.
///
/// A root that does not exist is simply skipped by the scan, so a missing `~/.themes`
/// is harmless.
pub fn theme_roots_from(env: &ThemeRootsEnv) -> ThemeRoots {
    let home = env.home.as_deref().filter(|home| home.is_absolute());
    let data_home = absolute_dir(env.xdg_data_home.as_deref())
        .or_else(|| home.map(|home| home.join(".local").join("share")));

    let mut gtk_theme_dirs = Vec::new();
    gtk_theme_dirs.extend(data_home.as_ref().map(|data_home| data_home.join("themes")));
    gtk_theme_dirs.extend(home.map(|home| home.join(".themes")));
    let data_dirs = env
        .xdg_data_dirs
        .as_deref()
        .filter(|dirs| !dirs.is_empty())
        .unwrap_or(OsStr::new(DEFAULT_XDG_DATA_DIRS));
    gtk_theme_dirs.extend(
        std::env::split_paths(data_dirs)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join("themes")),
    );
    let gtk_prefix = absolute_dir(env.gtk_data_prefix.as_deref())
        .unwrap_or_else(|| PathBuf::from(GTK_DEFAULT_DATA_PREFIX));
    gtk_theme_dirs.push(gtk_prefix.join("share").join("themes"));
    let gtk_theme_dirs = without_repeats(gtk_theme_dirs);

    let mut icon_dirs = Vec::new();
    icon_dirs.extend(home.map(|home| home.join(".icons")));
    icon_dirs.extend(data_home.map(|data_home| data_home.join("icons")));
    icon_dirs.push(PathBuf::from("/usr/share/icons"));
    ThemeRoots {
        gtk_theme_dirs,
        icon_dirs,
    }
}

/// `value` as a directory path when it is set, non-empty and absolute — the XDG Base
/// Directory Specification's rule for a usable variable value — else `None`.
fn absolute_dir(value: Option<&OsStr>) -> Option<PathBuf> {
    value.map(PathBuf::from).filter(|path| path.is_absolute())
}

/// `paths` with every path that equals an earlier one removed, order kept.
///
/// `Path` equality compares components, so `/usr/share/themes` built from
/// `/usr/share/` and from `/usr` count as equal; no filesystem access is made.
fn without_repeats(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut kept: Vec<PathBuf> = Vec::with_capacity(paths.len());
    for path in paths {
        if !kept.contains(&path) {
            kept.push(path);
        }
    }
    kept
}

/// The live paths of the files a theme/cursor change writes (R8.5).
///
/// The Hyprland-side cursor copy — the two `hl.env` lines of `hyprland.lua` — is
/// included only when the compositor runs its Lua config provider (task 11.2,
/// analysis §8.6): the app keeps no hyprlang cursor path, so on any other host that
/// write is hidden (logged at `info`) while the `settings.ini`, `uwsm/env`,
/// `index.theme`, `gsettings` and `hyprctl setcursor` legs stay.
///
/// The XCursor fallback theme is the one cursor copy outside the XDG config directory:
/// `$HOME/.icons/default/index.theme`, the path libXcursor and `setup.sh` use (analysis
/// §8.4; R8.2's "equivalent per-app paths"). It is left out when `$HOME` is unset,
/// because the cursor write may *create* that file and must never do so at a path
/// relative to the working directory.
pub fn themes_paths(capabilities: &Capabilities) -> ThemesPaths {
    let config = config_home();
    let hyprland_lua = if capabilities.hypr_config_provider() == HyprConfigProvider::Lua {
        Some(config.join("hypr").join("hyprland.lua"))
    } else {
        tracing::info!(
            provider = %capabilities.hypr_config_provider(),
            "Hyprland does not run its Lua config provider; the Hyprland-side cursor \
             write is hidden (analysis §8.6, R4.2)"
        );
        None
    };
    let cursor_fallback_theme =
        home_dir().map(|home| home.join(".icons").join("default").join("index.theme"));
    if cursor_fallback_theme.is_none() {
        tracing::info!(
            "the home directory is unknown; the XCursor fallback theme copy of the cursor \
             is not written (R3.4)"
        );
    }
    ThemesPaths {
        gtk3_settings: config.join("gtk-3.0").join("settings.ini"),
        gtk4_settings: config.join("gtk-4.0").join("settings.ini"),
        hyprland_lua,
        uwsm_env: config.join("uwsm").join("env"),
        cursor_fallback_theme,
        dotfiles_repo: capabilities.dotfiles_repo_root().map(Path::to_path_buf),
        home: home_dir(),
    }
}

/// Builds the Display page's model on the worker thread (task 6.1).
///
/// Only probes when Hyprland is reloadable (`hyprctl` on `$PATH` plus a live IPC
/// socket) — otherwise there is no compositor to query, so it returns `None` and
/// the Display page shows a placeholder (R4.2). Running the `hyprctl monitors -j`
/// probe here, on the startup worker, keeps it off the main thread and inside the
/// R8.1 cold-start budget (architecture §8). The merge itself is the GTK-free
/// [`DisplayModel::load`]; a failed probe or an unreadable `monitors.conf` degrades
/// gracefully there (R4.4). The detected laptop-display toggle script (task 11.8) is
/// handed over so the model offers the laptop toggle only when it exists (R4.2).
pub fn load_display(capabilities: &Capabilities) -> Option<DisplayModel> {
    if !capabilities.hyprland_reloadable() {
        tracing::info!(
            "Hyprland is not reloadable (no hyprctl, or no live IPC socket); the Display page \
             has no monitor data — it is hidden entirely when hyprctl is absent, or shows a \
             placeholder when hyprctl is present but the compositor is not running (R4.2)"
        );
        return None;
    }
    let monitors_conf = config_home().join("hypr").join("monitors.conf");
    DisplayModel::load(
        &SystemCommandRunner::new(),
        monitors_conf,
        capabilities.laptop_toggle_script().map(Path::to_path_buf),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn themes_are_hidden_when_gsettings_is_absent() {
        // R4.2 (task 6.4): the GTK/icon/cursor feature applies its changes with
        // `gsettings set`, so without the binary the model is not built and the
        // appearance section is hidden. The gate is checked before any filesystem
        // scan, so this needs no fixture.
        let caps = Capabilities::for_tests(&[], &[], false);
        assert!(
            load_themes(&caps).is_none(),
            "no gsettings -> no themes model -> appearance section hidden"
        );
    }

    #[test]
    fn power_is_hidden_when_hypridle_is_absent() {
        // R4.2 (task 6.8): the Power & Idle page edits hypridle.conf and restarts hypridle,
        // so without the hypridle binary no model is built and the whole category is hidden
        // (task 5.1's category gate). The gate is checked before any path is resolved, so
        // this needs no fixture.
        let caps = Capabilities::for_tests(&[], &[], false);
        assert!(
            load_power(&caps).is_none(),
            "no hypridle -> no Power & Idle model -> the category is hidden"
        );

        // Present -> a model is built (against the resolved hypridle.conf path).
        let caps = Capabilities::for_tests(&[Binary::Hypridle], &[], false);
        assert!(
            load_power(&caps).is_some(),
            "hypridle present -> a Power & Idle model is built"
        );
    }

    #[test]
    fn wallpaper_is_hidden_when_hyprpaper_is_absent() {
        // R4.2 (task 6.5): the wallpaper is applied live with `hyprctl hyprpaper`, so
        // without the hyprpaper binary the model is not built and the wallpaper section
        // is hidden. The gate is checked before any filesystem read, so this needs no
        // fixture.
        let caps = Capabilities::for_tests(&[], &[], false);
        assert!(
            load_wallpaper(&caps).is_none(),
            "no hyprpaper -> no wallpaper model -> wallpaper section hidden"
        );
    }

    // --- Task 10.6: the theme roots in GTK's lookup order -----------------------------

    /// A [`ThemeRootsEnv`] with home `/home/u` and the given variable values.
    fn roots_env(
        xdg_data_home: Option<&str>,
        xdg_data_dirs: Option<&str>,
        gtk_data_prefix: Option<&str>,
    ) -> ThemeRootsEnv {
        ThemeRootsEnv {
            home: Some(PathBuf::from("/home/u")),
            xdg_data_home: xdg_data_home.map(OsString::from),
            xdg_data_dirs: xdg_data_dirs.map(OsString::from),
            gtk_data_prefix: gtk_data_prefix.map(OsString::from),
        }
    }

    /// The GTK theme roots for `env`, as strings for readable assertions.
    fn gtk_roots(env: &ThemeRootsEnv) -> Vec<String> {
        theme_roots_from(env)
            .gtk_theme_dirs
            .iter()
            .map(|root| root.display().to_string())
            .collect()
    }

    #[test]
    fn gtk_theme_roots_follow_gtks_lookup_order_with_the_defaults() {
        // GTK 4's `_gtk_css_find_theme` (gtkcssprovider.c): $XDG_DATA_HOME/themes
        // (default ~/.local/share), then ~/.themes, then each $XDG_DATA_DIRS entry
        // (default /usr/local/share/:/usr/share/), then <prefix>/share/themes — here the
        // same /usr/share/themes as the last data dir, so it is listed once.
        let expected = [
            "/home/u/.local/share/themes",
            "/home/u/.themes",
            "/usr/local/share/themes",
            "/usr/share/themes",
        ];
        assert_eq!(gtk_roots(&roots_env(None, None, None)), expected);
        // Set-but-empty variables count as unset: as GLib does for the two XDG
        // variables; for GTK_DATA_PREFIX it is a deliberate departure (GTK would probe
        // a relative `share/themes`).
        assert_eq!(
            gtk_roots(&roots_env(Some(""), Some(""), Some(""))),
            expected
        );
    }

    #[test]
    fn gtk_theme_roots_follow_a_custom_data_home_and_data_dirs_in_order() {
        // A custom $XDG_DATA_HOME comes first and replaces ~/.local/share; the data
        // dirs keep their own order (a flatpak-style export before /usr/share); empty
        // and relative entries are skipped (XDG Base Directory Specification); the
        // trailing-slash /usr/share/ repeat and the built-in /usr/share/themes are not
        // listed twice; GTK_DATA_PREFIX moves the built-in root to the end of the list.
        let env = roots_env(
            Some("/data/home"),
            Some(
                "/var/lib/flatpak/exports/share::relative/share:/usr/share:/opt/x/share/:/usr/share/",
            ),
            Some("/opt/gtk"),
        );
        assert_eq!(
            gtk_roots(&env),
            [
                "/data/home/themes",
                "/home/u/.themes",
                "/var/lib/flatpak/exports/share/themes",
                "/usr/share/themes",
                "/opt/x/share/themes",
                "/opt/gtk/share/themes",
            ]
        );
        // Without GTK_DATA_PREFIX the built-in root is /usr/share/themes, already listed.
        let env = roots_env(Some("/data/home"), Some("/opt/x/share"), None);
        assert_eq!(
            gtk_roots(&env),
            [
                "/data/home/themes",
                "/home/u/.themes",
                "/opt/x/share/themes",
                "/usr/share/themes",
            ]
        );
    }

    #[test]
    fn invalid_or_missing_home_values_never_yield_relative_roots() {
        // A relative $XDG_DATA_HOME is invalid per the XDG spec and falls back to the
        // home default; without a usable home the home-based roots are left out, and
        // the system roots remain. No root is ever relative to the working directory.
        assert_eq!(
            gtk_roots(&roots_env(Some("rel/data"), None, None))[0],
            "/home/u/.local/share/themes"
        );
        for home in [None, Some(PathBuf::from("relative/home"))] {
            let homeless = ThemeRootsEnv {
                home,
                ..ThemeRootsEnv::default()
            };
            let roots = theme_roots_from(&homeless);
            assert_eq!(
                gtk_roots(&homeless),
                ["/usr/local/share/themes", "/usr/share/themes"]
            );
            assert_eq!(roots.icon_dirs, [PathBuf::from("/usr/share/icons")]);
            let with_data_home = ThemeRootsEnv {
                xdg_data_home: Some(OsString::from("/data")),
                ..homeless
            };
            assert_eq!(gtk_roots(&with_data_home)[0], "/data/themes");
            let data_home_roots = theme_roots_from(&with_data_home);
            assert!(
                data_home_roots
                    .gtk_theme_dirs
                    .iter()
                    .chain(&data_home_roots.icon_dirs)
                    .all(|root| root.is_absolute())
            );
        }
    }

    #[test]
    fn icon_roots_are_unchanged_by_the_gtk_order() {
        // R3.4: the icon/cursor scan merges names from every root, so its list stays
        // ~/.icons, $XDG_DATA_HOME/icons, /usr/share/icons.
        let roots = theme_roots_from(&roots_env(Some("/data/home"), Some("/opt/x/share"), None));
        assert_eq!(
            roots.icon_dirs,
            [
                PathBuf::from("/home/u/.icons"),
                PathBuf::from("/data/home/icons"),
                PathBuf::from("/usr/share/icons"),
            ]
        );
    }
}
