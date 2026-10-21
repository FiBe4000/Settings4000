//! The startup load: detection plus parsing every backing config file, run on a
//! worker thread concurrently with window construction (task 5.4; architecture §8;
//! R4.3, R4.4, R7.3, R6.2).
//!
//! # What this module is
//!
//! Cold start must stay under the R8.1 budget, so the app builds the window shell
//! immediately on the main thread while the slower work — installed-app detection
//! (task 4.3) and reading/parsing the backing config files (§3 parsers) — happens
//! off-thread (architecture §8). This module is that off-thread work: [`load`]
//! gathers a [`StartupLoad`] (the detected [`Capabilities`] plus one [`LoadedFile`]
//! per backing config it could read), which the window then applies on the main
//! thread to populate the store and build the pages (see [`super::window`]).
//!
//! # Why the load logic is kept GTK-free
//!
//! Everything here is pure data-in/data-out over the filesystem and the §3 parsers —
//! it touches no GTK type — so it is unit-tested headlessly (R6.2, see the tests
//! below) even though it lives under `ui/`. The window is what turns a
//! [`StartupLoad`] into widgets, on the main thread; this module never does.
//!
//! # Everything degrades to "absent"/"skipped" (R4.3, R4.4)
//!
//! A missing binary or daemon degrades inside [`Capabilities::detect`]; here, a
//! backing config that is missing, unreadable, or unparseable simply produces no
//! [`LoadedFile`] (logged at `warn`, never its contents — R7.3) and is skipped. One
//! bad file never aborts the others, and nothing here can fail startup: the window
//! still comes up with whatever loaded. A file that is present but simply lacks a
//! given key contributes only the settings it does hold.
//!
//! # Refresh re-runs the same pass (R4.3)
//!
//! The manual Refresh button re-runs [`load`] on the worker too — detection *and* the
//! backing files *and* the bespoke page models must all be rebuilt together, or the
//! refreshed capabilities and the rendered content diverge (e.g. installing hyprpaper
//! and clicking Refresh must make the wallpaper section appear, not wait for an app
//! restart). What differs from startup is how the result is merged into live state:
//! [`merge_refreshed_files`] preserves staged store edits and reports external file
//! edits (R5.6), and [`adopt_refreshed_model`] keeps a bespoke model that holds staged
//! edits rather than clobbering the user's in-progress work.
//!
//! # Scope: the settings defined so far (§6 extends this)
//!
//! The full `SettingId` ↔ file ↔ parser ↔ key mapping grows with the §6 category
//! pages; today the store-backed pages exercise two files, so those are what this
//! loads:
//!
//! - `~/.config/hypr/input.lua` (the app-owned `hl.config({ input = { … } })` table,
//!   analysis §7.1) via the Lua-literal editor → the Input page's keyboard layouts,
//!   keyboard options, mouse sensitivity, and touchpad toggles (tasks 6.6, 11.4);
//! - `~/.config/hypr/hypridle.conf` via the hyprlang parser → the Power & Idle page's
//!   dim/lock/DPMS timeouts (by positional `listener` matching, §3.2) and lock command
//!   (task 6.8).
//!
//! Each §6 page extends [`load_files`] with its own file loader in the same shape
//! (read → parse → map keys to `(SettingId, Value)` → build a re-reader). Where a
//! setting maps to more than one on-disk key, the composition is done here for
//! reading; the reverse split for *writing* is the owning page's job, since startup
//! produces no writes.
//!
//! The Notifications page has no startup model: every control on it is runtime-only
//! (task 11.7), so the window builds its model when it builds the page and the first
//! daemon query happens on page entry.

use std::io;
use std::path::{Path, PathBuf};

use crate::core::detect::{Binary, Capabilities, Daemon, DetectionInputs, HyprConfigProvider};
use crate::core::display::DisplayModel;
use crate::core::home_path::home_dir;
use crate::core::input::{InputModel, default_xkb_registry, input_values, read_input_lua};
use crate::core::lock_grace::parse_grace;
use crate::core::model::{SettingId, Value};
use crate::core::power::{PowerModel, power_key_path};
use crate::core::store::{FileReader, FileValues, RefreshReport, SettingsStore};
use crate::core::theme::{
    PaletteModel, ThemeRoots, ThemesModel, ThemesPaths, WallpaperModel, WallpaperPaths,
};
use crate::parsers::hyprlang::HyprlangFile;
use crate::system::command::SystemCommandRunner;

/// One backing config file loaded at startup: the live XDG path it was read from,
/// the bytes + parsed originals, and a reader to re-parse it later.
///
/// The window feeds each of these straight into
/// [`SettingsStore::load_file`](crate::core::store::SettingsStore::load_file), which
/// records the originals and the freshness baseline (from `initial.bytes`) and keeps
/// `reader` for the conflict-reload path (R5.6).
pub(crate) struct LoadedFile {
    /// The live XDG path the file was read from (the key the store tracks it under).
    pub(crate) path: PathBuf,
    /// The bytes read and the originals parsed from them, in a single read so the
    /// freshness baseline matches exactly what was parsed (see the store docs).
    pub(crate) initial: FileValues,
    /// A closure that re-reads and re-parses this file for a later conflict reload.
    pub(crate) reader: FileReader,
}

/// The product of one startup worker pass: detected capabilities plus the backing
/// files that could be read and parsed (task 5.4).
///
/// It is [`Send`] (its fields — [`Capabilities`], `PathBuf`/`Vec<u8>` data, and the
/// [`Send`] [`FileReader`] boxes — all are), so it can be produced on the worker
/// thread and handed back to the main thread for the window to apply.
pub(crate) struct StartupLoad {
    /// What detection found (architecture §4). Drives which categories/rows show.
    pub(crate) capabilities: Capabilities,
    /// The backing files that loaded, in a fixed order; each is applied to the store.
    /// A file that was missing/unreadable/unparseable is simply absent here.
    pub(crate) files: Vec<LoadedFile>,
    /// The Display page's runtime-discovered model (task 6.1), built by probing
    /// `hyprctl monitors -j` and reading `monitors.conf`, or `None` when there is no
    /// live compositor to enumerate (the Display page then shows a placeholder, R4.2).
    pub(crate) display: Option<DisplayModel>,
    /// The Input page's helper (tasks 6.6, 11.4): the `input.lua` path and XKB layout
    /// candidates, or `None` when the page cannot be offered — `hyprctl` is absent,
    /// Hyprland does not run its Lua config provider, or `input.lua` is missing or not
    /// fully understood by the editor (the Input category is then hidden entirely,
    /// R4.2). It supplies the layout add-list and renders the store's dirty Input
    /// settings into the `input.lua` write on Apply.
    pub(crate) input: Option<InputModel>,
    /// The Power & Idle page's helper (task 6.8): the `hypridle.conf` path, or `None` when
    /// hypridle is absent (the Power & Idle category is then hidden entirely, R4.2). It
    /// renders the store's dirty dim/lock/DPMS timeouts and lock command into the
    /// `hypridle.conf` write on Apply, which the pipeline follows with a hypridle restart.
    pub(crate) power: Option<PowerModel>,
    /// The Theme page's palette-scheme model (task 6.3), built by enumerating the
    /// discovered `colors/` directory and detecting the active scheme, or `None` when
    /// there is no dotfiles palette source (the Theme palette section is then hidden,
    /// R4.2/R8.5).
    pub(crate) palette: Option<PaletteModel>,
    /// The Theme page's GTK/icon/cursor theme model (task 6.4), built by discovering
    /// installed themes and reading the backing config, or `None` when `gsettings` is
    /// absent (the appearance section is then hidden, R4.2).
    pub(crate) themes: Option<ThemesModel>,
    /// The Theme page's wallpaper / lock-background model (task 6.5), built by reading
    /// `hyprpaper.conf`/`hyprlock.conf`, or `None` when hyprpaper is absent (the
    /// wallpaper section is then hidden, R4.2).
    pub(crate) wallpaper: Option<WallpaperModel>,
}

/// The live XDG paths of the backing config files loaded at startup (R8.5).
///
/// Resolved from `$XDG_CONFIG_HOME`/`$HOME` rather than a hardcoded `~/.dotfiles`
/// path, so the load behaves identically for a symlink-deployed dotfiles setup and a
/// plain-file one — the writer follows symlinks later (R8.5). Kept as its own type so
/// the loading logic can be driven with injected paths in tests without touching the
/// real environment.
struct BackingPaths {
    /// `~/.config/hypr/input.lua` — the app-owned Lua input table.
    input_lua: PathBuf,
    /// `~/.config/hypr/hypridle.conf` — the idle timeouts + lock command (hyprlang).
    hypridle_conf: PathBuf,
}

impl BackingPaths {
    /// Resolves the backing paths from the XDG environment.
    ///
    /// Prefers `$XDG_CONFIG_HOME`, falling back to `$HOME/.config`; if neither is set
    /// the returned relative paths simply fail to read in [`load_files`] (the files
    /// are skipped), so this never has to fail.
    fn from_system() -> Self {
        let config_home = config_home();
        BackingPaths {
            input_lua: input_lua_path(&config_home),
            hypridle_conf: config_home.join("hypr").join("hypridle.conf"),
        }
    }
}

/// The live path of `input.lua` under the XDG config base directory `config_home`
/// (R8.5) — one definition for the store loader and the Input page helper, which must
/// agree on the file.
fn input_lua_path(config_home: &Path) -> PathBuf {
    config_home.join("hypr").join("input.lua")
}

/// The XDG config base directory: `$XDG_CONFIG_HOME`, else `$HOME/.config`, else a
/// bare `.config` relative path (which simply misses on read).
fn config_home() -> PathBuf {
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME") {
        if !config_home.is_empty() {
            return PathBuf::from(config_home);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Path::new(&home).join(".config");
    }
    PathBuf::from(".config")
}

/// Runs one full startup pass — detection plus parsing every backing file — for the
/// real system (the worker-thread entry point, architecture §8).
///
/// Called off the main thread by [`super::window`]. It never panics and never fails:
/// detection degrades every absent probe to "absent" (task 4.3), and each file that
/// cannot be read or parsed is skipped with a `warn` (R4.4). No backing config paths
/// are passed to [`DetectionInputs::from_system`] for a redundant readability check —
/// this module reads and parses them directly, which subsumes readability; the §6
/// pages register their own per-row config paths with detection when they need
/// [`Capabilities::config_readable`].
pub(crate) fn load() -> StartupLoad {
    let capabilities = Capabilities::detect(&DetectionInputs::from_system(
        &SystemCommandRunner::new(),
        Vec::new(),
    ));
    let files = load_files(&BackingPaths::from_system());
    let display = load_display(&capabilities);
    let input = load_input(&capabilities);
    let power = load_power(&capabilities);
    let palette = load_palette(&capabilities);
    let themes = load_themes(&capabilities);
    let wallpaper = load_wallpaper(&capabilities);
    StartupLoad {
        capabilities,
        files,
        display,
        input,
        power,
        palette,
        themes,
        wallpaper,
    }
}

/// Folds the backing files a manual-refresh [`load`] pass read into the store
/// (task 9.3; R4.3, R5.6).
///
/// A refresh pass returns two classes of file, handled differently:
///
/// - **Files the store already tracks.** Their external changes go through
///   [`SettingsStore::refresh`]: each changed file is re-read against its freshness
///   baseline, its originals are reloaded with pending staged edits preserved, and it
///   is named in the returned [`RefreshReport`] so the window can warn (R5.6). The
///   worker's own re-read of a tracked file is deliberately discarded — `refresh`
///   reads the file's *current* bytes, so ingesting the worker's slightly older read
///   could re-baseline behind an edit that landed in between.
/// - **Files that appeared since the last load** (e.g. a `hypridle.conf` created
///   mid-session). These have no baseline to conflict with, so they are loaded exactly
///   like a startup file, making their settings available to the repopulated pages.
///
/// Kept GTK-free here, next to the load it merges, so the refresh contract is
/// headlessly testable (R6.2).
pub(crate) fn merge_refreshed_files(
    store: &mut SettingsStore,
    files: Vec<LoadedFile>,
) -> RefreshReport {
    let report = store.refresh();
    for file in files {
        if !store.is_tracked(&file.path) {
            tracing::info!(
                path = %file.path.display(),
                "backing config appeared since the last load; now tracking it (R4.3)"
            );
            store.load_file(file.path, file.initial, file.reader);
        }
    }
    report
}

/// Decides which instance of a bespoke page model survives a manual refresh
/// (task 9.3; R4.3).
///
/// `fresh` is what a new [`load`] pass built against the refreshed
/// [`Capabilities`]; `current` is the model the window holds (a staging source with
/// its own dirty state — Display, palette, GTK/icon/cursor themes, or wallpaper).
/// Three rules, in order:
///
/// - **`fresh` is `None`** → the model's capability is gone (its app was uninstalled,
///   or its source removed), so the model is dropped and its section hides (R4.2).
///   Any staged edits it held could no longer be applied or reloaded anyway.
/// - **`current` has staged edits** → keep it wholesale. This mirrors the store's
///   refresh contract (pending edits are never silently discarded, R5.6): replacing
///   the model would drop the user's in-progress work the moment they click Refresh.
///   Its originals stay stale until the next clean Refresh, which is safe for each
///   model on its own terms: the file-writing models (Display, GTK/icon/cursor
///   themes, wallpaper) re-check their backing files at Apply time (re-read + hash),
///   so a stale model still cannot clobber an external edit; and the palette model
///   writes no file itself — its apply runs `scripts/generate-colors`, which
///   regenerates from the *current* on-disk palette sources — so staleness there at
///   worst means an outdated scheme list in the drop-down.
/// - **Otherwise** → adopt `fresh`, so changes to the model's sources since the last
///   load (newly installed themes, an externally switched palette, a hand-edited
///   `monitors.conf`) are reflected in the rebuilt page.
///
/// Generic over the model with an explicit `is_dirty` accessor because the four
/// staging models share no trait; a plain `fn` pointer keeps the call sites to a
/// method reference like `DisplayModel::is_dirty`.
pub(crate) fn adopt_refreshed_model<M>(
    current: &mut Option<M>,
    fresh: Option<M>,
    is_dirty: fn(&M) -> bool,
) {
    match fresh {
        None => *current = None,
        Some(fresh) => {
            if current.as_ref().is_some_and(is_dirty) {
                tracing::debug!(
                    "keeping a page model with staged edits across a refresh; its sources \
                     re-sync on the next clean refresh (R4.3/R5.6)"
                );
            } else {
                *current = Some(fresh);
            }
        }
    }
}

/// Builds the Power & Idle page's helper on the worker thread (task 6.8; R4.2).
///
/// Only builds when hypridle is present — the Power & Idle category edits `hypridle.conf`
/// and restarts hypridle on Apply (task 5.1's category gate), so without hypridle there is
/// nothing to configure and the category is hidden entirely (R4.2). It records the live
/// `hypridle.conf` path (R8.5) for the Apply-time timeout/lock-command write. Building it
/// here, off the main thread, keeps startup inside the R8.1 budget (architecture §8).
fn load_power(capabilities: &Capabilities) -> Option<PowerModel> {
    if !capabilities.has_binary(Binary::Hypridle) {
        tracing::info!("hypridle not found; the Power & Idle page is hidden (R4.2)");
        return None;
    }
    let hypridle_conf = config_home().join("hypr").join("hypridle.conf");
    Some(PowerModel::load(hypridle_conf))
}

/// Builds the Input page's helper on the worker thread for the real system (tasks
/// 6.6, 11.2, 11.4; R4.2) — [`load_input_from`] with the live `input.lua` path and
/// the XKB registry resolved by [`default_xkb_registry`].
fn load_input(capabilities: &Capabilities) -> Option<InputModel> {
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
/// [`load_input_lua`]); this helper only supplies the candidates and the Apply-time
/// write. Split from [`load_input`] so the decision is testable against a fixture
/// file instead of the real `~/.config`.
fn load_input_from(
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
fn load_wallpaper(capabilities: &Capabilities) -> Option<WallpaperModel> {
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
/// Only builds when detection discovered the dotfiles palette source (R3.2/R8.5): its
/// [`PaletteSource`](crate::core::detect::PaletteSource) supplies the `colors/`
/// directory to enumerate and the `scripts/generate-colors` path the Apply pipeline
/// runs. When it is absent — no dotfiles repo behind the config, or an incomplete one —
/// this returns `None` and the Theme palette section is hidden (R4.2/R4.4). The active
/// scheme is read from the deployed generated `~/.config/hypr/colors.conf` header (task
/// 3.7, R3.2), at its live XDG path. Running the enumeration here keeps its file reads
/// off the main thread and inside the R8.1 cold-start budget (architecture §8).
fn load_palette(capabilities: &Capabilities) -> Option<PaletteModel> {
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
/// at `info` (R4.2). When present, it discovers installed themes under the XDG theme
/// roots (`~/.themes`, the data dirs, `/usr/share/...`), reads the backing config
/// (both `settings.ini`, `hyprland.lua`, `uwsm/env`, `~/.icons/default/index.theme`),
/// gates the live-restyle claim on
/// the settings portal (R2.2), and passes the app's own `GTK_THEME` environment value
/// so an override disables the GTK-theme drop-down (R3.3). It also hands the model the
/// discovered dotfiles repo root and whether `xsettingsd` runs, for the Theme page's
/// two "another program may rewrite GTK settings" notes (task 11.12), and the notifd
/// status, for the note that a notifd too old to reload shows a new icon theme only
/// after its next start (task 11.14). Running the
/// discovery here, on the startup worker, keeps its filesystem scans off the main
/// thread and inside the R8.1 cold-start budget (architecture §8).
fn load_themes(capabilities: &Capabilities) -> Option<ThemesModel> {
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
/// `~/.local/share`. A root that does not exist is simply skipped by discovery, so a
/// missing `~/.themes` is harmless.
fn theme_roots() -> ThemeRoots {
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
fn themes_paths(capabilities: &Capabilities) -> ThemesPaths {
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
/// socket) — otherwise there is no compositor to enumerate, so it returns `None` and
/// the Display page shows a placeholder (R4.2). Running the `hyprctl monitors -j`
/// probe here, on the startup worker, keeps it off the main thread and inside the
/// R8.1 cold-start budget (architecture §8). The merge itself is the GTK-free
/// [`DisplayModel::load`]; a failed probe or an unreadable `monitors.conf` degrades
/// gracefully there (R4.4). The detected laptop-display toggle script (task 11.8) is
/// handed over so the model offers the laptop toggle only when it exists (R4.2).
fn load_display(capabilities: &Capabilities) -> Option<DisplayModel> {
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

/// Reads and parses each backing file at `paths`, returning one [`LoadedFile`] per
/// file that loaded (task 5.4).
///
/// Split out from [`load`] so the file-loading half is exercised with injected paths
/// in tests (the detection half is covered by `core::detect`'s own tests). Each file
/// is attempted independently: a failure to read or parse it is logged and the file
/// is skipped, never propagated, so one bad file cannot suppress the others (R4.4).
fn load_files(paths: &BackingPaths) -> Vec<LoadedFile> {
    let mut files = Vec::new();
    push_loaded(&mut files, &paths.input_lua, load_input_lua, "input.lua");
    push_loaded(
        &mut files,
        &paths.hypridle_conf,
        load_hypridle_conf,
        "hypridle.conf",
    );
    files
}

/// Attempts to load `path` with `loader`, pushing a [`LoadedFile`] on success and
/// logging a `warn` on failure (R4.4/R7.3).
///
/// `loader` is stored as the file's re-reader too, so a later conflict reload
/// (R5.6) re-parses through the same code path. `label` is a short, static name used
/// only in the log line — never the file's path or contents.
fn push_loaded(
    files: &mut Vec<LoadedFile>,
    path: &Path,
    loader: fn(&Path) -> io::Result<FileValues>,
    label: &'static str,
) {
    match loader(path) {
        Ok(initial) => {
            tracing::debug!(
                file = label,
                settings = initial.values.len(),
                "loaded backing config file"
            );
            files.push(LoadedFile {
                path: path.to_path_buf(),
                initial,
                reader: Box::new(loader),
            });
        }
        Err(error) => {
            // R4.4: a missing/unreadable/unparseable config degrades to skipped. The
            // dependent controls simply have no store value (they render their
            // default and reject edits) until the file appears; startup is unaffected.
            tracing::warn!(
                file = label,
                %error,
                "backing config file could not be read or parsed; its settings are skipped (R4.4)"
            );
        }
    }
}

/// Reads and parses `input.lua` (the app-owned Lua input table) into the Input-page
/// settings it backs (tasks 5.2, 6.6, 11.4; analysis §7.1).
///
/// A read error propagates (the caller skips the file). So does a file the
/// Lua-literal editor does not fully understand — unbalanced brackets, no `input`
/// table, or any opaque line ([`check_input_lua`](crate::core::input::check_input_lua))
/// — reported as [`io::ErrorKind::InvalidData`] with the reason (line numbers only,
/// R7.3): the Input page is hidden for such a file, so it must not seed store values
/// either. Which keys become settings is decided by
/// [`input_values`], the read half of the same
/// address map the writer uses.
///
/// Crate-visible (like the `hypridle.conf` loader) so `crate::testing` can re-expose it
/// to the integration suites, which must load the store through the app's real
/// loader rather than a test re-implementation (task 7.2).
pub(crate) fn load_input_lua(path: &Path) -> io::Result<FileValues> {
    let (bytes, file) = read_input_lua(path)?;
    let values = input_values(&file);
    Ok(FileValues { bytes, values })
}

/// Reads and parses `hypridle.conf` (hyprlang) into the Power & Idle-page settings it
/// backs (task 6.8, analysis §6).
///
/// A read error propagates (the caller skips the file); a parse never fails — the hyprlang
/// parser is lossless and surfaces oddities as warnings. The three idle timeouts are read
/// by **positional listener matching** (`listener[0]` = dim, `[1]` = lock, `[2]` = DPMS),
/// through the shared [`power_key_path`] map so a value is parsed from and written back to
/// the identical address; the lock command comes from `general.lock_cmd`, and the lock
/// grace period from the `--grace N` token inside it (task 11.13). Only the keys
/// actually present and parseable become settings. An out-of-range value on disk is stored
/// verbatim as the original (the store validates only staged edits), so a hand-written
/// config is never rejected at load.
///
/// Crate-visible for the same reason as [`load_input_lua`] (task 7.2).
pub(crate) fn load_hypridle_conf(path: &Path) -> io::Result<FileValues> {
    let bytes = std::fs::read(path)?;
    let values = {
        let text = String::from_utf8_lossy(&bytes);
        let (file, warnings) = HyprlangFile::parse(&text);
        for warning in &warnings {
            tracing::warn!(file = "hypridle.conf", %warning, "hyprlang parse warning");
        }

        let mut values = Vec::new();
        // The three idle timeouts, addressed by positional listener matching (§3.2); an
        // unparseable value is skipped rather than defaulted.
        for id in [
            SettingId::DimTimeout,
            SettingId::LockTimeout,
            SettingId::DpmsTimeout,
        ] {
            if let Some(path) = power_key_path(id) {
                if let Some(raw) = file.value(&path) {
                    if let Ok(seconds) = raw.trim().parse::<i64>() {
                        values.push((id, Value::Integer(seconds)));
                    }
                }
            }
        }
        // The session lock command lives in `general.lock_cmd` (not a listener's
        // on-timeout — see `core::power`), read verbatim. hyprlock's grace period is the
        // `--grace N` token inside it (task 11.13): seeded only when the command starts
        // hyprlock in a shape the token editor accepts, which is also what makes the
        // grace control appear (`ui::row`'s content gate). No flag seeds 0.
        if let Some(path) = power_key_path(SettingId::LockCommand) {
            if let Some(command) = file.value(&path) {
                values.push((SettingId::LockCommand, Value::String(command.to_string())));
                match parse_grace(command) {
                    Ok(seconds) => values.push((
                        SettingId::LockGracePeriod,
                        Value::Integer(i64::from(seconds.unwrap_or(0))),
                    )),
                    Err(reason) => tracing::debug!(
                        file = "hypridle.conf",
                        %reason,
                        "no editable hyprlock grace period in lock_cmd; the grace control \
                         stays hidden"
                    ),
                }
            }
        }
        values
    };

    Ok(FileValues { bytes, values })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::store::SettingsStore;

    /// Applies the loaded files into a fresh store, mirroring what the window does on
    /// the main thread when the worker completes.
    fn store_from(files: Vec<LoadedFile>) -> SettingsStore {
        let mut store = SettingsStore::new();
        for file in files {
            store.load_file(file.path, file.initial, file.reader);
        }
        store
    }

    /// A minimal usable `input.lua` whose `input` table holds only `sensitivity`.
    fn input_lua_with_sensitivity(sensitivity: &str) -> String {
        format!(
            "hl.config({{\n    input = {{\n        sensitivity = {sensitivity},\n    }},\n}})\n"
        )
    }

    #[test]
    fn input_lua_parses_into_the_input_settings() {
        // The five Input-page settings map from their Lua literals at the right table
        // depth (`input.*` and `input.touchpad.*`, `tap_to_click` with an underscore),
        // with the right kinds.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("input.lua");
        std::fs::write(
            &path,
            "hl.config({\n    input = {\n        kb_layout = \"us,se\",\n        kb_options = \"grp:win_space_toggle,caps:escape\",\n        sensitivity = 0.3,\n        touchpad = {\n            natural_scroll = true,\n            tap_to_click = false,\n        },\n    },\n})\n",
        )
        .expect("write input.lua fixture");

        let values = load_input_lua(&path).expect("input.lua loads");
        assert_eq!(
            values.values,
            vec![
                (
                    SettingId::KeyboardLayouts,
                    Value::String("us,se".to_string())
                ),
                (
                    SettingId::KeyboardOptions,
                    Value::String("grp:win_space_toggle,caps:escape".to_string())
                ),
                (SettingId::MouseSensitivity, Value::Float(0.3)),
                (SettingId::TouchpadNaturalScroll, Value::Bool(true)),
                (SettingId::TouchpadTapToClick, Value::Bool(false)),
            ],
        );
        assert_eq!(
            values.bytes,
            std::fs::read(&path).expect("read back"),
            "the freshness baseline is exactly the bytes parsed"
        );
    }

    #[test]
    fn input_lua_seeds_empty_keyboard_options_when_absent() {
        // With an `input` table but no `kb_options` field, an empty KeyboardOptions is
        // seeded so a curated switch can stage a first option (the writer inserts the
        // field on Apply).
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("input.lua");
        std::fs::write(&path, input_lua_with_sensitivity("0.3")).expect("write input.lua");
        let values = load_input_lua(&path).expect("loads");
        assert!(
            values
                .values
                .contains(&(SettingId::KeyboardOptions, Value::String(String::new()))),
            "an empty kb_options is seeded when the input table exists"
        );
    }

    #[test]
    fn an_unusable_input_lua_is_skipped_by_the_store_loader() {
        // A file the Lua-literal editor does not fully understand seeds no store
        // values: no `input` table, an opaque line, or unbalanced brackets are each an
        // InvalidData error the caller skips (the Input page is hidden for it too).
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("input.lua");
        for contents in [
            "-- no input table here\n".to_string(),
            input_lua_with_sensitivity("docked and 0 or 0.3"),
            "hl.config({\n    input = {\n".to_string(),
        ] {
            std::fs::write(&path, &contents).expect("write input.lua");
            let error = load_input_lua(&path).expect_err("an unusable file is skipped");
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "for {contents:?}");
        }
        // A missing file keeps its own read error.
        let error = load_input_lua(&dir.path().join("absent.lua")).expect_err("missing");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn a_missing_file_is_skipped_without_error() {
        // R4.4: a backing config that does not exist yields no LoadedFile, and the
        // orchestration continues — a missing file never aborts the load. Point both
        // backing paths at nonexistent files and assert nothing loads.
        let dir = tempfile::tempdir().expect("temp dir");
        let paths = BackingPaths {
            input_lua: dir.path().join("no-input.lua"),
            hypridle_conf: dir.path().join("no-hypridle.conf"),
        };
        assert!(
            load_files(&paths).is_empty(),
            "missing files must be skipped, not loaded"
        );
    }

    #[test]
    fn an_unusable_file_is_skipped_and_the_others_still_load() {
        // R4.4: an input.lua the editor refuses (an opaque line) is skipped, rather than
        // panicking or loading garbage, and the readable hypridle.conf beside it still
        // loads — one bad file never suppresses the others.
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("input.lua"),
            input_lua_with_sensitivity("docked and 0 or 0.3"),
        )
        .expect("write an unusable input.lua");
        std::fs::write(
            dir.path().join("hypridle.conf"),
            b"listener {\n    timeout = 120\n}\n",
        )
        .expect("write hypridle.conf");
        let paths = BackingPaths {
            input_lua: dir.path().join("input.lua"),
            hypridle_conf: dir.path().join("hypridle.conf"),
        };
        let loaded = load_files(&paths);
        assert_eq!(loaded.len(), 1, "only the usable file loads");
        assert_eq!(loaded[0].path, paths.hypridle_conf);
        assert_eq!(
            loaded[0].initial.values,
            vec![(SettingId::DimTimeout, Value::Integer(120))]
        );
    }

    #[test]
    fn loaded_files_populate_the_store_with_originals_and_a_baseline() {
        // The end-to-end contract the window relies on: applying the LoadedFiles to a
        // store establishes real originals AND a freshness baseline, so a refresh
        // over the unchanged files reports no conflict.
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("input.lua"),
            "hl.config({\n    input = {\n        kb_layout = \"us\",\n        sensitivity = 0.5,\n    },\n})\n",
        )
        .expect("write input.lua");
        std::fs::write(
            dir.path().join("hypridle.conf"),
            b"listener {\n    timeout = 10\n}\n",
        )
        .expect("write hypridle.conf");
        let paths = BackingPaths {
            input_lua: dir.path().join("input.lua"),
            hypridle_conf: dir.path().join("hypridle.conf"),
        };

        let mut store = store_from(load_files(&paths));
        assert_eq!(
            store.value(SettingId::KeyboardLayouts),
            Some(&Value::String("us".to_string())),
            "the parsed original is readable from the store"
        );
        assert_eq!(
            store.value(SettingId::MouseSensitivity),
            Some(&Value::Float(0.5))
        );
        assert_eq!(
            store.value(SettingId::DimTimeout),
            Some(&Value::Integer(10))
        );
        assert!(
            store.refresh().is_empty(),
            "the freshness baseline matches the on-disk bytes, so no self-conflict"
        );
    }

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
    fn hypridle_conf_parses_into_the_power_settings() {
        // The four Power & Idle settings map from hypridle.conf by positional listener
        // matching: listener[0].timeout -> dim, [1] -> lock, [2] -> DPMS, and
        // general.lock_cmd -> the lock command (task 6.8).
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("hypridle.conf");
        std::fs::write(
            &path,
            b"general {\n    lock_cmd = pidof hyprlock || hyprlock\n}\n\nlistener {\n    timeout = 150\n    on-timeout = brightnessctl -s set 10\n}\n\nlistener {\n    timeout = 300\n    on-timeout = loginctl lock-session\n}\n\nlistener {\n    timeout = 330\n    on-timeout = hyprctl dispatch dpms off\n}\n",
        )
        .expect("write hypridle.conf fixture");

        let values = load_hypridle_conf(&path).expect("hypridle.conf loads");
        assert_eq!(
            values.values,
            vec![
                (SettingId::DimTimeout, Value::Integer(150)),
                (SettingId::LockTimeout, Value::Integer(300)),
                (SettingId::DpmsTimeout, Value::Integer(330)),
                (
                    SettingId::LockCommand,
                    Value::String("pidof hyprlock || hyprlock".to_string())
                ),
                // hyprlock is started without a grace flag: that is a grace of 0.
                (SettingId::LockGracePeriod, Value::Integer(0)),
            ],
        );
    }

    #[test]
    fn hypridle_conf_seeds_the_grace_period_from_the_lock_command() {
        // Task 11.13: the grace period is the `--grace N` token inside
        // `general.lock_cmd`, seeded only when the command starts hyprlock in a shape
        // the token editor accepts; otherwise no grace value is loaded (and the control
        // stays hidden), while the lock command itself always loads.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("hypridle.conf");
        for (command, grace) in [
            ("pidof hyprlock || hyprlock --grace 10", Some(10)),
            ("hyprlock -g 0", Some(0)),
            ("swaylock -f", None),
            ("hyprlock --grace=10", None),
        ] {
            std::fs::write(&path, format!("general {{\n    lock_cmd = {command}\n}}\n"))
                .expect("write hypridle.conf fixture");
            let values = load_hypridle_conf(&path)
                .expect("hypridle.conf loads")
                .values;
            let mut expected = vec![(SettingId::LockCommand, Value::String(command.to_string()))];
            if let Some(seconds) = grace {
                expected.push((SettingId::LockGracePeriod, Value::Integer(seconds)));
            }
            assert_eq!(values, expected, "{command:?}");
        }
    }

    #[test]
    fn hypridle_conf_loads_only_the_listeners_that_exist() {
        // A config with fewer than three listeners contributes only the settings whose
        // address resolves — positional matching skips the absent occurrences rather than
        // guessing (R4.4), and a missing general block skips the lock command.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("hypridle.conf");
        std::fs::write(
            &path,
            b"listener {\n    timeout = 120\n}\n\nlistener {\n    timeout = 240\n}\n",
        )
        .expect("write hypridle.conf fixture");

        let values = load_hypridle_conf(&path).expect("hypridle.conf loads");
        assert_eq!(
            values.values,
            vec![
                (SettingId::DimTimeout, Value::Integer(120)),
                (SettingId::LockTimeout, Value::Integer(240)),
            ],
            "only the two present listeners load; no DPMS timeout, no lock command"
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

    #[test]
    fn refresh_merge_tracks_a_new_file_and_preserves_staged_edits() {
        // The task-9.3 refresh contract end to end at the headless seam: between the
        // startup load and a manual Refresh, one backing file appears (hypridle.conf)
        // and one tracked file is edited externally (input.lua), while the user has a
        // staged edit pending. The merge must (a) start tracking the new file so its
        // settings become available, (b) reload the edited file's originals and report
        // it (R5.6), and (c) keep the staged edit (R5.1).
        let dir = tempfile::tempdir().expect("temp dir");
        let paths = BackingPaths {
            input_lua: dir.path().join("input.lua"),
            hypridle_conf: dir.path().join("hypridle.conf"),
        };
        std::fs::write(&paths.input_lua, input_lua_with_sensitivity("0.3"))
            .expect("write input.lua");

        // Startup: only input.lua exists; the user stages a sensitivity edit.
        let mut store = store_from(load_files(&paths));
        store
            .stage(SettingId::MouseSensitivity, Value::Float(0.8))
            .expect("stage a valid sensitivity");

        // Between load and Refresh: hypridle.conf appears, input.lua is edited
        // externally (sensitivity 0.3 -> 0.1).
        std::fs::write(&paths.hypridle_conf, b"listener {\n    timeout = 120\n}\n")
            .expect("write hypridle.conf");
        std::fs::write(&paths.input_lua, input_lua_with_sensitivity("0.1"))
            .expect("rewrite input.lua");

        // Refresh: re-run the same loader pass and merge it into the live store.
        let report = merge_refreshed_files(&mut store, load_files(&paths));

        assert_eq!(
            report.reloaded(),
            std::slice::from_ref(&paths.input_lua),
            "the externally edited tracked file is reloaded and reported (R5.6)"
        );
        assert!(
            store.is_tracked(&paths.hypridle_conf),
            "the file that appeared since startup is now tracked"
        );
        assert_eq!(
            store.value(SettingId::DimTimeout),
            Some(&Value::Integer(120)),
            "the new file's settings are available to the repopulated pages"
        );
        assert_eq!(
            store.original(SettingId::MouseSensitivity),
            Some(&Value::Float(0.1)),
            "the edited file's originals were reloaded from disk"
        );
        assert_eq!(
            store.value(SettingId::MouseSensitivity),
            Some(&Value::Float(0.8)),
            "the pending staged edit survives the refresh (R5.1/R5.6)"
        );
    }

    #[test]
    fn refresh_merge_is_quiet_when_nothing_changed() {
        // The common case: no file appeared or changed between load and Refresh, so
        // the merge reports nothing (no conflict warning) and staged edits stay put.
        let dir = tempfile::tempdir().expect("temp dir");
        let paths = BackingPaths {
            input_lua: dir.path().join("input.lua"),
            hypridle_conf: dir.path().join("no-hypridle.conf"),
        };
        std::fs::write(&paths.input_lua, input_lua_with_sensitivity("0.3"))
            .expect("write input.lua");

        let mut store = store_from(load_files(&paths));
        store
            .stage(SettingId::MouseSensitivity, Value::Float(-0.5))
            .expect("stage a valid sensitivity");

        let report = merge_refreshed_files(&mut store, load_files(&paths));
        assert!(report.is_empty(), "unchanged files raise no warning");
        assert_eq!(
            store.value(SettingId::MouseSensitivity),
            Some(&Value::Float(-0.5)),
            "the staged edit is untouched by a no-op refresh"
        );
    }

    #[test]
    fn a_hyprlang_provider_hides_the_input_page_and_the_hyprland_cursor_write() {
        // Task 11.2 (analysis §8.6): the config provider is a guard. With `hyprctl`
        // present, a Lua host shows the Input page and keeps the Hyprland-side cursor
        // copy; a hyprlang host hides both, with nothing routed to a hyprlang writer.
        use crate::ui::category::{SidebarCategory, visible_categories};

        let dir = tempfile::tempdir().expect("temp dir");
        let input_lua = dir.path().join("input.lua");
        std::fs::write(&input_lua, input_lua_with_sensitivity("0.3")).expect("write input.lua");
        let xkb = dir.path().join("no-evdev.xml");

        let lua = Capabilities::for_tests(&[Binary::Hyprctl, Binary::Gsettings], &[], true);
        assert_eq!(lua.hypr_config_provider(), HyprConfigProvider::Lua);
        assert!(visible_categories(&lua).contains(&SidebarCategory::Input));
        assert!(
            load_input_from(&lua, input_lua.clone(), &xkb).is_some(),
            "the Input helper is built"
        );
        assert!(
            themes_paths(&lua)
                .hyprland_lua
                .is_some_and(|path| path.ends_with("hypr/hyprland.lua")),
            "the Hyprland-side cursor copy is written on a Lua host"
        );

        let hyprlang = Capabilities::for_tests(&[Binary::Hyprctl, Binary::Gsettings], &[], true)
            .with_hypr_config_provider_for_tests(HyprConfigProvider::Hyprlang);
        assert!(
            !visible_categories(&hyprlang).contains(&SidebarCategory::Input),
            "the Input page is hidden on a hyprlang host"
        );
        assert!(
            load_input_from(&hyprlang, input_lua, &xkb).is_none(),
            "no Input helper is built"
        );
        assert!(
            themes_paths(&hyprlang).hyprland_lua.is_none(),
            "the Hyprland-side cursor write is hidden on a hyprlang host"
        );
        // The rest of the Theme page is unaffected: gsettings still gates it.
        assert!(visible_categories(&hyprlang).contains(&SidebarCategory::Theme));
        assert!(load_themes(&hyprlang).is_some());
    }

    #[test]
    fn an_absent_or_unusable_input_lua_hides_the_input_page_and_logs_why_at_info() {
        // Task 11.4 (architecture §10, R4.2): on a Lua host with `hyprctl`, the Input
        // page still needs an `input.lua` the editor fully understands. A missing file
        // and one with an opaque line inside the `input` table each build no helper —
        // which leaves the page without rows, so the window drops the category — and
        // the reason is logged at `info`, by line number for the opaque case and never
        // with file contents (R7.3).
        use crate::testing::capture_logs;
        use crate::ui::input::page_rows;

        let caps = Capabilities::for_tests(&[Binary::Hyprctl], &[], true);
        let dir = tempfile::tempdir().expect("temp dir");
        let xkb = dir.path().join("no-evdev.xml");
        let hidden_reason = |events: &[crate::testing::CapturedEvent]| {
            events
                .iter()
                .find(|event| {
                    event.level == tracing::Level::INFO
                        && event
                            .message
                            .contains("input.lua cannot back the Input page")
                })
                .map(|event| event.fields.clone())
        };

        // No input.lua at all.
        let missing = dir.path().join("input.lua");
        let mut model = None;
        let events = capture_logs(|| model = load_input_from(&caps, missing.clone(), &xkb));
        assert!(model.is_none(), "no helper without input.lua");
        assert!(
            page_rows(model.as_ref(), &caps).is_empty(),
            "the page has no rows"
        );
        let fields = hidden_reason(&events).expect("the hide is logged at info");
        assert!(
            fields.contains("unreadable"),
            "the reason is given: {fields}"
        );

        // An input.lua with a computed value inside the `input` table (line 3).
        std::fs::write(
            &missing,
            "hl.config({\n    input = {\n        sensitivity = secret_value(),\n    },\n})\n",
        )
        .expect("write input.lua");
        let events = capture_logs(|| model = load_input_from(&caps, missing.clone(), &xkb));
        assert!(model.is_none(), "no helper for an opaque input table");
        assert!(page_rows(model.as_ref(), &caps).is_empty());
        let fields = hidden_reason(&events).expect("the hide is logged at info");
        assert!(
            fields.contains("(lines 3)"),
            "the opaque line is named: {fields}"
        );
        assert!(
            !fields.contains("secret_value"),
            "no file contents: {fields}"
        );

        // The same tree with a usable file shows the page.
        std::fs::write(&missing, input_lua_with_sensitivity("0.3")).expect("write input.lua");
        let model = load_input_from(&caps, missing, &xkb);
        assert!(model.is_some());
        assert!(!page_rows(model.as_ref(), &caps).is_empty());
    }

    #[test]
    fn refresh_rebuilds_a_model_when_its_capability_appears_or_disappears() {
        // The task-9.3 accept scenario at the capability seam: hypridle is installed
        // between startup and a Refresh. The startup pass built no Power & Idle model
        // (category hidden, R4.2); the refresh pass runs the *same* loader against the
        // new capabilities, builds the model, and the category becomes visible.
        use crate::ui::category::{SidebarCategory, visible_categories};

        let before = Capabilities::for_tests(&[], &[], false);
        let mut current = load_power(&before);
        assert!(current.is_none(), "absent at startup");
        assert!(
            !visible_categories(&before).contains(&SidebarCategory::PowerAndIdle),
            "the category is hidden while the capability is absent (R4.2)"
        );

        // hypridle installed -> Refresh: the loader now builds the model and the
        // adoption swaps it in (no staged edits to protect).
        let after = Capabilities::for_tests(&[Binary::Hypridle], &[], false);
        adopt_refreshed_model(&mut current, load_power(&after), |_| false);
        assert!(current.is_some(), "the model is rebuilt on Refresh (R4.3)");
        assert!(
            visible_categories(&after).contains(&SidebarCategory::PowerAndIdle),
            "the category appears with the refreshed capabilities"
        );

        // hypridle uninstalled -> Refresh: the fresh pass yields None and the model is
        // dropped so the section hides again (R4.2).
        adopt_refreshed_model(&mut current, load_power(&before), |_| false);
        assert!(
            current.is_none(),
            "the model is dropped when its app is gone"
        );
    }

    #[test]
    fn a_dirty_model_survives_a_refresh_and_a_clean_one_is_replaced() {
        // The three adopt_refreshed_model rules, on a stand-in staging model (the four
        // real staging models share no trait; the rule is model-agnostic).
        struct Staging {
            label: &'static str,
            dirty: bool,
        }
        fn is_dirty(model: &Staging) -> bool {
            model.dirty
        }

        // Clean -> the fresh rebuild wins, so external source changes are reflected.
        let mut current = Some(Staging {
            label: "stale",
            dirty: false,
        });
        adopt_refreshed_model(
            &mut current,
            Some(Staging {
                label: "fresh",
                dirty: false,
            }),
            is_dirty,
        );
        assert_eq!(current.as_ref().map(|m| m.label), Some("fresh"));

        // Dirty -> the user's staged edits are kept; the rebuild is discarded (R5.6).
        let mut current = Some(Staging {
            label: "dirty",
            dirty: true,
        });
        adopt_refreshed_model(
            &mut current,
            Some(Staging {
                label: "fresh",
                dirty: false,
            }),
            is_dirty,
        );
        assert_eq!(current.as_ref().map(|m| m.label), Some("dirty"));

        // Capability gone -> dropped even while dirty, so the section hides (R4.2).
        adopt_refreshed_model(&mut current, None, is_dirty);
        assert!(current.is_none());
    }

    #[test]
    fn a_real_wallpaper_model_keeps_its_staged_edit_across_a_refresh() {
        // The dirty-keep rule on a real staging model (not a stand-in): a staged
        // fit-mode edit on a fixture-backed WallpaperModel must survive adoption of
        // the clean model a refresh pass rebuilds from the same sources — this is the
        // exact wiring `apply_refresh_load` runs for the four staging models.
        let dir = tempfile::tempdir().expect("temp dir");
        let hyprpaper_conf = dir.path().join("hyprpaper.conf");
        std::fs::write(
            &hyprpaper_conf,
            b"wallpaper {\n    path = /tmp/wallpaper.jpg\n    fit_mode = cover\n}\n",
        )
        .expect("write hyprpaper.conf fixture");
        // Both loads read the same fixture paths, exactly like startup and a later
        // Refresh reading the same live config (hyprlock.conf is simply absent).
        let paths = || WallpaperPaths {
            hyprpaper_conf: hyprpaper_conf.clone(),
            hyprlock_conf: dir.path().join("hyprlock.conf"),
            home: None,
        };

        let mut model = WallpaperModel::load(paths(), false);
        model.stage_fit("contain");
        assert!(
            model.is_dirty(),
            "the staged fit mode marks the model dirty"
        );
        let mut current = Some(model);

        // Refresh: adoption must keep the dirty model so the staged edit is not
        // silently discarded (R5.6), discarding the clean rebuild.
        adopt_refreshed_model(
            &mut current,
            Some(WallpaperModel::load(paths(), false)),
            WallpaperModel::is_dirty,
        );
        let kept = current.expect("the dirty model survives the refresh");
        assert!(kept.is_dirty(), "the staged edit is still pending");
        let fit_index = kept.selected_fit_index().expect("a fit mode is selected");
        assert_eq!(
            kept.fit_options()[fit_index],
            "contain",
            "the kept model still carries the staged fit mode"
        );
    }
}
