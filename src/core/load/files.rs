//! The store-backed half of the startup load (task 5.4; R4.4, R5.6, R7.3): reading and
//! parsing each backing config file whose settings are staged in the shared
//! [`SettingsStore`], and folding a manual refresh's re-read files back into it.
//!
//! Each loader has the plain `fn(&Path) -> io::Result<FileValues>` shape of a
//! [`FileReader`], so the same function serves as the initial load and as the store's
//! re-reader for a later conflict reload: both always parse through one code path. See
//! the parent module for how this half fits into the whole load.

use std::io;
use std::path::{Path, PathBuf};

use super::{config_home, input_lua_path, notifd_config_path};
use crate::core::input::{input_values, read_input_lua};
use crate::core::lock_grace::parse_grace;
use crate::core::model::{SettingId, Value};
use crate::core::notifd_config::{NotifdConfigUnavailable, read_values};
use crate::core::power::power_key_path;
use crate::core::store::{FileReader, FileValues, RefreshReport, SettingsStore};
use crate::parsers::hyprlang::HyprlangFile;

/// One backing config file loaded at startup: the live XDG path it was read from,
/// the bytes + parsed originals, and a reader to re-parse it later.
///
/// The window feeds each of these straight into
/// [`SettingsStore::load_file`](crate::core::store::SettingsStore::load_file), which
/// records the originals and the freshness baseline (from `initial.bytes`) and keeps
/// `reader` for the conflict-reload path (R5.6).
pub struct LoadedFile {
    /// The live XDG path the file was read from (the key the store tracks it under).
    pub path: PathBuf,
    /// The bytes read and the originals parsed from them, in a single read so the
    /// freshness baseline matches exactly what was parsed (see the store docs).
    pub initial: FileValues,
    /// A closure that re-reads and re-parses this file for a later conflict reload.
    pub reader: FileReader,
}

/// The live XDG paths of the backing config files loaded at startup (R8.5).
///
/// Resolved from `$XDG_CONFIG_HOME`/`$HOME` rather than a hardcoded `~/.dotfiles`
/// path, so the load behaves identically for a symlink-deployed dotfiles setup and a
/// plain-file one — the writer follows symlinks later (R8.5). Kept as its own type so
/// the loading logic can be driven with injected paths in tests without touching the
/// real environment.
pub struct BackingPaths {
    /// `~/.config/hypr/input.lua` — the app-owned Lua input table.
    pub input_lua: PathBuf,
    /// `~/.config/hypr/hypridle.conf` — the idle timeouts + lock command (hyprlang).
    pub hypridle_conf: PathBuf,
    /// `~/.config/notifd/config` — notifd's seven settings (`key=value`).
    pub notifd_config: PathBuf,
}

impl BackingPaths {
    /// Resolves the backing paths from the XDG environment.
    ///
    /// Prefers `$XDG_CONFIG_HOME`, falling back to `$HOME/.config`; if neither is set
    /// the returned relative paths simply fail to read in [`load_files`] (the files
    /// are skipped), so this never has to fail.
    pub fn from_system() -> Self {
        let config_home = config_home();
        BackingPaths {
            input_lua: input_lua_path(&config_home),
            hypridle_conf: config_home.join("hypr").join("hypridle.conf"),
            notifd_config: notifd_config_path(&config_home),
        }
    }
}

/// Folds the backing files a manual-refresh [`load`](super::load) pass read into the store
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
pub fn merge_refreshed_files(store: &mut SettingsStore, files: Vec<LoadedFile>) -> RefreshReport {
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

/// Reads and parses each backing file at `paths`, returning one [`LoadedFile`] per
/// file that loaded (task 5.4).
///
/// Split out from [`load`](super::load) so the file-loading half is exercised with injected paths
/// in tests (the detection half is covered by `core::detect`'s own tests). Each file
/// is attempted independently: a failure to read or parse it is logged and the file
/// is skipped, never propagated, so one bad file cannot suppress the others (R4.4).
pub fn load_files(paths: &BackingPaths) -> Vec<LoadedFile> {
    let mut files = Vec::new();
    push_loaded(&mut files, &paths.input_lua, load_input_lua, "input.lua");
    push_loaded(
        &mut files,
        &paths.hypridle_conf,
        load_hypridle_conf,
        "hypridle.conf",
    );
    // notifd's file is read a second time here on purpose: `load_notifd_config_from`
    // already read it once through `NotifdConfigModel::offer` for the visibility gate.
    // Both go through the same `read_notifd_config`, so the rows are shown exactly when
    // the store tracks the file; keep both reads rather than removing one.
    push_loaded(
        &mut files,
        &paths.notifd_config,
        load_notifd_config,
        "notifd/config",
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
/// Public (like the `hypridle.conf` loader) so the integration suites, through
/// `crate::testing::loaders`, load the store through the app's real loader rather than
/// a test re-implementation (task 7.2).
pub fn load_input_lua(path: &Path) -> io::Result<FileValues> {
    let (bytes, file) = read_input_lua(path)?;
    let values = input_values(&file);
    Ok(FileValues { bytes, values })
}

/// Reads `~/.config/notifd/config` into the seven Notifications-page settings it backs
/// (task 11.15; analysis §8.2).
///
/// Every setting gets a value — the one notifd uses: its last valid assignment in the
/// file, else the daemon's default ([`read_values`]) — so a hand-trimmed file still
/// offers every control, and a changed missing key is appended on Apply. Assignments
/// notifd ignores are logged at `warn` with line number and key only, keys the file lacks
/// at `info` (R7.3). A file that cannot be read, is not UTF-8, or holds characters notifd
/// reads differently propagates as an error (the latter two as
/// [`io::ErrorKind::InvalidData`]), so the store does not track it — the same rule that
/// keeps the staged rows hidden for it.
///
/// Public for the same reason as [`load_input_lua`] (task 7.2).
pub fn load_notifd_config(path: &Path) -> io::Result<FileValues> {
    let (bytes, file) =
        crate::core::notifd_config::read_notifd_config(path).map_err(|error| match error {
            NotifdConfigUnavailable::Unreadable(error) => error,
            other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
        })?;
    let read = read_values(&file);
    for ignored in &read.ignored {
        tracing::warn!(
            file = "notifd/config",
            line = ignored.line,
            key = ignored.key,
            "notifd ignores this value (it fails the key's rule); the key keeps the \
             daemon's default or an earlier valid line"
        );
    }
    if !read.missing.is_empty() {
        tracing::info!(
            file = "notifd/config",
            missing = ?read.missing,
            "keys not set in notifd's settings file show notifd's defaults and are added \
             when changed"
        );
    }
    Ok(FileValues {
        bytes,
        values: read.values,
    })
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
/// Public for the same reason as [`load_input_lua`] (task 7.2).
pub fn load_hypridle_conf(path: &Path) -> io::Result<FileValues> {
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
    use crate::testing::input_lua_with_sensitivity;

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
            notifd_config: dir.path().join("no-notifd-config"),
        };
        assert!(
            load_files(&paths).is_empty(),
            "missing files must be skipped, not loaded"
        );
    }

    #[test]
    fn an_unusable_file_never_blocks_the_others() {
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
            notifd_config: dir.path().join("no-notifd-config"),
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
    fn loaded_files_fully_baseline_the_store() {
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
            notifd_config: dir.path().join("no-notifd-config"),
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
    fn a_refresh_merge_keeps_edits_while_files_change() {
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
            notifd_config: dir.path().join("no-notifd-config"),
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
            notifd_config: dir.path().join("no-notifd-config"),
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
    fn the_notifd_loader_seeds_every_setting() {
        // Task 11.15: a trimmed file still seeds all seven settings (missing ones at the
        // daemon default).
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config");
        std::fs::write(&path, "# trimmed\ncritical_per_app = 2\n").expect("write the file");
        let loaded = load_notifd_config(&path).expect("a readable file loads");
        assert_eq!(loaded.values.len(), 7);
        assert!(
            loaded
                .values
                .contains(&(SettingId::NotifdCriticalPerApp, Value::Integer(2)))
        );
        assert!(
            loaded
                .values
                .contains(&(SettingId::NotifdTimeoutLow, Value::Integer(5000)))
        );
    }

    #[test]
    fn the_notifd_loader_refuses_a_file_notifd_reads_differently() {
        // Task 11.15: a file with a character notifd splits differently than this app
        // would is not tracked at all.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config");
        std::fs::write(&path, "timeout_low_ms=1\x0c\n").expect("write the file");
        let error = load_notifd_config(&path).expect_err("refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("line 1"), "{error}");
    }
}
