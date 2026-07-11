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

use crate::core::detect::{Capabilities, DetectionInputs};
use crate::core::model::{SettingId, Value};
use crate::core::store::{FileReader, FileValues};
use crate::parsers::hyprlang::{HyprlangFile, KeyPath};
use crate::parsers::swaync::SwayncConfigFile;

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
    StartupLoad {
        capabilities,
        files,
    }
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
