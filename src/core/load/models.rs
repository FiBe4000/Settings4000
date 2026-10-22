//! The bespoke-model half of the startup load (tasks 6.1, 6.3–6.6, 6.8, 11.4, 11.15;
//! R4.2, R4.4, R8.5): building each page model or write helper the shared store does not
//! cover, or deciding — from the detected [`Capabilities`] — that its page, section, or
//! control is hidden, logged at `info` with the reason.
//!
//! Every builder runs on the startup worker, so its filesystem scans and command probes
//! stay off the main thread and inside the R8.1 cold-start budget (architecture §8). See
//! the parent module for how this half fits into the whole load.

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

/// The XDG roots scanned for installed GTK, icon, and cursor themes (R3.3/R3.4).
///
/// GTK themes live under `~/.themes`, `$XDG_DATA_HOME/themes`, and
/// `/usr/share/themes`; icon and cursor themes under `~/.icons`,
/// `$XDG_DATA_HOME/icons`, and `/usr/share/icons`. `$XDG_DATA_HOME` falls back to
/// `~/.local/share`. A root that does not exist is simply skipped by the scan, so a
/// missing `~/.themes` is harmless.
pub fn theme_roots() -> ThemeRoots {
    let data_home = data_home();
    let home = home_dir();
    let mut gtk_theme_dirs = Vec::new();
    let mut icon_dirs = Vec::new();
    if let Some(home) = &home {
        gtk_theme_dirs.push(home.join(".themes"));
        icon_dirs.push(home.join(".icons"));
    }
    gtk_theme_dirs.push(data_home.join("themes"));
    gtk_theme_dirs.push(PathBuf::from("/usr/share/themes"));
    icon_dirs.push(data_home.join("icons"));
    icon_dirs.push(PathBuf::from("/usr/share/icons"));
    ThemeRoots {
        gtk_theme_dirs,
        icon_dirs,
    }
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

/// The XDG data base directory: `$XDG_DATA_HOME`, else `$HOME/.local/share`, else a
/// bare `.local/share` relative path (which simply misses on scan).
fn data_home() -> PathBuf {
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
        if !data_home.is_empty() {
            return PathBuf::from(data_home);
        }
    }
    match home_dir() {
        Some(home) => home.join(".local").join("share"),
        None => PathBuf::from(".local").join("share"),
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
}
