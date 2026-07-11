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
//! # Scope: the settings defined so far (§6 extends this)
//!
//! The full `SettingId` ↔ file ↔ parser ↔ key mapping grows with the §6 category
//! pages; today the declarative framework (task 5.2) exercises two files, so those
//! are what this loads:
//!
//! - `~/.config/hypr/input.conf` (the sourced `input {}` block, analysis §6.3) via
//!   the hyprlang parser → the Input page's keyboard layouts, mouse sensitivity, and
//!   touchpad toggles;
//! - `~/.config/swaync/config.json` via the swaync JSON adapter → the Notifications
//!   page's position and auto-dismiss timeout.
//!
//! Each §6 page extends [`load_files`] with its own file loader in the same shape
//! (read → parse → map keys to `(SettingId, Value)` → build a re-reader). Where a
//! setting maps to more than one on-disk key — swaync's split `positionX`/`positionY`
//! composing the single [`SettingId::NotificationPosition`] enum — the composition is
//! done here for reading; the reverse split for *writing* is the owning §6 page's job
//! (task 6.7), since v1 startup produces no writes.

use std::io;
use std::path::{Path, PathBuf};

use crate::core::detect::{Binary, Capabilities, DetectionInputs};
use crate::core::display::DisplayModel;
use crate::core::model::{SettingId, Value};
use crate::core::store::{FileReader, FileValues};
use crate::core::theme::{
    PaletteModel, ThemeRoots, ThemesModel, ThemesPaths, WallpaperModel, WallpaperPaths,
};
use crate::parsers::hyprlang::{HyprlangFile, KeyPath};
use crate::parsers::swaync::SwayncConfigFile;
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
    /// `~/.config/hypr/input.conf` — the sourced `input {}` block (hyprlang).
    input_conf: PathBuf,
    /// `~/.config/swaync/config.json` — the swaync notification config (JSON).
    swaync_config: PathBuf,
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
            input_conf: config_home.join("hypr").join("input.conf"),
            swaync_config: config_home.join("swaync").join("config.json"),
        }
    }
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
    let capabilities = Capabilities::detect(&DetectionInputs::from_system(Vec::new()));
    let files = load_files(&BackingPaths::from_system());
    let display = load_display(&capabilities);
    let palette = load_palette(&capabilities);
    let themes = load_themes(&capabilities);
    let wallpaper = load_wallpaper(&capabilities);
    StartupLoad {
        capabilities,
        files,
        display,
        palette,
        themes,
        wallpaper,
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
/// writes (R8.5).
fn wallpaper_paths() -> WallpaperPaths {
    let config = config_home();
    WallpaperPaths {
        hyprpaper_conf: config.join("hypr").join("hyprpaper.conf"),
        hyprlock_conf: config.join("hypr").join("hyprlock.conf"),
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
/// (both `settings.ini`, `hyprland.conf`, `uwsm/env`), gates the live-restyle claim on
/// the settings portal (R2.2), and passes the app's own `GTK_THEME` environment value
/// so an override disables the GTK-theme drop-down (R3.3). Running the discovery here,
/// on the startup worker, keeps its filesystem scans off the main thread and inside the
/// R8.1 cold-start budget (architecture §8).
fn load_themes(capabilities: &Capabilities) -> Option<ThemesModel> {
    if !capabilities.has_binary(Binary::Gsettings) {
        tracing::info!("gsettings not found; the GTK/icon/cursor theme controls are hidden (R4.2)");
        return None;
    }
    Some(ThemesModel::load(
        &theme_roots(),
        themes_paths(),
        capabilities.settings_portal_available(),
        std::env::var("GTK_THEME").ok(),
    ))
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

/// The live XDG paths of the four config files a theme/cursor change writes (R8.5).
fn themes_paths() -> ThemesPaths {
    let config = config_home();
    ThemesPaths {
        gtk3_settings: config.join("gtk-3.0").join("settings.ini"),
        gtk4_settings: config.join("gtk-4.0").join("settings.ini"),
        hyprland_conf: config.join("hypr").join("hyprland.conf"),
        uwsm_env: config.join("uwsm").join("env"),
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

/// The user's home directory from `$HOME`, or `None` when it is unset.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// Builds the Display page's model on the worker thread (task 6.1).
///
/// Only probes when Hyprland is reloadable (`hyprctl` on `$PATH` plus a live IPC
/// socket) — otherwise there is no compositor to enumerate, so it returns `None` and
/// the Display page shows a placeholder (R4.2). Running the `hyprctl monitors -j`
/// probe here, on the startup worker, keeps it off the main thread and inside the
/// R8.1 cold-start budget (architecture §8). The merge itself is the GTK-free
/// [`DisplayModel::load`]; a failed probe or an unreadable `monitors.conf` degrades
/// gracefully there (R4.4).
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
    DisplayModel::load(&SystemCommandRunner::new(), monitors_conf)
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
    push_loaded(&mut files, &paths.input_conf, load_input_conf, "input.conf");
    push_loaded(
        &mut files,
        &paths.swaync_config,
        load_swaync_config,
        "swaync/config.json",
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

/// Reads and parses `input.conf` (the sourced `input {}` block, hyprlang) into the
/// Input-page settings it backs (task 5.2/6.6, analysis §6.3).
///
/// A read error propagates (the caller skips the file); a parse never fails — the
/// hyprlang parser is lossless and surfaces oddities as warnings — so only the keys
/// actually present and parseable become settings. An out-of-range value on disk is
/// stored verbatim as the original (the store validates only staged edits), so a
/// hand-written config is never rejected at load.
fn load_input_conf(path: &Path) -> io::Result<FileValues> {
    let bytes = std::fs::read(path)?;
    // Scope the parse so the borrow of `bytes` (through `text`) and the parsed file
    // are both dropped before `bytes` is moved into the returned `FileValues`.
    let values = {
        let text = String::from_utf8_lossy(&bytes);
        let (file, warnings) = HyprlangFile::parse(&text);
        for warning in &warnings {
            tracing::warn!(file = "input.conf", %warning, "hyprlang parse warning");
        }

        let mut values = Vec::new();
        // The ordered keyboard-layout list, kept as the raw comma-joined `kb_layout`
        // value; the reorderable list widget (R2.3) splits/joins on commas.
        if let Some(layouts) = file.value(&KeyPath::at(&["input"], "kb_layout")) {
            values.push((
                SettingId::KeyboardLayouts,
                Value::String(layouts.to_string()),
            ));
        }
        // Mouse/touchpad sensitivity (a decimal in `-1.0..=1.0`). An unparseable
        // value is skipped rather than defaulted.
        if let Some(raw) = file.value(&KeyPath::at(&["input"], "sensitivity")) {
            if let Ok(sensitivity) = raw.trim().parse::<f64>() {
                values.push((SettingId::MouseSensitivity, Value::Float(sensitivity)));
            }
        }
        // Touchpad toggles live one section deeper, under `input.touchpad`.
        if let Some(raw) = file.value(&KeyPath::at(&["input", "touchpad"], "natural_scroll")) {
            if let Some(flag) = parse_hypr_bool(raw) {
                values.push((SettingId::TouchpadNaturalScroll, Value::Bool(flag)));
            }
        }
        if let Some(raw) = file.value(&KeyPath::at(&["input", "touchpad"], "tap-to-click")) {
            if let Some(flag) = parse_hypr_bool(raw) {
                values.push((SettingId::TouchpadTapToClick, Value::Bool(flag)));
            }
        }
        values
    };

    Ok(FileValues { bytes, values })
}

/// Reads and parses swaync's `config.json` into the Notifications-page settings it
/// backs (task 5.2/6.7).
///
/// A read error propagates (the caller skips the file). Malformed JSON, or a
/// non-object root, is treated as "unparseable": it is turned into an
/// [`io::Error`] so the caller skips the file with a `warn`, consistent with the
/// other loaders — swaync itself would reject such a file, so there is nothing to
/// edit.
fn load_swaync_config(path: &Path) -> io::Result<FileValues> {
    let bytes = std::fs::read(path)?;
    let values = {
        let text = String::from_utf8_lossy(&bytes);
        let config = SwayncConfigFile::parse(&text)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;

        let mut values = Vec::new();
        // swaync stores the anchor as two keys (`positionY` ∈ {top,bottom},
        // `positionX` ∈ {left,center,right}); the Notifications drop-down (task 5.2)
        // presents one combined `positionY-positionX` token (e.g. `top-right`). Only
        // compose when both halves are present; the reverse split for *writing* is
        // task 6.7's job (v1 startup produces no writes).
        if let (Some(y), Some(x)) = (config.string("positionY"), config.string("positionX")) {
            values.push((
                SettingId::NotificationPosition,
                Value::Enum(format!("{y}-{x}")),
            ));
        }
        // The auto-dismiss timeout in whole seconds — a single top-level integer key.
        if let Some(seconds) = config.integer("timeout") {
            values.push((SettingId::NotificationTimeout, Value::Integer(seconds)));
        }
        values
    };

    Ok(FileValues { bytes, values })
}

/// Parses a hyprlang boolean token, or `None` for anything unrecognised.
///
/// Hyprland accepts several spellings for a boolean keyword (`true`/`false`,
/// `yes`/`no`, `on`/`off`, `1`/`0`), matched case-insensitively. An unrecognised
/// token yields `None` so the caller skips the setting rather than guessing.
fn parse_hypr_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
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

    #[test]
    fn input_conf_parses_into_the_input_settings() {
        // The four Input-page settings map from their hyprlang keys, at the right
        // section depth (`input.*` and `input.touchpad.*`), with the right kinds.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("input.conf");
        std::fs::write(
            &path,
            b"input {\n    kb_layout = us,se\n    sensitivity = 0.3\n    touchpad {\n        natural_scroll = true\n        tap-to-click = no\n    }\n}\n",
        )
        .expect("write input.conf fixture");

        let values = load_input_conf(&path).expect("input.conf loads");
        assert_eq!(
            values.values,
            vec![
                (
                    SettingId::KeyboardLayouts,
                    Value::String("us,se".to_string())
                ),
                (SettingId::MouseSensitivity, Value::Float(0.3)),
                (SettingId::TouchpadNaturalScroll, Value::Bool(true)),
                (SettingId::TouchpadTapToClick, Value::Bool(false)),
            ],
        );
    }

    #[test]
    fn swaync_config_parses_position_and_timeout() {
        // The two Notifications settings: the timeout as a plain integer, and the
        // position composed from swaync's split positionY/positionX keys into the
        // combined token the drop-down uses.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            br#"{
  "positionX": "right",
  "positionY": "top",
  "timeout": 8
}
"#,
        )
        .expect("write swaync config fixture");

        let values = load_swaync_config(&path).expect("swaync config loads");
        assert_eq!(
            values.values,
            vec![
                (
                    SettingId::NotificationPosition,
                    Value::Enum("top-right".to_string())
                ),
                (SettingId::NotificationTimeout, Value::Integer(8)),
            ],
        );
    }

    #[test]
    fn swaync_position_requires_both_halves() {
        // The position is composed only when BOTH positionY and positionX are present:
        // a single half cannot form a meaningful `positionY-positionX` token, so
        // NotificationPosition is omitted (its control degrades to its default) while
        // an unrelated setting like the timeout still loads.
        let dir = tempfile::tempdir().expect("temp dir");

        // Only positionY present -> no NotificationPosition, but the timeout loads.
        let only_y = dir.path().join("only-y.json");
        std::fs::write(&only_y, br#"{ "positionY": "top", "timeout": 5 }"#)
            .expect("write only-positionY fixture");
        let values = load_swaync_config(&only_y).expect("swaync config loads");
        assert!(
            !values
                .values
                .iter()
                .any(|(id, _)| *id == SettingId::NotificationPosition),
            "positionY alone must not compose a NotificationPosition"
        );
        assert_eq!(
            values.values,
            vec![(SettingId::NotificationTimeout, Value::Integer(5))],
            "the other settings still load with only one position half"
        );

        // Only positionX present -> likewise no NotificationPosition.
        let only_x = dir.path().join("only-x.json");
        std::fs::write(&only_x, br#"{ "positionX": "right" }"#)
            .expect("write only-positionX fixture");
        let values = load_swaync_config(&only_x).expect("swaync config loads");
        assert!(
            !values
                .values
                .iter()
                .any(|(id, _)| *id == SettingId::NotificationPosition),
            "positionX alone must not compose a NotificationPosition"
        );
    }

    #[test]
    fn a_missing_file_is_skipped_without_error() {
        // R4.4: a backing config that does not exist yields no LoadedFile, and the
        // orchestration continues — a missing file never aborts the load. Point both
        // backing paths at nonexistent files and assert nothing loads.
        let dir = tempfile::tempdir().expect("temp dir");
        let paths = BackingPaths {
            input_conf: dir.path().join("no-input.conf"),
            swaync_config: dir.path().join("no-swaync.json"),
        };
        assert!(
            load_files(&paths).is_empty(),
            "missing files must be skipped, not loaded"
        );
    }

    #[test]
    fn an_unparseable_swaync_config_degrades_to_skipped() {
        // R4.4: malformed JSON is treated as unparseable — the loader returns an
        // error so the file is skipped, rather than panicking or loading garbage.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.json");
        std::fs::write(&path, b"{ not valid json ]").expect("write a broken config");

        assert!(
            load_swaync_config(&path).is_err(),
            "a malformed swaync config must surface as an error the loader skips"
        );

        // And through the orchestration: only the readable input.conf loads.
        std::fs::write(
            dir.path().join("input.conf"),
            b"input {\n    sensitivity = 0.0\n}\n",
        )
        .expect("write input.conf");
        let paths = BackingPaths {
            input_conf: dir.path().join("input.conf"),
            swaync_config: path,
        };
        let loaded = load_files(&paths);
        assert_eq!(loaded.len(), 1, "only the parseable file loads");
        assert_eq!(loaded[0].initial.values.len(), 1);
    }

    #[test]
    fn loaded_files_populate_the_store_with_originals_and_a_baseline() {
        // The end-to-end contract the window relies on: applying the LoadedFiles to a
        // store establishes real originals AND a freshness baseline, so a refresh
        // over the unchanged files reports no conflict.
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("input.conf"),
            b"input {\n    kb_layout = us\n    sensitivity = 0.5\n}\n",
        )
        .expect("write input.conf");
        std::fs::write(dir.path().join("config.json"), br#"{ "timeout": 10 }"#)
            .expect("write swaync config");
        let paths = BackingPaths {
            input_conf: dir.path().join("input.conf"),
            swaync_config: dir.path().join("config.json"),
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
            store.value(SettingId::NotificationTimeout),
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
    fn hypr_bool_parses_the_accepted_spellings() {
        for token in ["true", "yes", "on", "1", "TRUE", " True "] {
            assert_eq!(parse_hypr_bool(token), Some(true), "`{token}` is true");
        }
        for token in ["false", "no", "off", "0", "OFF"] {
            assert_eq!(parse_hypr_bool(token), Some(false), "`{token}` is false");
        }
        assert_eq!(
            parse_hypr_bool("maybe"),
            None,
            "an unknown token is skipped"
        );
    }
}
